#include "VideoEncoderSharedMem.h"
#include "alvr_server/Logger.h"
#include <chrono>
#include <thread>

VideoEncoderSharedMem::VideoEncoderSharedMem(
    std::shared_ptr<CD3DRender> d3dRender,
    uint32_t width,
    uint32_t height
)
    : m_d3dRender(d3dRender)
    , m_width(width)
    , m_height(height)
    , m_fileHandle(INVALID_HANDLE_VALUE)
    , m_mappingHandle(NULL)
    , m_shm(nullptr)
    , m_frameData(nullptr)
    , m_initialized(false)
    , m_frameIndex(0) {
}

VideoEncoderSharedMem::~VideoEncoderSharedMem() {
    Shutdown();
}

void VideoEncoderSharedMem::Initialize() {
    Info("VideoEncoderSharedMem: Initializing for %dx%d\n", m_width, m_height);

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

    // Map shared memory
    if (!MapSharedMemory()) {
        throw MakeException("Failed to map shared memory - is alvr_macos_bridge running?");
    }

    // Wait for macOS side to be ready
    if (!WaitForMacOSReady(5000)) {
        UnmapSharedMemory();
        throw MakeException("Timeout waiting for macOS bridge - start alvr_macos_bridge first");
    }

    // Set configuration
    m_shm->config_width = m_width;
    m_shm->config_height = m_height;
    m_shm->config_format = DXGI_FORMAT_B8G8R8A8_UNORM;
    _mm_sfence();  // Ensure writes are visible
    m_shm->config_set = 1;

    m_initialized = true;
    Info("VideoEncoderSharedMem: Ready, connected to macOS bridge\n");
}

void VideoEncoderSharedMem::Shutdown() {
    if (m_shm) {
        m_shm->shutdown = 1;
    }
    UnmapSharedMemory();
    m_stagingTexture.Reset();
    m_initialized = false;
}

bool VideoEncoderSharedMem::MapSharedMemory() {
    // Under Wine, file paths are translated to Unix paths
    // /tmp/ is typically mapped to the host /tmp/

    // First try to open existing file (created by macOS bridge)
    m_fileHandle = CreateFileA(
        "Z:\\tmp\\alvr_frame_buffer.shm",  // Wine Z: drive is Unix root
        GENERIC_READ | GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        NULL,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        NULL
    );

    if (m_fileHandle == INVALID_HANDLE_VALUE) {
        DWORD err = GetLastError();
        Error("VideoEncoderSharedMem: Cannot open shared memory file: %d\n", err);
        Error("VideoEncoderSharedMem: Make sure alvr_macos_bridge is running\n");
        return false;
    }

    // Get file size
    LARGE_INTEGER fileSize;
    if (!GetFileSizeEx(m_fileHandle, &fileSize)) {
        Error("VideoEncoderSharedMem: Cannot get file size\n");
        CloseHandle(m_fileHandle);
        m_fileHandle = INVALID_HANDLE_VALUE;
        return false;
    }

    size_t expectedSize = alvr_shm_total_size();
    if ((size_t)fileSize.QuadPart < expectedSize) {
        Error("VideoEncoderSharedMem: File too small: %lld < %zu\n",
              fileSize.QuadPart, expectedSize);
        CloseHandle(m_fileHandle);
        m_fileHandle = INVALID_HANDLE_VALUE;
        return false;
    }

    // Create file mapping
    m_mappingHandle = CreateFileMappingA(
        m_fileHandle,
        NULL,
        PAGE_READWRITE,
        0, 0,  // Map entire file
        NULL
    );

    if (!m_mappingHandle) {
        Error("VideoEncoderSharedMem: CreateFileMapping failed: %d\n", GetLastError());
        CloseHandle(m_fileHandle);
        m_fileHandle = INVALID_HANDLE_VALUE;
        return false;
    }

    // Map view
    void* ptr = MapViewOfFile(
        m_mappingHandle,
        FILE_MAP_ALL_ACCESS,
        0, 0,
        expectedSize
    );

    if (!ptr) {
        Error("VideoEncoderSharedMem: MapViewOfFile failed: %d\n", GetLastError());
        CloseHandle(m_mappingHandle);
        CloseHandle(m_fileHandle);
        m_mappingHandle = NULL;
        m_fileHandle = INVALID_HANDLE_VALUE;
        return false;
    }

    m_shm = (AlvrSharedMemory*)ptr;
    m_frameData = (uint8_t*)ptr + alvr_shm_frame_offset(0);

    // Verify magic
    if (m_shm->magic != ALVR_SHM_MAGIC) {
        Error("VideoEncoderSharedMem: Invalid magic: 0x%x (expected 0x%x)\n",
              m_shm->magic, ALVR_SHM_MAGIC);
        UnmapSharedMemory();
        return false;
    }

    Info("VideoEncoderSharedMem: Mapped shared memory at %p\n", ptr);
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

bool VideoEncoderSharedMem::WaitForMacOSReady(int timeoutMs) {
    auto start = std::chrono::steady_clock::now();

    while (true) {
        if (m_shm->initialized) {
            return true;
        }

        auto now = std::chrono::steady_clock::now();
        auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(now - start);
        if (elapsed.count() >= timeoutMs) {
            return false;
        }

        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
}

int VideoEncoderSharedMem::AcquireWriteBuffer() {
    // Find a buffer that's in EMPTY state
    // Using write_sequence to determine which buffer to use
    uint64_t seq = m_shm->write_sequence;

    for (int attempt = 0; attempt < ALVR_NUM_BUFFERS; attempt++) {
        int idx = alvr_shm_next_buffer(seq + attempt);
        AlvrFrameHeader* header = &m_shm->frame_headers[idx];

        uint32_t expected = ALVR_FRAME_EMPTY;
        // Atomic compare-exchange
        if (_InterlockedCompareExchange((volatile long*)&header->state,
                                         ALVR_FRAME_WRITING, expected) == expected) {
            return idx;
        }
    }

    // All buffers busy - drop frame
    return -1;
}

void VideoEncoderSharedMem::ReleaseWriteBuffer(int bufferIndex) {
    AlvrFrameHeader* header = &m_shm->frame_headers[bufferIndex];
    _mm_sfence();  // Ensure all writes are visible
    header->state = ALVR_FRAME_READY;
    _InterlockedIncrement64((volatile LONG64*)&m_shm->write_sequence);
    _InterlockedIncrement64((volatile LONG64*)&m_shm->frames_written);
}

void VideoEncoderSharedMem::Transmit(
    ID3D11Texture2D* pTexture,
    uint64_t presentationTime,
    uint64_t targetTimestampNs,
    bool insertIDR
) {
    if (!m_initialized || !m_shm) {
        return;
    }

    // Check for shutdown
    if (m_shm->shutdown) {
        return;
    }

    // Acquire a buffer to write to
    int bufferIdx = AcquireWriteBuffer();
    if (bufferIdx < 0) {
        // All buffers full - drop frame
        _InterlockedIncrement64((volatile LONG64*)&m_shm->frames_dropped);
        if (m_frameIndex % 100 == 0) {
            Warn("VideoEncoderSharedMem: Dropping frame %llu (encoder too slow)\n", m_frameIndex);
        }
        m_frameIndex++;
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
        Error("VideoEncoderSharedMem: Failed to map staging texture: 0x%x\n", hr);
        // Release buffer back to empty state
        m_shm->frame_headers[bufferIdx].state = ALVR_FRAME_EMPTY;
        return;
    }

    // Calculate destination in shared memory
    uint8_t* dstBase = m_frameData + (bufferIdx * ALVR_MAX_FRAME_SIZE);
    uint32_t srcPitch = mapped.RowPitch;
    uint32_t dstPitch = m_width * ALVR_BYTES_PER_PIXEL;

    // Copy pixel data (handle row pitch)
    if (srcPitch == dstPitch) {
        memcpy(dstBase, mapped.pData, m_height * dstPitch);
    } else {
        uint8_t* src = (uint8_t*)mapped.pData;
        uint8_t* dst = dstBase;
        for (uint32_t y = 0; y < m_height; y++) {
            memcpy(dst, src, dstPitch);
            src += srcPitch;
            dst += dstPitch;
        }
    }

    m_d3dRender->GetContext()->Unmap(m_stagingTexture.Get(), 0);

    // Fill frame header
    AlvrFrameHeader* header = &m_shm->frame_headers[bufferIdx];
    header->width = m_width;
    header->height = m_height;
    header->stride = dstPitch;
    header->timestamp_ns = targetTimestampNs;
    header->frame_number = m_frameIndex;
    header->is_idr = insertIDR ? 1 : 0;
    // TODO: Copy pose data if needed
    memset(header->pose, 0, sizeof(header->pose));

    // Release buffer for encoding
    ReleaseWriteBuffer(bufferIdx);

    m_frameIndex++;

    // Log progress periodically
    if (m_frameIndex % 90 == 0) {
        Info("VideoEncoderSharedMem: Frame %llu written (w:%llu e:%llu d:%llu)\n",
             m_frameIndex, m_shm->frames_written, m_shm->frames_encoded, m_shm->frames_dropped);
    }
}
