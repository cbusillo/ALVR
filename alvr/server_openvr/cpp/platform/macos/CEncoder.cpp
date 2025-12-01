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
#include <sys/mman.h>
#include <sys/poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include "ALVR-common/packet_types.h"
#include "alvr_server/Logger.h"
#include "alvr_server/PoseHistory.h"
#include "alvr_server/Settings.h"
#include "protocol.h"

// VideoToolbox includes
#include <VideoToolbox/VideoToolbox.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>

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

    if (sampleBuffer == NULL) {
        return;
    }

    // Get the encoded data
    CMBlockBufferRef blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer);
    if (blockBuffer == NULL) {
        return;
    }

    size_t totalLength = 0;
    char* dataPointer = NULL;
    OSStatus blockStatus = CMBlockBufferGetDataPointer(
        blockBuffer, 0, NULL, &totalLength, &dataPointer
    );

    if (blockStatus != kCMBlockBufferNoErr || dataPointer == NULL) {
        return;
    }

    // Check if this is a keyframe
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    bool isKeyframe = false;
    if (attachments != NULL && CFArrayGetCount(attachments) > 0) {
        CFDictionaryRef attachment = (CFDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
        CFBooleanRef notSync = (CFBooleanRef)CFDictionaryGetValue(
            attachment, kCMSampleAttachmentKey_NotSync
        );
        isKeyframe = (notSync == NULL || CFBooleanGetValue(notSync) == false);
    }

    // Get presentation timestamp
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
    uint64_t ptsNs = (uint64_t)(CMTimeGetSeconds(pts) * 1e9);

    // TODO: Send NAL units to network
    // ParseFrameNals(codec, dataPointer, totalLength, ptsNs, isKeyframe);

    Info("Encoded frame: %zu bytes, keyframe: %d, pts: %llu", totalLength, isKeyframe, ptsNs);
}

} // namespace

void CEncoder::GetFds(int client, int (*received_fds)[6]) {
    struct msghdr msg;
    struct cmsghdr* cmsg;
    union {
        struct cmsghdr cm;
        u_int8_t pktinfo_sizer[sizeof(struct cmsghdr) + 1024];
    } control_un;
    struct iovec iov[1];
    char data[1];
    int ret;

    msg.msg_control = &control_un;
    msg.msg_controllen = sizeof(control_un);
    msg.msg_flags = 0;
    msg.msg_name = NULL;
    msg.msg_namelen = 0;
    iov[0].iov_base = data;
    iov[0].iov_len = 1;
    msg.msg_iov = iov;
    msg.msg_iovlen = 1;

    ret = recvmsg(client, &msg, 0);
    if (ret == -1) {
        throw std::runtime_error(std::string("recvmsg failed: ") + strerror(errno));
    }

    for (cmsg = CMSG_FIRSTHDR(&msg); cmsg != NULL; cmsg = CMSG_NXTHDR(&msg, cmsg)) {
        if (cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS) {
            memcpy(received_fds, CMSG_DATA(cmsg), sizeof(*received_fds));
            break;
        }
    }

    if (cmsg == NULL) {
        throw std::runtime_error("cmsg is NULL - no file descriptors received");
    }
}

void CEncoder::Run() {
    Info("CEncoder::Run (macOS VideoToolbox)\n");

    // Use /tmp for socket on macOS (XDG_RUNTIME_DIR not typically set)
    const char* runtime_dir = getenv("XDG_RUNTIME_DIR");
    if (runtime_dir) {
        m_socketPath = runtime_dir;
    } else {
        m_socketPath = "/tmp";
    }
    m_socketPath += "/alvr-ipc";

    int ret;
    ret = unlink(m_socketPath.c_str());

    m_socket.fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un name;
    if (m_socket.fd == -1) {
        perror("socket");
        return;
    }

    memset(&name, 0, sizeof(name));
    name.sun_family = AF_UNIX;
    strncpy(name.sun_path, m_socketPath.c_str(), sizeof(name.sun_path) - 1);

    ret = bind(m_socket.fd, (const struct sockaddr*)&name, sizeof(name));
    if (ret == -1) {
        perror("bind");
        return;
    }

    ret = listen(m_socket.fd, 1024);
    if (ret == -1) {
        perror("listen");
        return;
    }

    Info("CEncoder listening on %s\n", m_socketPath.c_str());

    struct pollfd client;
    client.fd = accept_timeout(m_socket, m_exiting);
    if (m_exiting)
        return;

    init_packet init;
    client.events = POLLIN;
    read_exactly(client, (char*)&init, sizeof(init), m_exiting);
    if (m_exiting)
        return;

    Info("CEncoder client connected, pid %d, images: %d\n",
         (int)init.source_pid, init.num_images);
    Info("Image size: %dx%d\n",
         init.image_create_info.extent.width,
         init.image_create_info.extent.height);

    try {
        GetFds(client.fd, &m_fds);
        m_connected = true;

        Info("Received %d file descriptors from client\n", 6);

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
            init.image_create_info.extent.width,
            init.image_create_info.extent.height,
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

        Info("VideoToolbox HEVC encoder initialized (%dx%d)\n",
             init.image_create_info.extent.width,
             init.image_create_info.extent.height);

        // Main encoding loop
        present_packet frame_info;
        uint64_t frameCount = 0;

        while (not m_exiting) {
            read_latest(client, (char*)&frame_info, sizeof(frame_info), m_exiting);
            if (m_exiting) break;

            auto pose = m_poseHistory->GetBestPoseMatch((const vr::HmdMatrix34_t&)frame_info.pose);
            if (!pose) {
                continue;
            }

            frameCount++;

            // TODO: Import GPU memory from file descriptors and encode
            // For now, just log that we received a frame
            if (frameCount % 90 == 0) {
                Info("Received frame %llu, image %d, semaphore %llu\n",
                     frameCount, frame_info.image, frame_info.semaphore_value);
            }

            // Check for IDR insertion request
            if (m_scheduler.CheckIDRInsertion()) {
                VTCompressionSessionCompleteFrames(compressionSession, kCMTimeInvalid);
                // Force next frame to be keyframe
                Info("IDR requested\n");
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
    unlink(m_socketPath.c_str());
}

void CEncoder::OnStreamStart() { m_scheduler.OnStreamStart(); }

void CEncoder::OnPacketLoss() { m_scheduler.OnPacketLoss(); }

void CEncoder::InsertIDR() { m_scheduler.InsertIDR(); }

void CEncoder::CaptureFrame() { m_captureFrame = true; }
