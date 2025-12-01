#include "VideoEncoderSocket.h"
#include "alvr_server/Logger.h"

// Use TCP sockets - works in Wine and native Windows
#include <winsock2.h>
#include <ws2tcpip.h>

#pragma comment(lib, "ws2_32.lib")

// TCP port for ALVR macOS server
#define ALVR_TCP_PORT 9944

VideoEncoderSocket::VideoEncoderSocket(
    std::shared_ptr<CD3DRender> d3dRender,
    uint32_t width,
    uint32_t height
)
    : m_d3dRender(d3dRender)
    , m_width(width)
    , m_height(height)
    , m_socket(INVALID_SOCKET)
    , m_socketPath("127.0.0.1")  // localhost
    , m_connected(false)
    , m_initSent(false)
    , m_frameIndex(0) {

    // Pre-allocate pixel buffer for BGRA data
    m_pixelBuffer.resize(m_width * m_height * 4);
}

VideoEncoderSocket::~VideoEncoderSocket() {
    Shutdown();
}

void VideoEncoderSocket::Initialize() {
    Info("VideoEncoderSocket: Initializing for %dx%d\n", m_width, m_height);

    // Initialize Winsock
    WSADATA wsaData;
    int result = WSAStartup(MAKEWORD(2, 2), &wsaData);
    if (result != 0) {
        throw MakeException("WSAStartup failed: %d", result);
    }

    // Create staging texture for CPU readback
    D3D11_TEXTURE2D_DESC stagingDesc = {};
    stagingDesc.Width = m_width;
    stagingDesc.Height = m_height;
    stagingDesc.MipLevels = 1;
    stagingDesc.ArraySize = 1;
    stagingDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    stagingDesc.SampleDesc.Count = 1;
    stagingDesc.Usage = D3D11_USAGE_STAGING;
    stagingDesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

    HRESULT hr = m_d3dRender->GetDevice()->CreateTexture2D(
        &stagingDesc, nullptr, &m_stagingTexture
    );
    if (FAILED(hr)) {
        throw MakeException("Failed to create staging texture: 0x%x", hr);
    }

    // Try to connect to macOS server
    if (!Connect()) {
        Info("VideoEncoderSocket: Not connected yet, will retry on first frame\n");
    }
}

void VideoEncoderSocket::Shutdown() {
    Disconnect();
    WSACleanup();
    m_stagingTexture.Reset();
}

bool VideoEncoderSocket::Connect() {
    if (m_connected) return true;

    // Create TCP socket
    m_socket = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (m_socket == INVALID_SOCKET) {
        int err = WSAGetLastError();
        Error("VideoEncoderSocket: socket() failed: %d\n", err);
        return false;
    }

    // Disable Nagle's algorithm for lower latency
    int flag = 1;
    setsockopt(m_socket, IPPROTO_TCP, TCP_NODELAY, (char*)&flag, sizeof(flag));

    // Connect to macOS server on localhost
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(ALVR_TCP_PORT);
    addr.sin_addr.s_addr = inet_addr("127.0.0.1");

    if (connect(m_socket, (struct sockaddr*)&addr, sizeof(addr)) == SOCKET_ERROR) {
        int err = WSAGetLastError();
        // Don't spam logs - connection refused is expected when server not running
        if (err != WSAECONNREFUSED) {
            Error("VideoEncoderSocket: connect() failed: %d\n", err);
        }
        closesocket(m_socket);
        m_socket = INVALID_SOCKET;
        return false;
    }

    m_connected = true;
    Info("VideoEncoderSocket: Connected to 127.0.0.1:%d\n", ALVR_TCP_PORT);

    // Send init packet
    socket_init_packet init = {};
    init.num_images = 3;
    init.width = m_width;
    init.height = m_height;
    init.format = DXGI_FORMAT_B8G8R8A8_UNORM;
    init.source_pid = GetCurrentProcessId();

    if (!SendData(&init, sizeof(init))) {
        Error("VideoEncoderSocket: Failed to send init packet\n");
        Disconnect();
        return false;
    }

    m_initSent = true;
    Info("VideoEncoderSocket: Init packet sent (%dx%d)\n", m_width, m_height);
    return true;
}

void VideoEncoderSocket::Disconnect() {
    if (m_socket != INVALID_SOCKET) {
        closesocket(m_socket);
        m_socket = INVALID_SOCKET;
    }
    m_connected = false;
    m_initSent = false;
}

bool VideoEncoderSocket::SendData(const void* data, size_t size) {
    if (m_socket == INVALID_SOCKET) return false;

    const char* ptr = (const char*)data;
    size_t remaining = size;

    while (remaining > 0) {
        int sent = send(m_socket, ptr, (int)remaining, 0);
        if (sent == SOCKET_ERROR) {
            int err = WSAGetLastError();
            Error("VideoEncoderSocket: send() failed: %d\n", err);
            return false;
        }
        ptr += sent;
        remaining -= sent;
    }
    return true;
}

void VideoEncoderSocket::Transmit(
    ID3D11Texture2D* pTexture,
    uint64_t presentationTime,
    uint64_t targetTimestampNs,
    bool insertIDR
) {
    // Try to connect if not connected
    if (!m_connected && !Connect()) {
        // Silently drop frames if not connected
        return;
    }

    // Copy texture to staging for CPU access
    m_d3dRender->GetContext()->CopyResource(m_stagingTexture.Get(), pTexture);

    // Map staging texture
    D3D11_MAPPED_SUBRESOURCE mapped;
    HRESULT hr = m_d3dRender->GetContext()->Map(
        m_stagingTexture.Get(), 0, D3D11_MAP_READ, 0, &mapped
    );
    if (FAILED(hr)) {
        Error("VideoEncoderSocket: Failed to map staging texture: 0x%x\n", hr);
        return;
    }

    // Copy to contiguous buffer (handle row pitch)
    uint32_t srcPitch = mapped.RowPitch;
    uint32_t dstPitch = m_width * 4;

    if (srcPitch == dstPitch) {
        memcpy(m_pixelBuffer.data(), mapped.pData, m_height * dstPitch);
    } else {
        uint8_t* src = (uint8_t*)mapped.pData;
        uint8_t* dst = m_pixelBuffer.data();
        for (uint32_t y = 0; y < m_height; y++) {
            memcpy(dst, src, dstPitch);
            src += srcPitch;
            dst += dstPitch;
        }
    }

    m_d3dRender->GetContext()->Unmap(m_stagingTexture.Get(), 0);

    // Build frame packet
    socket_frame_packet frame = {};
    frame.image_index = m_frameIndex % 3;
    frame.frame_number = m_frameIndex;
    frame.semaphore_value = m_frameIndex;
    // pose left as identity/zeros for now
    frame.width = m_width;
    frame.height = m_height;
    frame.stride = dstPitch;
    frame.is_idr = insertIDR ? 1 : 0;
    frame.data_size = (uint32_t)m_pixelBuffer.size();

    // Send header
    if (!SendData(&frame, sizeof(frame))) {
        Error("VideoEncoderSocket: Failed to send frame header\n");
        Disconnect();
        return;
    }

    // Send pixel data
    if (!SendData(m_pixelBuffer.data(), m_pixelBuffer.size())) {
        Error("VideoEncoderSocket: Failed to send pixel data\n");
        Disconnect();
        return;
    }

    m_frameIndex++;

    // Log progress periodically
    if (m_frameIndex % 90 == 0) {
        Info("VideoEncoderSocket: Sent frame %u (%zu bytes)\n",
             m_frameIndex, sizeof(frame) + m_pixelBuffer.size());
    }
}
