#pragma once

#include "VideoEncoder.h"
#include "shared/d3drender.h"
#include <memory>
#include <string>
#include <vector>

// When running under Wine, we can use Unix sockets via Wine's POSIX layer
// For native Windows (not supported), this encoder will fail to connect

// Protocol structures matching macOS server
// Must match alvr/server_openvr/cpp/platform/macos/protocol.h
#pragma pack(push, 1)
struct socket_init_packet {
    uint32_t num_images;
    uint8_t device_uuid[16];  // VK_UUID_SIZE
    // VkImageCreateInfo equivalent fields
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
    float pose[3][4];  // 3x4 matrix
    // For raw pixel transfer (not using GPU memory sharing)
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint8_t is_idr;
    uint32_t data_size;
    // Followed by raw BGRA pixel data
};
#pragma pack(pop)

class VideoEncoderSocket : public VideoEncoder {
public:
    VideoEncoderSocket(
        std::shared_ptr<CD3DRender> d3dRender,
        uint32_t width,
        uint32_t height
    );
    ~VideoEncoderSocket();

    void Initialize() override;
    void Shutdown() override;

    void Transmit(
        ID3D11Texture2D* pTexture,
        uint64_t presentationTime,
        uint64_t targetTimestampNs,
        bool insertIDR
    ) override;

private:
    bool Connect();
    void Disconnect();
    bool SendData(const void* data, size_t size);

    std::shared_ptr<CD3DRender> m_d3dRender;
    uint32_t m_width;
    uint32_t m_height;

    // Socket (Unix socket via Wine, or -1 if not connected)
    int m_socket;
    std::string m_socketPath;
    bool m_connected;
    bool m_initSent;

    // Staging texture for CPU readback
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_stagingTexture;
    std::vector<uint8_t> m_pixelBuffer;

    uint32_t m_frameIndex;
};
