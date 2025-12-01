// Test encoder server - mimics macOS CEncoder receiving frames via TCP
// Compile: clang++ -std=c++17 -o test_encoder_server test_encoder_server.cpp \
//          -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework CoreFoundation

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <vector>

#include <VideoToolbox/VideoToolbox.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>

#define ALVR_TCP_PORT 9944

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
};
#pragma pack(pop)

// VideoToolbox callback
static uint64_t g_totalEncodedBytes = 0;
static uint64_t g_encodedFrames = 0;

void encoderCallback(
    void *outputCallbackRefCon,
    void *sourceFrameRefCon,
    OSStatus status,
    VTEncodeInfoFlags infoFlags,
    CMSampleBufferRef sampleBuffer
) {
    if (status != noErr) {
        printf("Encoding failed: %d\n", (int)status);
        return;
    }
    if (!sampleBuffer) return;

    CMBlockBufferRef blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer);
    if (!blockBuffer) return;

    size_t totalLength = 0;
    char* dataPointer = NULL;
    CMBlockBufferGetDataPointer(blockBuffer, 0, NULL, &totalLength, &dataPointer);

    // Check if keyframe
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    bool isKeyframe = false;
    if (attachments && CFArrayGetCount(attachments) > 0) {
        CFDictionaryRef attachment = (CFDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
        CFBooleanRef notSync = (CFBooleanRef)CFDictionaryGetValue(attachment, kCMSampleAttachmentKey_NotSync);
        isKeyframe = (notSync == NULL || CFBooleanGetValue(notSync) == false);
    }

    g_totalEncodedBytes += totalLength;
    g_encodedFrames++;

    if (g_encodedFrames % 10 == 0 || isKeyframe) {
        printf("Encoded frame %llu: %zu bytes%s (total: %llu KB)\n",
               g_encodedFrames, totalLength,
               isKeyframe ? " [KEYFRAME]" : "",
               g_totalEncodedBytes / 1024);
    }
}

ssize_t read_fully(int fd, void* buf, size_t size) {
    char* ptr = (char*)buf;
    size_t remaining = size;
    while (remaining > 0) {
        ssize_t n = read(fd, ptr, remaining);
        if (n <= 0) return n;
        ptr += n;
        remaining -= n;
    }
    return size;
}

int main() {
    printf("ALVR Encoder Test Server\n");
    printf("========================\n\n");

    // Create TCP socket
    int server_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (server_fd < 0) {
        perror("socket");
        return 1;
    }

    int opt = 1;
    setsockopt(server_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr = {};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port = htons(ALVR_TCP_PORT);

    if (bind(server_fd, (struct sockaddr*)&addr, sizeof(addr)) < 0) {
        perror("bind");
        close(server_fd);
        return 1;
    }

    if (listen(server_fd, 1) < 0) {
        perror("listen");
        close(server_fd);
        return 1;
    }

    printf("Listening on port %d...\n", ALVR_TCP_PORT);
    printf("Run the Wine test: wine test_frame_sender.exe\n\n");

    struct sockaddr_in client_addr;
    socklen_t client_len = sizeof(client_addr);
    int client_fd = accept(server_fd, (struct sockaddr*)&client_addr, &client_len);
    if (client_fd < 0) {
        perror("accept");
        close(server_fd);
        return 1;
    }

    // Disable Nagle
    int flag = 1;
    setsockopt(client_fd, IPPROTO_TCP, TCP_NODELAY, &flag, sizeof(flag));

    printf("Client connected from %s:%d\n",
           inet_ntoa(client_addr.sin_addr), ntohs(client_addr.sin_port));

    // Read init packet
    socket_init_packet init;
    if (read_fully(client_fd, &init, sizeof(init)) != sizeof(init)) {
        printf("Failed to read init packet\n");
        close(client_fd);
        close(server_fd);
        return 1;
    }

    printf("Init: %dx%d, format=0x%x, pid=%d\n\n",
           init.width, init.height, init.format, init.source_pid);

    // Create VideoToolbox encoder
    VTCompressionSessionRef session = NULL;

    CFMutableDictionaryRef encoderSpec = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 0,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks
    );
    CFDictionarySetValue(encoderSpec,
        kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
        kCFBooleanTrue);

    OSStatus status = VTCompressionSessionCreate(
        kCFAllocatorDefault,
        init.width, init.height,
        kCMVideoCodecType_HEVC,
        encoderSpec,
        NULL, kCFAllocatorDefault,
        encoderCallback, NULL,
        &session
    );
    CFRelease(encoderSpec);

    if (status != noErr) {
        printf("Failed to create encoder: %d\n", (int)status);
        close(client_fd);
        close(server_fd);
        return 1;
    }

    // Configure encoder
    VTSessionSetProperty(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    VTSessionSetProperty(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);

    int32_t bitrate = 10000000;
    CFNumberRef bitrateRef = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &bitrate);
    VTSessionSetProperty(session, kVTCompressionPropertyKey_AverageBitRate, bitrateRef);
    CFRelease(bitrateRef);

    VTCompressionSessionPrepareToEncodeFrames(session);
    printf("VideoToolbox HEVC encoder ready\n\n");

    // Pre-allocate buffer
    std::vector<uint8_t> pixelData;
    pixelData.resize(init.width * init.height * 4);

    CMTime frameDuration = CMTimeMake(1, 90);
    uint64_t frameCount = 0;

    // Receive and encode frames
    while (true) {
        socket_frame_packet frame;
        ssize_t n = read_fully(client_fd, &frame, sizeof(frame));
        if (n <= 0) {
            printf("\nConnection closed\n");
            break;
        }

        // Resize if needed
        if (frame.data_size > pixelData.size()) {
            pixelData.resize(frame.data_size);
        }

        // Read pixel data
        n = read_fully(client_fd, pixelData.data(), frame.data_size);
        if (n <= 0) {
            printf("\nFailed to read pixel data\n");
            break;
        }

        frameCount++;

        // Create CVPixelBuffer
        CVPixelBufferRef pixelBuffer = NULL;
        CVReturn cvRet = CVPixelBufferCreate(
            kCFAllocatorDefault,
            frame.width, frame.height,
            kCVPixelFormatType_32BGRA,
            NULL, &pixelBuffer
        );

        if (cvRet != kCVReturnSuccess) {
            printf("Failed to create pixel buffer: %d\n", cvRet);
            continue;
        }

        // Copy data
        CVPixelBufferLockBaseAddress(pixelBuffer, 0);
        void* baseAddr = CVPixelBufferGetBaseAddress(pixelBuffer);
        size_t bytesPerRow = CVPixelBufferGetBytesPerRow(pixelBuffer);

        if (bytesPerRow == frame.stride) {
            memcpy(baseAddr, pixelData.data(), frame.data_size);
        } else {
            uint8_t* dst = (uint8_t*)baseAddr;
            uint8_t* src = pixelData.data();
            for (uint32_t y = 0; y < frame.height; y++) {
                memcpy(dst, src, frame.stride);
                dst += bytesPerRow;
                src += frame.stride;
            }
        }
        CVPixelBufferUnlockBaseAddress(pixelBuffer, 0);

        // Encode
        CMTime pts = CMTimeMake(frameCount, 90);
        CFDictionaryRef frameProps = NULL;

        if (frame.is_idr) {
            CFMutableDictionaryRef props = CFDictionaryCreateMutable(
                kCFAllocatorDefault, 1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks
            );
            CFDictionarySetValue(props, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
            frameProps = props;
        }

        status = VTCompressionSessionEncodeFrame(
            session, pixelBuffer, pts, frameDuration,
            frameProps, NULL, NULL
        );

        if (frameProps) CFRelease(frameProps);
        CVPixelBufferRelease(pixelBuffer);

        if (status != noErr) {
            printf("Encode failed: %d\n", (int)status);
        }
    }

    // Cleanup
    VTCompressionSessionInvalidate(session);
    CFRelease(session);
    close(client_fd);
    close(server_fd);

    printf("\n=== SUMMARY ===\n");
    printf("Received frames: %llu\n", frameCount);
    printf("Encoded frames:  %llu\n", g_encodedFrames);
    printf("Total encoded:   %llu KB\n", g_totalEncodedBytes / 1024);
    printf("Avg per frame:   %llu bytes\n",
           g_encodedFrames > 0 ? g_totalEncodedBytes / g_encodedFrames : 0);

    return 0;
}
