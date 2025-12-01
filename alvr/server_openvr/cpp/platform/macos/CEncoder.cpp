#include "CEncoder.h"

#include <chrono>
#include <exception>
#include <fstream>
#include <iostream>
#include <memory>
#include <poll.h>
#include <sstream>
#include <stdexcept>
#include <stdlib.h>
#include <string>
#include <vector>
#include <sys/mman.h>
#include <sys/poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <unistd.h>

#include "ALVR-common/packet_types.h"
#include "alvr_server/Logger.h"
#include "alvr_server/PoseHistory.h"
#include "alvr_server/Settings.h"
#include "alvr_server/bindings.h"

// Forward declaration for NAL parsing
void ParseFrameNals(int codec, unsigned char* buf, int len, unsigned long long targetTimestampNs, bool isIdr);

// VideoToolbox includes
#include <VideoToolbox/VideoToolbox.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>

// TCP port for Wine→macOS communication
#define ALVR_TCP_PORT 9944

// Protocol structures matching Windows VideoEncoderSocket
#pragma pack(push, 1)
struct socket_init_packet {
    uint32_t num_images;
    uint8_t device_uuid[16];
    uint32_t width;
    uint32_t height;
    uint32_t format;
    uint32_t mem_index;
    uint32_t source_pid;
};

struct socket_frame_packet {
    uint32_t image_index;
    uint32_t frame_number;
    uint64_t semaphore_value;
    float pose[3][4];
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint8_t is_idr;
    uint32_t data_size;
    // Followed by raw BGRA pixel data
};
#pragma pack(pop)

CEncoder::CEncoder(std::shared_ptr<PoseHistory> poseHistory)
    : m_poseHistory(poseHistory) { }

CEncoder::~CEncoder() { Stop(); }

namespace {

void read_exactly(pollfd pollfds, char* out, size_t size, std::atomic_bool& exiting) {
    while (not exiting and size != 0) {
        int timeout = 1;
        pollfds.events = POLLIN;
        int count = poll(&pollfds, 1, timeout);
        if (count < 0) {
            throw std::runtime_error(std::string("poll failed: ") + strerror(errno));
        } else if (count == 1) {
            int s = read(pollfds.fd, out, size);
            if (s == -1) {
                throw std::runtime_error(std::string("read failed: ") + strerror(errno));
            }
            out += s;
            size -= s;
        }
    }
}

void read_latest(pollfd pollfds, char* out, size_t size, std::atomic_bool& exiting) {
    read_exactly(pollfds, out, size, exiting);
    while (not exiting) {
        int timeout = 0;
        pollfds.events = POLLIN;
        int count = poll(&pollfds, 1, timeout);
        if (count == 0)
            return;
        read_exactly(pollfds, out, size, exiting);
    }
}

int accept_timeout(pollfd socket, std::atomic_bool& exiting) {
    while (not exiting) {
        int timeout = 15;
        socket.events = POLLIN;
        int count = poll(&socket, 1, timeout);
        if (count < 0) {
            throw std::runtime_error(std::string("poll failed: ") + strerror(errno));
        } else if (count == 1) {
            return accept(socket.fd, NULL, NULL);
        }
    }
    return -1;
}

// Frame context for passing timestamp through async callback
struct FrameContext {
    uint64_t targetTimestampNs;
    bool isIDR;
};

// NAL start code for Annex-B format
static const uint8_t NAL_START_CODE[] = { 0x00, 0x00, 0x00, 0x01 };

// Convert HVCC/AVCC format (length-prefixed) to Annex-B (start-code-prefixed)
// VideoToolbox outputs HVCC by default, but ALVR expects Annex-B
void convertHVCCToAnnexB(
    CMSampleBufferRef sampleBuffer,
    std::vector<uint8_t>& annexBData,
    bool& isKeyframe,
    uint64_t& ptsNs
) {
    annexBData.clear();
    isKeyframe = false;

    // Get format description
    CMFormatDescriptionRef formatDesc = CMSampleBufferGetFormatDescription(sampleBuffer);
    if (!formatDesc) return;

    // Check if this is a keyframe
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    if (attachments && CFArrayGetCount(attachments) > 0) {
        CFDictionaryRef attachment = (CFDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
        CFBooleanRef notSync = (CFBooleanRef)CFDictionaryGetValue(attachment, kCMSampleAttachmentKey_NotSync);
        isKeyframe = (notSync == NULL || !CFBooleanGetValue(notSync));
    }

    // Get timestamp
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
    ptsNs = (uint64_t)(CMTimeGetSeconds(pts) * 1e9);

    // For keyframes, prepend VPS/SPS/PPS from format description
    if (isKeyframe) {
        // Get HEVC parameter sets (VPS, SPS, PPS)
        size_t paramSetCount = 0;
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
            formatDesc, 0, NULL, NULL, &paramSetCount, NULL);

        for (size_t i = 0; i < paramSetCount; i++) {
            const uint8_t* paramSet = NULL;
            size_t paramSetSize = 0;
            OSStatus status = CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                formatDesc, i, &paramSet, &paramSetSize, NULL, NULL);
            if (status == noErr && paramSet && paramSetSize > 0) {
                // Add start code + parameter set
                annexBData.insert(annexBData.end(), NAL_START_CODE, NAL_START_CODE + 4);
                annexBData.insert(annexBData.end(), paramSet, paramSet + paramSetSize);
            }
        }
    }

    // Get encoded data
    CMBlockBufferRef blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer);
    if (!blockBuffer) return;

    size_t totalLength = 0;
    char* dataPointer = NULL;
    CMBlockBufferGetDataPointer(blockBuffer, 0, NULL, &totalLength, &dataPointer);
    if (!dataPointer) return;

    // Get NAL unit length size (usually 4 bytes for HEVC)
    int nalLengthSize = 4;
    CFNumberRef nalSizeField = (CFNumberRef)CMFormatDescriptionGetExtension(
        formatDesc, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms);
    // Default to 4 if not found

    // Convert each NAL unit from length-prefixed to start-code-prefixed
    size_t offset = 0;
    while (offset < totalLength) {
        if (offset + nalLengthSize > totalLength) break;

        // Read NAL unit length (big-endian)
        uint32_t nalLength = 0;
        for (int i = 0; i < nalLengthSize; i++) {
            nalLength = (nalLength << 8) | (uint8_t)dataPointer[offset + i];
        }
        offset += nalLengthSize;

        if (offset + nalLength > totalLength) break;

        // Add start code + NAL unit data
        annexBData.insert(annexBData.end(), NAL_START_CODE, NAL_START_CODE + 4);
        annexBData.insert(annexBData.end(),
            (uint8_t*)dataPointer + offset,
            (uint8_t*)dataPointer + offset + nalLength);
        offset += nalLength;
    }
}

// VideoToolbox encoder callback
void vtCompressionOutputCallback(
    void *outputCallbackRefCon,
    void *sourceFrameRefCon,
    OSStatus status,
    VTEncodeInfoFlags infoFlags,
    CMSampleBufferRef sampleBuffer
) {
    if (status != noErr) {
        Error("VideoToolbox encoding failed: %d", (int)status);
        return;
    }

    if (!sampleBuffer) return;

    // Get frame context
    FrameContext* ctx = (FrameContext*)sourceFrameRefCon;
    uint64_t targetTimestampNs = ctx ? ctx->targetTimestampNs : 0;
    bool requestedIDR = ctx ? ctx->isIDR : false;
    delete ctx;

    // Convert HVCC to Annex-B format
    static std::vector<uint8_t> annexBData;
    bool isKeyframe = false;
    uint64_t ptsNs = 0;
    convertHVCCToAnnexB(sampleBuffer, annexBData, isKeyframe, ptsNs);

    if (annexBData.empty()) return;

    // Use the target timestamp from frame context for network timing
    // (ptsNs is the encode PTS, targetTimestampNs is for ALVR timing)
    if (targetTimestampNs == 0) {
        targetTimestampNs = ptsNs;
    }

    // Send to ALVR network layer
    ParseFrameNals(
        ALVR_CODEC_HEVC,
        annexBData.data(),
        (int)annexBData.size(),
        targetTimestampNs,
        isKeyframe
    );

    // Log periodically
    static uint64_t frameCount = 0;
    frameCount++;
    if (frameCount % 90 == 0 || isKeyframe) {
        Info("Sent frame %llu: %zu bytes%s", frameCount, annexBData.size(),
             isKeyframe ? " [KEYFRAME]" : "");
    }
}

} // namespace

void CEncoder::Run() {
    Info("CEncoder::Run (macOS VideoToolbox via TCP)\n");

    // Create TCP socket
    m_socket.fd = socket(AF_INET, SOCK_STREAM, 0);
    if (m_socket.fd == -1) {
        perror("socket");
        return;
    }

    // Allow port reuse
    int opt = 1;
    setsockopt(m_socket.fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port = htons(ALVR_TCP_PORT);

    if (bind(m_socket.fd, (const struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("bind");
        close(m_socket.fd);
        return;
    }

    if (listen(m_socket.fd, 1) == -1) {
        perror("listen");
        close(m_socket.fd);
        return;
    }

    Info("CEncoder listening on TCP port %d\n", ALVR_TCP_PORT);

    struct pollfd client;
    client.fd = accept_timeout(m_socket, m_exiting);
    if (m_exiting)
        return;

    // Disable Nagle's algorithm for lower latency
    int flag = 1;
    setsockopt(client.fd, IPPROTO_TCP, TCP_NODELAY, &flag, sizeof(flag));

    // Read init packet (TCP protocol - raw pixel transfer)
    socket_init_packet init;
    client.events = POLLIN;
    read_exactly(client, (char*)&init, sizeof(init), m_exiting);
    if (m_exiting)
        return;

    Info("CEncoder client connected, pid %d\n", (int)init.source_pid);
    Info("Image size: %dx%d, format: 0x%x\n", init.width, init.height, init.format);

    m_connected = true;

    // Pre-allocate pixel buffer for receiving frame data
    std::vector<uint8_t> pixelData;
    pixelData.resize(init.width * init.height * 4);  // BGRA

    try {
        // Create VideoToolbox encoder
        VTCompressionSessionRef compressionSession = NULL;

        CFMutableDictionaryRef encoderSpec = CFDictionaryCreateMutable(
            kCFAllocatorDefault, 0,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks
        );

        // Request hardware encoding
        CFDictionarySetValue(
            encoderSpec,
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
            kCFBooleanTrue
        );

        OSStatus status = VTCompressionSessionCreate(
            kCFAllocatorDefault,
            init.width,
            init.height,
            kCMVideoCodecType_HEVC,  // Use HEVC for better compression
            encoderSpec,
            NULL,  // sourceImageBufferAttributes
            kCFAllocatorDefault,
            vtCompressionOutputCallback,
            NULL,  // outputCallbackRefCon
            &compressionSession
        );

        CFRelease(encoderSpec);

        if (status != noErr) {
            Error("Failed to create VideoToolbox compression session: %d", (int)status);
            throw std::runtime_error("Failed to create VideoToolbox session");
        }

        // Configure encoder for low-latency VR streaming
        VTSessionSetProperty(compressionSession,
            kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
        VTSessionSetProperty(compressionSession,
            kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);

        // Set bitrate (10 Mbps default, adjustable)
        int32_t bitrate = 10000000;
        CFNumberRef bitrateRef = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &bitrate);
        VTSessionSetProperty(compressionSession,
            kVTCompressionPropertyKey_AverageBitRate, bitrateRef);
        CFRelease(bitrateRef);

        // Set keyframe interval (every 2 seconds at 90fps = 180 frames)
        int32_t keyframeInterval = 180;
        CFNumberRef keyframeRef = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &keyframeInterval);
        VTSessionSetProperty(compressionSession,
            kVTCompressionPropertyKey_MaxKeyFrameInterval, keyframeRef);
        CFRelease(keyframeRef);

        VTCompressionSessionPrepareToEncodeFrames(compressionSession);

        Info("VideoToolbox HEVC encoder initialized (%dx%d)\n", init.width, init.height);

        // Main encoding loop - receive frames via TCP
        socket_frame_packet frame_info;
        uint64_t frameCount = 0;
        CMTime frameDuration = CMTimeMake(1, 90);  // 90fps

        while (not m_exiting) {
            // Read frame header
            read_exactly(client, (char*)&frame_info, sizeof(frame_info), m_exiting);
            if (m_exiting) break;

            // Validate and resize buffer if needed
            if (frame_info.data_size > pixelData.size()) {
                pixelData.resize(frame_info.data_size);
            }

            // Read pixel data
            read_exactly(client, (char*)pixelData.data(), frame_info.data_size, m_exiting);
            if (m_exiting) break;

            // Convert pose to HmdMatrix34_t and look up in history
            vr::HmdMatrix34_t pose;
            memcpy(&pose, frame_info.pose, sizeof(pose));
            auto poseMatch = m_poseHistory->GetBestPoseMatch(pose);
            if (!poseMatch) {
                // Still process frames even without pose match
            }

            frameCount++;

            // Create CVPixelBuffer from received pixel data
            CVPixelBufferRef cvPixelBuffer = NULL;

            // Create pixel buffer attributes dictionary
            CFMutableDictionaryRef pixelBufferAttrs = CFDictionaryCreateMutable(
                kCFAllocatorDefault, 4,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks
            );

            int32_t pixelFormat = kCVPixelFormatType_32BGRA;
            CFNumberRef pixelFormatNum = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &pixelFormat);
            CFDictionarySetValue(pixelBufferAttrs, kCVPixelBufferPixelFormatTypeKey, pixelFormatNum);
            CFRelease(pixelFormatNum);

            int32_t widthInt = frame_info.width;
            CFNumberRef widthNum = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &widthInt);
            CFDictionarySetValue(pixelBufferAttrs, kCVPixelBufferWidthKey, widthNum);
            CFRelease(widthNum);

            int32_t heightInt = frame_info.height;
            CFNumberRef heightNum = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &heightInt);
            CFDictionarySetValue(pixelBufferAttrs, kCVPixelBufferHeightKey, heightNum);
            CFRelease(heightNum);

            CFDictionaryRef emptyDict = CFDictionaryCreate(kCFAllocatorDefault, NULL, NULL, 0,
                &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
            CFDictionarySetValue(pixelBufferAttrs, kCVPixelBufferIOSurfacePropertiesKey, emptyDict);
            CFRelease(emptyDict);

            CVReturn cvRet = CVPixelBufferCreate(
                kCFAllocatorDefault,
                frame_info.width,
                frame_info.height,
                kCVPixelFormatType_32BGRA,
                pixelBufferAttrs,
                &cvPixelBuffer
            );
            CFRelease(pixelBufferAttrs);

            if (cvRet != kCVReturnSuccess) {
                Error("Failed to create CVPixelBuffer: %d", cvRet);
                continue;
            }

            // Copy pixel data to CVPixelBuffer
            CVPixelBufferLockBaseAddress(cvPixelBuffer, 0);
            void* baseAddress = CVPixelBufferGetBaseAddress(cvPixelBuffer);
            size_t bytesPerRow = CVPixelBufferGetBytesPerRow(cvPixelBuffer);
            size_t srcBytesPerRow = frame_info.stride;

            if (bytesPerRow == srcBytesPerRow) {
                memcpy(baseAddress, pixelData.data(), frame_info.data_size);
            } else {
                // Handle row padding
                uint8_t* dst = (uint8_t*)baseAddress;
                uint8_t* src = pixelData.data();
                for (uint32_t y = 0; y < frame_info.height; y++) {
                    memcpy(dst, src, srcBytesPerRow);
                    dst += bytesPerRow;
                    src += srcBytesPerRow;
                }
            }
            CVPixelBufferUnlockBaseAddress(cvPixelBuffer, 0);

            // Create presentation timestamp
            CMTime pts = CMTimeMake(frameCount, 90);

            // Check for IDR insertion request (from frame or scheduler)
            bool forceIDR = frame_info.is_idr || m_scheduler.CheckIDRInsertion();
            CFDictionaryRef frameProps = NULL;
            if (forceIDR) {
                CFMutableDictionaryRef props = CFDictionaryCreateMutable(
                    kCFAllocatorDefault, 1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks
                );
                CFDictionarySetValue(props, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
                frameProps = props;
                if (frameCount > 1) {  // Don't log for first frame
                    Info("Forcing IDR frame\n");
                }
            }

            // Create frame context for callback
            // Use semaphore_value as approximate target timestamp (in ns)
            // This will be used for ALVR timing
            FrameContext* frameCtx = new FrameContext();
            frameCtx->targetTimestampNs = frame_info.semaphore_value * 1000000;  // Assume ms → ns
            frameCtx->isIDR = forceIDR;

            // Encode the frame
            OSStatus encodeStatus = VTCompressionSessionEncodeFrame(
                compressionSession,
                cvPixelBuffer,
                pts,
                frameDuration,
                frameProps,
                frameCtx,  // sourceFrameRefCon - passed to callback
                NULL       // infoFlagsOut
            );

            if (frameProps) {
                CFRelease(frameProps);
            }
            CVPixelBufferRelease(cvPixelBuffer);

            if (encodeStatus != noErr) {
                Error("VTCompressionSessionEncodeFrame failed: %d", (int)encodeStatus);
                delete frameCtx;  // Clean up on error
            }

            // Log progress periodically
            if (frameCount % 90 == 0) {
                Info("Received frame %llu (%dx%d, %u bytes)\n",
                     frameCount, frame_info.width, frame_info.height, frame_info.data_size);
            }
        }

        // Cleanup
        VTCompressionSessionInvalidate(compressionSession);
        CFRelease(compressionSession);

    } catch (std::exception& e) {
        Error("Error in encoder thread: %s", e.what());
    }

    client.events = POLLHUP;
    close(client.fd);
}

void CEncoder::Stop() {
    m_exiting = true;
    m_socket.events = POLLHUP;
    close(m_socket.fd);
}

void CEncoder::OnStreamStart() { m_scheduler.OnStreamStart(); }

void CEncoder::OnPacketLoss() { m_scheduler.OnPacketLoss(); }

void CEncoder::InsertIDR() { m_scheduler.InsertIDR(); }

void CEncoder::CaptureFrame() { m_captureFrame = true; }
