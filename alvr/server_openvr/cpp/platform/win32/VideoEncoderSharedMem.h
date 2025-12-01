#pragma once

#include <winsock2.h>  // Must be before windows.h
#include "VideoEncoder.h"
#include "shared/d3drender.h"
#include "shared/alvr_shm_protocol.h"
#include <wrl/client.h>
#include <memory>
#include <string>

// VideoEncoderSharedMem: Transfers frames to macOS via shared memory
// for hardware encoding with VideoToolbox.
//
// This encoder is used when running under Wine/CrossOver on macOS.
// It writes raw BGRA frames to a memory-mapped file that the native
// macOS alvr_macos_bridge process reads and encodes.

class VideoEncoderSharedMem : public VideoEncoder {
public:
    VideoEncoderSharedMem(
        std::shared_ptr<CD3DRender> d3dRender,
        uint32_t width,
        uint32_t height
    );
    ~VideoEncoderSharedMem();

    void Initialize() override;
    void Shutdown() override;

    void Transmit(
        ID3D11Texture2D* pTexture,
        uint64_t presentationTime,
        uint64_t targetTimestampNs,
        bool insertIDR
    ) override;

private:
    bool MapSharedMemory();
    void UnmapSharedMemory();
    bool WaitForMacOSReady(int timeoutMs);
    int AcquireWriteBuffer();
    void ReleaseWriteBuffer(int bufferIndex);

    std::shared_ptr<CD3DRender> m_d3dRender;
    uint32_t m_width;
    uint32_t m_height;

    // Shared memory
    HANDLE m_fileHandle;
    HANDLE m_mappingHandle;
    AlvrSharedMemory* m_shm;
    uint8_t* m_frameData;  // Pointer to frame pixel data region

    // Staging texture for CPU readback
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_stagingTexture;

    // State
    bool m_initialized;
    uint64_t m_frameIndex;
};
