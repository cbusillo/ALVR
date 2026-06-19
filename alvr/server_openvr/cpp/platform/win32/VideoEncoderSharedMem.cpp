#include "VideoEncoderSharedMem.h"
#include "alvr_server/Logger.h"
#include "alvr_server/Utils.h"

#include <chrono>
#include <cstring>
#include <intrin.h>
#include <string>
#include <thread>

namespace {
constexpr int kBridgeOpenTimeoutMs = 5000;
constexpr int kBridgeOpenRetryMs = 100;
constexpr uint64_t kMaxBridgeHeartbeatAgeNs = 5'000'000'000ULL;
constexpr uint64_t kBridgeHeartbeatFutureToleranceNs = 250'000'000ULL;
constexpr DXGI_FORMAT kUnsupportedFormat = DXGI_FORMAT_UNKNOWN;

uint32_t
ElapsedUs(std::chrono::steady_clock::time_point start, std::chrono::steady_clock::time_point end) {
    uint64_t micros
        = (uint64_t)std::chrono::duration_cast<std::chrono::microseconds>(end - start).count();
    return micros > UINT32_MAX ? UINT32_MAX : (uint32_t)micros;
}

uint64_t UnixTimeNs() {
    FILETIME fileTime = { };
    GetSystemTimeAsFileTime(&fileTime);
    ULARGE_INTEGER value = { };
    value.LowPart = fileTime.dwLowDateTime;
    value.HighPart = fileTime.dwHighDateTime;
    constexpr uint64_t kUnixEpochAsFiletime = 116444736000000000ULL;
    if (value.QuadPart < kUnixEpochAsFiletime) {
        return 0;
    }
    return (value.QuadPart - kUnixEpochAsFiletime) * 100ULL;
}

std::string WineSharedMemoryPath() {
    std::string path = "Z:" ALVR_SHM_PATH;
    for (char& ch : path) {
        if (ch == '/') {
            ch = '\\';
        }
    }
    return path;
}

bool IsRgbaFormat(DXGI_FORMAT format) {
    return format == DXGI_FORMAT_R8G8B8A8_UNORM || format == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
}

bool IsBgraFormat(DXGI_FORMAT format) {
    return format == DXGI_FORMAT_B8G8R8A8_UNORM || format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
}

bool IsSupportedSharedMemoryFormat(DXGI_FORMAT format) {
    return IsRgbaFormat(format) || IsBgraFormat(format);
}

bool FitsSharedMemoryFrame(int width, int height) {
    if (width <= 0 || height <= 0 || width > ALVR_MAX_WIDTH || height > ALVR_MAX_HEIGHT) {
        return false;
    }

    uint64_t size = (uint64_t)width * (uint64_t)height * ALVR_BYTES_PER_PIXEL;
    return size <= ALVR_MAX_FRAME_SIZE;
}

bool BridgeMappingLive(AlvrSharedMemory* shm) {
    if (!shm) {
        return false;
    }

    bool headerReady = shm->magic == ALVR_SHM_MAGIC && shm->version == ALVR_SHM_VERSION;
    bool bridgeReady = _InterlockedOr((volatile long*)&shm->initialized, 0) != 0;
    bool bridgeShutdown = _InterlockedOr((volatile long*)&shm->shutdown, 0) != 0;
    uint64_t sessionId = shm->bridge_session_id;
    uint64_t heartbeatNs = shm->bridge_heartbeat_ns;
    uint64_t now = UnixTimeNs();
    bool heartbeatReady = sessionId != 0 && heartbeatNs != 0
        && ((heartbeatNs <= now && now - heartbeatNs <= kMaxBridgeHeartbeatAgeNs)
            || (heartbeatNs > now && heartbeatNs - now <= kBridgeHeartbeatFutureToleranceNs));
    return headerReady && bridgeReady && !bridgeShutdown && heartbeatReady;
}
}

VideoEncoderSharedMem::VideoEncoderSharedMem(
    std::shared_ptr<CD3DRender> d3dRender, int width, int height
)
    : m_d3dRender(d3dRender)
    , m_width(width)
    , m_height(height)
    , m_fileHandle(INVALID_HANDLE_VALUE)
    , m_mappingHandle(NULL)
    , m_shm(nullptr)
    , m_frameData(nullptr)
    , m_initialized(false)
    , m_frameIndex(0)
    , m_stagingTextureDesc { } { }

VideoEncoderSharedMem::~VideoEncoderSharedMem() { Shutdown(); }

void VideoEncoderSharedMem::Initialize() {
    Info("VideoEncoderSharedMem: Initializing for %dx%d\n", m_width, m_height);

    if (!FitsSharedMemoryFrame(m_width, m_height)) {
        throw MakeException(
            "VideoEncoderSharedMem: frame size %dx%d exceeds shared-memory ABI limit %dx%d",
            m_width,
            m_height,
            ALVR_MAX_WIDTH,
            ALVR_MAX_HEIGHT
        );
    }

    if (!MapSharedMemory()) {
        throw MakeException("VideoEncoderSharedMem: failed to map /tmp shared memory");
    }

    if (!WaitForBridgeReady(5000)) {
        UnmapSharedMemory();
        throw MakeException("VideoEncoderSharedMem: timeout waiting for macOS bridge");
    }

    m_shm->config_width = m_width;
    m_shm->config_height = m_height;
    m_shm->config_format = DXGI_FORMAT_B8G8R8A8_UNORM;
    _InterlockedExchange((volatile long*)&m_shm->config_set, 1);

    m_initialized = true;
    Info("VideoEncoderSharedMem: Ready, connected to macOS bridge\n");
}

void VideoEncoderSharedMem::Shutdown() {
    if (m_shm) {
        _InterlockedExchange((volatile long*)&m_shm->shutdown, 1);
    }
    UnmapSharedMemory();
    m_stagingTexture.Reset();
    m_initialized = false;
}

bool VideoEncoderSharedMem::SetupStagingTexture(ID3D11Texture2D* pTexture) {
    D3D11_TEXTURE2D_DESC desc;
    pTexture->GetDesc(&desc);

    if (!IsSupportedSharedMemoryFormat(desc.Format)) {
        Error(
            "VideoEncoderSharedMem: unsupported texture format for BGRA bridge: %u. Disable "
            "HDR/10-bit output for the shared-memory encoder.\n",
            desc.Format
        );
        m_stagingTextureDesc.Format = kUnsupportedFormat;
        return false;
    }

    if ((int)desc.Width != m_width || (int)desc.Height != m_height) {
        Error(
            "VideoEncoderSharedMem: texture size mismatch: texture=%ux%u encoder=%dx%d\n",
            desc.Width,
            desc.Height,
            m_width,
            m_height
        );
        return false;
    }

    m_stagingTextureDesc = desc;
    m_stagingTextureDesc.Usage = D3D11_USAGE_STAGING;
    m_stagingTextureDesc.BindFlags = 0;
    m_stagingTextureDesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    m_stagingTextureDesc.MiscFlags = 0;

    HRESULT hr = m_d3dRender->GetDevice()->CreateTexture2D(
        &m_stagingTextureDesc, nullptr, &m_stagingTexture
    );
    if (FAILED(hr)) {
        Error("VideoEncoderSharedMem: failed to create staging texture: 0x%x\n", hr);
        return false;
    }

    Info(
        "VideoEncoderSharedMem: staging texture ready, format=%u size=%ux%u\n",
        m_stagingTextureDesc.Format,
        m_stagingTextureDesc.Width,
        m_stagingTextureDesc.Height
    );
    return true;
}

bool VideoEncoderSharedMem::MapSharedMemory() {
    std::string sharedMemoryPath = WineSharedMemoryPath();
    auto start = std::chrono::steady_clock::now();
    do {
        if (TryMapSharedMemory(sharedMemoryPath)) {
            return true;
        }

        std::this_thread::sleep_for(std::chrono::milliseconds(kBridgeOpenRetryMs));
    } while (std::chrono::duration_cast<std::chrono::milliseconds>(
                 std::chrono::steady_clock::now() - start
             )
                 .count()
             < kBridgeOpenTimeoutMs);

    return false;
}

bool VideoEncoderSharedMem::TryMapSharedMemory(const std::string& sharedMemoryPath) {
    UnmapSharedMemory();

    m_fileHandle = CreateFileA(
        sharedMemoryPath.c_str(),
        GENERIC_READ | GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        NULL,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        NULL
    );

    if (m_fileHandle == INVALID_HANDLE_VALUE) {
        return false;
    }

    LARGE_INTEGER fileSize;
    if (!GetFileSizeEx(m_fileHandle, &fileSize)) {
        Warn(
            "VideoEncoderSharedMem: GetFileSizeEx failed while waiting for bridge: %lu\n",
            GetLastError()
        );
        UnmapSharedMemory();
        return false;
    }

    size_t expectedSize = alvr_shm_total_size();
    if ((size_t)fileSize.QuadPart < expectedSize) {
        Debug(
            "VideoEncoderSharedMem: waiting for shared file to be sized: %lld < %zu\n",
            fileSize.QuadPart,
            expectedSize
        );
        UnmapSharedMemory();
        return false;
    }

    m_mappingHandle = CreateFileMappingA(m_fileHandle, NULL, PAGE_READWRITE, 0, 0, NULL);
    if (!m_mappingHandle) {
        Warn(
            "VideoEncoderSharedMem: CreateFileMapping failed while waiting for bridge: %lu\n",
            GetLastError()
        );
        UnmapSharedMemory();
        return false;
    }

    void* ptr = MapViewOfFile(m_mappingHandle, FILE_MAP_ALL_ACCESS, 0, 0, expectedSize);
    if (!ptr) {
        Warn(
            "VideoEncoderSharedMem: MapViewOfFile failed while waiting for bridge: %lu\n",
            GetLastError()
        );
        UnmapSharedMemory();
        return false;
    }

    m_shm = (AlvrSharedMemory*)ptr;
    m_frameData = (uint8_t*)ptr + alvr_shm_frame_offset(0);

    if (m_shm->magic != ALVR_SHM_MAGIC || m_shm->version != ALVR_SHM_VERSION) {
        Debug(
            "VideoEncoderSharedMem: waiting for shared header magic/version: magic=0x%x "
            "version=%u\n",
            m_shm->magic,
            m_shm->version
        );
        UnmapSharedMemory();
        return false;
    }

    return true;
}

void VideoEncoderSharedMem::UnmapSharedMemory() {
    if (m_shm) {
        UnmapViewOfFile(m_shm);
        m_shm = nullptr;
        m_frameData = nullptr;
    }
    if (m_mappingHandle) {
        CloseHandle(m_mappingHandle);
        m_mappingHandle = NULL;
    }
    if (m_fileHandle != INVALID_HANDLE_VALUE) {
        CloseHandle(m_fileHandle);
        m_fileHandle = INVALID_HANDLE_VALUE;
    }
}

bool VideoEncoderSharedMem::WaitForBridgeReady(int timeoutMs) {
    auto start = std::chrono::steady_clock::now();
    while (true) {
        if (BridgeMappingLive(m_shm)) {
            return true;
        }

        auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - start
        );
        if (elapsed.count() >= timeoutMs) {
            return false;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
}

int VideoEncoderSharedMem::AcquireWriteBuffer() {
    uint64_t sequence = m_shm->write_sequence;
    for (int attempt = 0; attempt < ALVR_NUM_BUFFERS; attempt++) {
        int idx = alvr_shm_next_buffer(sequence + attempt);
        AlvrFrameHeader* header = &m_shm->frame_headers[idx];
        uint32_t expected = ALVR_FRAME_EMPTY;
        if (_InterlockedCompareExchange(
                (volatile long*)&header->state, ALVR_FRAME_WRITING, expected
            )
            == expected) {
            return idx;
        }
    }

    return -1;
}

void VideoEncoderSharedMem::ReleaseWriteBuffer(int bufferIndex) {
    AlvrFrameHeader* header = &m_shm->frame_headers[bufferIndex];
    _InterlockedExchange((volatile long*)&header->state, ALVR_FRAME_READY);
    _InterlockedIncrement64((volatile LONG64*)&m_shm->write_sequence);
    _InterlockedIncrement64((volatile LONG64*)&m_shm->frames_written);
}

void VideoEncoderSharedMem::Transmit(
    ID3D11Texture2D* pTexture, uint64_t presentationTime, uint64_t targetTimestampNs, bool insertIDR
) {
    (void)presentationTime;
    auto transmitStart = std::chrono::steady_clock::now();

    if (!m_initialized || !BridgeMappingLive(m_shm)) {
        return;
    }

    if (m_stagingTextureDesc.Format == kUnsupportedFormat) {
        return;
    }

    if (!m_stagingTexture && !SetupStagingTexture(pTexture)) {
        return;
    }

    int bufferIdx = AcquireWriteBuffer();
    if (bufferIdx < 0) {
        _InterlockedIncrement64((volatile LONG64*)&m_shm->frames_dropped);
        if (m_frameIndex % 100 == 0) {
            Warn("VideoEncoderSharedMem: dropping frame %llu\n", m_frameIndex);
        }
        m_frameIndex++;
        return;
    }

    auto copyResourceStart = std::chrono::steady_clock::now();
    m_d3dRender->GetContext()->CopyResource(m_stagingTexture.Get(), pTexture);
    auto copyResourceDone = std::chrono::steady_clock::now();

    D3D11_MAPPED_SUBRESOURCE mapped;
    auto mapStart = std::chrono::steady_clock::now();
    HRESULT hr
        = m_d3dRender->GetContext()->Map(m_stagingTexture.Get(), 0, D3D11_MAP_READ, 0, &mapped);
    auto mapDone = std::chrono::steady_clock::now();
    if (FAILED(hr)) {
        Error("VideoEncoderSharedMem: failed to map staging texture: 0x%x\n", hr);
        _InterlockedExchange(
            (volatile long*)&m_shm->frame_headers[bufferIdx].state, ALVR_FRAME_EMPTY
        );
        return;
    }

    uint8_t* dstBase = m_frameData + (bufferIdx * ALVR_MAX_FRAME_SIZE);
    uint32_t srcPitch = mapped.RowPitch;
    uint32_t dstPitch = m_width * ALVR_BYTES_PER_PIXEL;

    uint8_t* src = (uint8_t*)mapped.pData;
    uint8_t* dst = dstBase;
    auto copyPixelsStart = std::chrono::steady_clock::now();
    if (IsBgraFormat(m_stagingTextureDesc.Format)) {
        for (int y = 0; y < m_height; y++) {
            memcpy(dst, src, dstPitch);
            src += srcPitch;
            dst += dstPitch;
        }
    } else {
        for (int y = 0; y < m_height; y++) {
            for (int x = 0; x < m_width; x++) {
                const uint8_t* pixel = src + x * ALVR_BYTES_PER_PIXEL;
                uint8_t* out = dst + x * ALVR_BYTES_PER_PIXEL;
                out[0] = pixel[2];
                out[1] = pixel[1];
                out[2] = pixel[0];
                out[3] = pixel[3];
            }
            src += srcPitch;
            dst += dstPitch;
        }
    }
    auto copyPixelsDone = std::chrono::steady_clock::now();

    m_d3dRender->GetContext()->Unmap(m_stagingTexture.Get(), 0);

    AlvrFrameHeader* header = &m_shm->frame_headers[bufferIdx];
    header->width = m_width;
    header->height = m_height;
    header->stride = dstPitch;
    header->timestamp_ns = targetTimestampNs;
    header->frame_number = m_frameIndex;
    header->is_idr = insertIDR ? 1 : 0;
    memset(header->pose, 0, sizeof(header->pose));
    header->producer_publish_wall_ns = UnixTimeNs();
    header->producer_capture_total_us = ElapsedUs(transmitStart, copyPixelsDone);
    header->producer_copy_resource_us = ElapsedUs(copyResourceStart, copyResourceDone);
    header->producer_map_wait_us = ElapsedUs(mapStart, mapDone);
    header->producer_copy_pixels_us = ElapsedUs(copyPixelsStart, copyPixelsDone);
    header->producer_pair_copy_us = 0;
    header->producer_left_capture_us = 0;
    header->producer_right_capture_us = 0;
    header->producer_real_submit_us = 0;

    ReleaseWriteBuffer(bufferIdx);
    m_frameIndex++;

    if (m_frameIndex % 90 == 0) {
        Info(
            "VideoEncoderSharedMem: frame %llu written (w:%llu e:%llu d:%llu)\n",
            m_frameIndex,
            m_shm->frames_written,
            m_shm->frames_encoded,
            m_shm->frames_dropped
        );
    }
}
