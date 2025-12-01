#pragma once

// ALVR Shared Memory Protocol
// Used for zero-copy frame transfer between Wine and native macOS
//
// Architecture:
//   Wine (ALVR driver) -> Shared Memory -> macOS (alvr_macos_bridge)
//                                              |
//                                              v
//                                         VideoToolbox encode
//                                              |
//                                              v
//                                         ALVR network -> AVP

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Shared memory file path - accessible from both Wine and macOS
#define ALVR_SHM_PATH "/tmp/alvr_frame_buffer.shm"
#define ALVR_SHM_MAGIC 0x414C5652  // "ALVR"
#define ALVR_SHM_VERSION 1

// Maximum frame dimensions (4K stereo)
#define ALVR_MAX_WIDTH 4096
#define ALVR_MAX_HEIGHT 2048
#define ALVR_BYTES_PER_PIXEL 4  // BGRA
#define ALVR_MAX_FRAME_SIZE (ALVR_MAX_WIDTH * ALVR_MAX_HEIGHT * ALVR_BYTES_PER_PIXEL)

// Triple buffering for non-blocking operation
#define ALVR_NUM_BUFFERS 3

// Frame states for lock-free synchronization
typedef enum {
    ALVR_FRAME_EMPTY = 0,      // Buffer is free for writing
    ALVR_FRAME_WRITING = 1,    // Wine is writing to this buffer
    ALVR_FRAME_READY = 2,      // Frame is ready for encoding
    ALVR_FRAME_ENCODING = 3    // macOS is encoding this frame
} AlvrFrameState;

// Per-frame metadata
typedef struct {
    volatile uint32_t state;       // AlvrFrameState
    uint32_t width;
    uint32_t height;
    uint32_t stride;               // Row pitch in bytes
    uint64_t timestamp_ns;         // Target timestamp for this frame
    uint64_t frame_number;
    uint8_t is_idr;                // Request IDR/keyframe
    uint8_t padding[7];
    // Pose data for reprojection
    float pose[3][4];              // 3x4 transform matrix
} AlvrFrameHeader;

// Shared memory layout
typedef struct {
    // Header - initialized by macOS, read by both
    uint32_t magic;                // ALVR_SHM_MAGIC
    uint32_t version;              // ALVR_SHM_VERSION
    uint32_t initialized;          // Set to 1 when macOS is ready
    uint32_t shutdown;             // Set to 1 to signal shutdown

    // Configuration - set by Wine on first frame
    uint32_t config_width;
    uint32_t config_height;
    uint32_t config_format;        // DXGI_FORMAT (usually BGRA)
    uint32_t config_set;           // Set to 1 after config is written

    // Write cursor - Wine increments after writing each frame
    volatile uint64_t write_sequence;

    // Read cursor - macOS increments after encoding each frame
    volatile uint64_t read_sequence;

    // Statistics
    volatile uint64_t frames_written;
    volatile uint64_t frames_encoded;
    volatile uint64_t frames_dropped;

    // Padding to align frame headers
    uint8_t reserved[64];

    // Frame headers (separate from pixel data for cache efficiency)
    AlvrFrameHeader frame_headers[ALVR_NUM_BUFFERS];

    // Frame pixel data follows after headers
    // Actual offset: sizeof(AlvrSharedMemory) aligned to page boundary
    // Each buffer: ALVR_MAX_FRAME_SIZE bytes
} AlvrSharedMemory;

// Calculate offset to frame pixel data
static inline size_t alvr_shm_frame_offset(int buffer_index) {
    // Align to 4K page boundary for efficient mmap
    size_t header_size = (sizeof(AlvrSharedMemory) + 4095) & ~4095;
    return header_size + (buffer_index * ALVR_MAX_FRAME_SIZE);
}

// Total shared memory size
static inline size_t alvr_shm_total_size(void) {
    return alvr_shm_frame_offset(ALVR_NUM_BUFFERS);
}

// Helper to get next buffer index (lock-free ring buffer)
static inline int alvr_shm_next_buffer(uint64_t sequence) {
    return (int)(sequence % ALVR_NUM_BUFFERS);
}

#ifdef __cplusplus
}
#endif
