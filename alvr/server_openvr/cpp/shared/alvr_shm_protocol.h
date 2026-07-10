#pragma once

#include <stddef.h>
#include <stdint.h>

#define ALVR_SHM_PATH "/tmp/alvr_frame_buffer.shm"
#define ALVR_SHM_MAGIC 0x414C5652
#define ALVR_SHM_VERSION 5

#define ALVR_MAX_WIDTH 4096
#define ALVR_MAX_HEIGHT 2048
#define ALVR_BYTES_PER_PIXEL 4
#define ALVR_MAX_FRAME_SIZE (ALVR_MAX_WIDTH * ALVR_MAX_HEIGHT * ALVR_BYTES_PER_PIXEL)
#define ALVR_NUM_BUFFERS 3

typedef enum {
    ALVR_FRAME_EMPTY = 0,
    ALVR_FRAME_WRITING = 1,
    ALVR_FRAME_READY = 2,
    ALVR_FRAME_ENCODING = 3,
} AlvrFrameState;

typedef struct {
    volatile uint32_t state;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint64_t timestamp_ns;
    uint64_t frame_number;
    uint8_t is_idr;
    uint8_t padding[7];
    float pose[3][4];
    uint64_t producer_publish_wall_ns;
    uint32_t producer_capture_total_us;
    uint32_t producer_copy_resource_us;
    uint32_t producer_map_wait_us;
    uint32_t producer_copy_pixels_us;
    uint32_t producer_pair_copy_us;
    uint32_t producer_left_capture_us;
    uint32_t producer_right_capture_us;
    uint32_t producer_real_submit_us;
} AlvrFrameHeader;

typedef struct {
    uint32_t magic;
    uint32_t version;
    uint32_t initialized;
    uint32_t shutdown;
    uint32_t config_width;
    uint32_t config_height;
    uint32_t config_format;
    uint32_t config_set;
    volatile uint64_t write_sequence;
    volatile uint64_t read_sequence;
    volatile uint64_t frames_written;
    volatile uint64_t frames_encoded;
    volatile uint64_t frames_dropped;
    volatile uint64_t bridge_session_id;
    volatile uint64_t bridge_heartbeat_ns;
    volatile uint32_t view_config_set;
    float view_fov[2][4];
    float view_eye_x_m[2];
    volatile uint32_t hmd_pose_set;
    volatile uint32_t hmd_pose_sequence;
    volatile uint32_t frame_pose_sequence;
    uint64_t hmd_pose_timestamp_ns;
    uint64_t frame_pose_timestamp_ns;
    float frame_pose[3][4];
    float hmd_pose[3][4];
    AlvrFrameHeader frame_headers[ALVR_NUM_BUFFERS];
} AlvrSharedMemory;

#ifdef __cplusplus
static_assert(offsetof(AlvrSharedMemory, write_sequence) == 32, "write_sequence ABI offset");
static_assert(offsetof(AlvrSharedMemory, hmd_pose_set) == 132, "hmd pose flag ABI offset");
static_assert(offsetof(AlvrSharedMemory, hmd_pose_timestamp_ns) == 144, "hmd pose timestamp ABI offset");
static_assert(offsetof(AlvrSharedMemory, frame_headers) == 256, "frame_headers ABI offset");
static_assert(offsetof(AlvrSharedMemory, view_config_set) == 88, "view config ABI offset");
static_assert(offsetof(AlvrFrameHeader, producer_publish_wall_ns) == 88, "frame timing ABI offset");
static_assert(sizeof(AlvrFrameHeader) == 128, "frame header ABI size");
#endif

static inline size_t alvr_shm_frame_offset(int buffer_index) {
    size_t header_size = (sizeof(AlvrSharedMemory) + 4095) & ~((size_t)4095);
    return header_size + (buffer_index * ALVR_MAX_FRAME_SIZE);
}

static inline size_t alvr_shm_total_size(void) {
    return alvr_shm_frame_offset(ALVR_NUM_BUFFERS);
}

static inline int alvr_shm_next_buffer(uint64_t sequence) {
    return (int)(sequence % ALVR_NUM_BUFFERS);
}
