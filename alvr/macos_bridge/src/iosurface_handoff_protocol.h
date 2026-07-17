#ifndef ALVR_IOSURFACE_HANDOFF_PROTOCOL_H
#define ALVR_IOSURFACE_HANDOFF_PROTOCOL_H

#include <stdint.h>

#define ALVR_IOSURFACE_PROTOCOL_VERSION UINT32_C(3)
#define ALVR_IOSURFACE_PIXEL_FORMAT_BGRA UINT32_C(0x42475241)

enum alvr_iosurface_message_id
{
    ALVR_IOSURFACE_MESSAGE_REQUEST = 0x41560001,
    ALVR_IOSURFACE_MESSAGE_OFFER = 0x41560002,
    ALVR_IOSURFACE_MESSAGE_ACK = 0x41560003,
    ALVR_IOSURFACE_MESSAGE_FRAME_READY = 0x41560004,
    ALVR_IOSURFACE_MESSAGE_SLOT_RELEASE = 0x41560005
};

enum alvr_iosurface_frame_flags
{
    ALVR_IOSURFACE_FRAME_SELF_TEST = 1u << 0,
    ALVR_IOSURFACE_FRAME_CONSUMER_SAMPLE = 1u << 1,
    ALVR_IOSURFACE_FRAME_FALLBACK_POSE = 1u << 2,
    ALVR_IOSURFACE_FRAME_STARTUP_BARRIER = 1u << 3
};

enum alvr_iosurface_probe_status
{
    ALVR_IOSURFACE_PROBE_PASS = 0,
    ALVR_IOSURFACE_PROBE_PROTOCOL_MISMATCH = 1,
    ALVR_IOSURFACE_PROBE_LOOKUP_FAILED = 2,
    ALVR_IOSURFACE_PROBE_METADATA_MISMATCH = 3,
    ALVR_IOSURFACE_PROBE_PIXEL_MISMATCH = 4,
    ALVR_IOSURFACE_PROBE_LOCK_FAILED = 5,
    ALVR_IOSURFACE_PROBE_BIND_FAILED = 6,
    ALVR_IOSURFACE_PROBE_IDENTITY_MISMATCH = 7,
    ALVR_IOSURFACE_PROBE_COPY_FAILED = 8,
    ALVR_IOSURFACE_PROBE_SESSION_CLOSED = 9,
    ALVR_IOSURFACE_PROBE_FRAME_DROPPED = 10
};

struct alvr_iosurface_request
{
    uint64_t session_nonce;
    uint32_t protocol_version;
    uint32_t client_pid;
};

struct alvr_iosurface_offer
{
    uint64_t session_nonce;
    uint64_t frame_id;
    uint32_t protocol_version;
    uint32_t slot_index;
    uint32_t generation;
    uint32_t surface_id;
    uint32_t width;
    uint32_t height;
    uint32_t bytes_per_row;
    uint32_t pixel_format;
    uint32_t sample_x;
    uint32_t sample_y;
    uint8_t expected_bgra[4];
    uint32_t producer_pid;
};

struct alvr_iosurface_ack
{
    uint64_t session_nonce;
    uint64_t frame_id;
    uint32_t protocol_version;
    uint32_t slot_index;
    uint32_t generation;
    uint32_t status;
    uint32_t surface_id;
    uint8_t actual_bgra[4];
    uint32_t client_pid;
    uint32_t reserved;
};

struct alvr_iosurface_frame_ready
{
    uint64_t session_nonce;
    uint64_t frame_id;
    uint64_t video_timestamp_ns;
    uint32_t protocol_version;
    uint32_t slot_index;
    uint32_t generation;
    uint32_t flags;
    uint32_t surface_id;
    uint32_t width;
    uint32_t height;
    uint32_t sample_x;
    uint32_t sample_y;
    uint8_t expected_bgra[4];
    uint32_t producer_pid;
    uint64_t pose_timestamp_ns;
    uint64_t pose_generation;
    float pose[3][4];
};

struct alvr_iosurface_slot_release
{
    uint64_t session_nonce;
    uint64_t frame_id;
    uint32_t protocol_version;
    uint32_t slot_index;
    uint32_t generation;
    uint32_t status;
    uint32_t surface_id;
    uint32_t consumer_pid;
    uint8_t actual_bgra[4];
    uint32_t reserved;
};

#if defined(__cplusplus)
static_assert(sizeof(struct alvr_iosurface_request) == 16);
static_assert(sizeof(struct alvr_iosurface_offer) == 64);
static_assert(sizeof(struct alvr_iosurface_ack) == 48);
static_assert(sizeof(struct alvr_iosurface_frame_ready) == 136);
static_assert(sizeof(struct alvr_iosurface_slot_release) == 48);
#else
_Static_assert(sizeof(struct alvr_iosurface_request) == 16,
               "request wire layout changed");
_Static_assert(sizeof(struct alvr_iosurface_offer) == 64,
               "offer wire layout changed");
_Static_assert(sizeof(struct alvr_iosurface_ack) == 48,
               "ack wire layout changed");
_Static_assert(sizeof(struct alvr_iosurface_frame_ready) == 136,
               "frame-ready wire layout changed");
_Static_assert(sizeof(struct alvr_iosurface_slot_release) == 48,
               "slot-release wire layout changed");
#endif

#endif
