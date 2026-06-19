#pragma once

#include "VideoEncoder.h"
#include "shared/alvr_shm_protocol.h"
#include <string>
#include <windows.h>
#include <wrl/client.h>

class VideoEncoderSharedMem : public VideoEncoder {
public:
    VideoEncoderSharedMem(std::shared_ptr<CD3DRender> d3dRender, int width, int height);
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
    bool SetupStagingTexture(ID3D11Texture2D* pTexture);
    bool MapSharedMemory();
    bool TryMapSharedMemory(const std::string& sharedMemoryPath);
    void UnmapSharedMemory();
    bool WaitForBridgeReady(int timeoutMs);
    int AcquireWriteBuffer();
    void ReleaseWriteBuffer(int bufferIndex);

    std::shared_ptr<CD3DRender> m_d3dRender;
    int m_width;
    int m_height;
    HANDLE m_fileHandle;
    HANDLE m_mappingHandle;
    AlvrSharedMemory* m_shm;
    uint8_t* m_frameData;
    bool m_initialized;
    uint64_t m_frameIndex;
    D3D11_TEXTURE2D_DESC m_stagingTextureDesc;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_stagingTexture;
};
