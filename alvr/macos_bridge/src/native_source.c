#include <bootstrap.h>
#include <bsm/libbsm.h>
#include <CoreFoundation/CoreFoundation.h>
#include <IOSurface/IOSurface.h>
#include <libproc.h>
#include <mach/mach.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "iosurface_handoff_protocol.h"

enum
{
    source_slot_count = 3,
    import_send_timeout_ms = 5000,
    release_send_timeout_ms = 200
};

struct request_message
{
    mach_msg_header_t header;
    struct alvr_iosurface_request payload;
};

struct offer_message
{
    mach_msg_header_t header;
    mach_msg_body_t body;
    mach_msg_port_descriptor_t surface_port;
    struct alvr_iosurface_offer payload;
};

struct frame_ready_message
{
    mach_msg_header_t header;
    struct alvr_iosurface_frame_ready payload;
};

struct slot_release_message
{
    mach_msg_header_t header;
    struct alvr_iosurface_slot_release payload;
};

union receive_message
{
    struct request_message request;
    struct frame_ready_message frame;
    uint8_t bytes[sizeof(struct frame_ready_message) + MAX_TRAILER_SIZE];
};

struct source_slot
{
    IOSurfaceRef surface;
    uint32_t surface_id;
    uint32_t last_generation;
};

struct alvr_native_source_frame
{
    uint64_t frame_id;
    uint64_t video_timestamp_ns;
    uint64_t pose_timestamp_ns;
    uint64_t pose_generation;
    float pose[3][4];
    uint32_t slot_index;
    uint32_t generation;
    uint32_t flags;
    uint32_t surface_id;
    uint32_t width;
    uint32_t height;
    uint32_t sample_x;
    uint32_t sample_y;
    uint8_t expected_bgra[4];
    uint8_t actual_bgra[4];
    uint32_t reply_port;
    uint32_t validation_status;
};

_Static_assert(sizeof(struct alvr_native_source_frame) == 128,
               "native source frame ABI changed");

struct alvr_native_source
{
    char *service_name;
    uint64_t session_nonce;
    uint64_t last_frame_id;
    uint64_t last_video_timestamp_ns;
    uint64_t last_pose_generation;
    uint32_t width;
    uint32_t height;
    uint32_t producer_pid;
    uint32_t producer_pidversion;
    uint64_t producer_start_token;
    mach_port_t receive_port;
    struct source_slot slots[source_slot_count];
};

void alvr_native_source_destroy(void *opaque_source);
uint32_t alvr_native_source_producer_pid(void *opaque_source);
uint32_t alvr_native_source_producer_pidversion(void *opaque_source);
uint64_t alvr_native_source_producer_start_token(void *opaque_source);

static void set_error(char *buffer, size_t capacity, const char *message)
{
    if (buffer && capacity) snprintf(buffer, capacity, "%s", message);
}

static void set_mach_error(char *buffer,
                           size_t capacity,
                           const char *operation,
                           kern_return_t result)
{
    if (buffer && capacity)
        snprintf(buffer,
                 capacity,
                 "%s failed: %d (%s)",
                 operation,
                 result,
                 mach_error_string(result));
}

static bool valid_pose_matrix(const float pose[3][4])
{
    for (uint32_t row = 0; row < 3; ++row)
    {
        double rotation_length_squared = 0.0;
        for (uint32_t column = 0; column < 4; ++column)
        {
            if (!isfinite(pose[row][column])) return false;
            if (column < 3)
                rotation_length_squared +=
                    (double)pose[row][column] * (double)pose[row][column];
        }
        if (rotation_length_squared < 0.5 || rotation_length_squared > 1.5)
            return false;
    }
    return true;
}

static bool dictionary_set_size(CFMutableDictionaryRef dictionary,
                                CFStringRef key,
                                size_t value)
{
    int64_t signed_value = (int64_t)value;
    CFNumberRef number = CFNumberCreate(
        kCFAllocatorDefault, kCFNumberSInt64Type, &signed_value);

    if (!number) return false;
    CFDictionarySetValue(dictionary, key, number);
    CFRelease(number);
    return true;
}

static IOSurfaceRef create_surface(uint32_t width, uint32_t height)
{
    const size_t bytes_per_row = IOSurfaceAlignProperty(
        kIOSurfaceBytesPerRow, (size_t)width * 4);
    const size_t alloc_size = bytes_per_row * height;
    CFMutableDictionaryRef properties = CFDictionaryCreateMutable(
        kCFAllocatorDefault,
        0,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks);
    IOSurfaceRef surface = NULL;

    if (!properties) return NULL;
    if (!dictionary_set_size(properties, kIOSurfaceWidth, width) ||
        !dictionary_set_size(properties, kIOSurfaceHeight, height) ||
        !dictionary_set_size(properties, kIOSurfaceBytesPerElement, 4) ||
        !dictionary_set_size(properties, kIOSurfaceBytesPerRow, bytes_per_row) ||
        !dictionary_set_size(properties, kIOSurfaceAllocSize, alloc_size) ||
        !dictionary_set_size(properties,
                             kIOSurfacePixelFormat,
                             ALVR_IOSURFACE_PIXEL_FORMAT_BGRA))
    {
        CFRelease(properties);
        return NULL;
    }

    surface = IOSurfaceCreate(properties);
    CFRelease(properties);
    if (!surface) return NULL;
    if (IOSurfaceLock(surface, 0, NULL) != kIOReturnSuccess)
    {
        CFRelease(surface);
        return NULL;
    }
    memset(IOSurfaceGetBaseAddress(surface), 0, IOSurfaceGetAllocSize(surface));
    IOSurfaceUnlock(surface, 0, NULL);
    return surface;
}

static void deallocate_port(mach_port_t *port)
{
    if (*port == MACH_PORT_NULL) return;
    mach_port_deallocate(mach_task_self(), *port);
    *port = MACH_PORT_NULL;
}

static void destroy_receive_port(mach_port_t *port)
{
    if (*port == MACH_PORT_NULL) return;
    mach_port_mod_refs(
        mach_task_self(), *port, MACH_PORT_RIGHT_RECEIVE, -1);
    *port = MACH_PORT_NULL;
}

static kern_return_t check_in_service(const char *service_name,
                                      mach_port_t *receive_port)
{
    kern_return_t result;

    *receive_port = MACH_PORT_NULL;
    result = bootstrap_check_in(bootstrap_port, service_name, receive_port);
    if (result != KERN_SUCCESS) *receive_port = MACH_PORT_NULL;
    return result;
}

static uint64_t monotonic_milliseconds(void)
{
    struct timespec timestamp;

    if (clock_gettime(CLOCK_MONOTONIC, &timestamp) != 0) return 0;
    return (uint64_t)timestamp.tv_sec * UINT64_C(1000) +
           (uint64_t)timestamp.tv_nsec / UINT64_C(1000000);
}

static mach_msg_timeout_t remaining_timeout(uint64_t deadline_ms,
                                            mach_msg_timeout_t fallback_ms)
{
    const uint64_t now_ms = monotonic_milliseconds();
    uint64_t remaining_ms;

    if (!now_ms) return fallback_ms;
    if (now_ms >= deadline_ms) return 0;
    remaining_ms = deadline_ms - now_ms;
    if (remaining_ms > UINT32_MAX) remaining_ms = UINT32_MAX;
    return (mach_msg_timeout_t)remaining_ms;
}

static pid_t message_sender_pid(const mach_msg_header_t *header)
{
    const mach_msg_audit_trailer_t *trailer =
        (const mach_msg_audit_trailer_t *)((const uint8_t *)header +
                                           round_msg(header->msgh_size));

    if (trailer->msgh_trailer_type != MACH_MSG_TRAILER_FORMAT_0 ||
        trailer->msgh_trailer_size < sizeof(*trailer))
        return -1;
    return audit_token_to_pid(trailer->msgh_audit);
}

static uint32_t message_sender_pidversion(const mach_msg_header_t *header)
{
    const mach_msg_audit_trailer_t *trailer =
        (const mach_msg_audit_trailer_t *)((const uint8_t *)header +
                                           round_msg(header->msgh_size));
    if (trailer->msgh_trailer_type != MACH_MSG_TRAILER_FORMAT_0 ||
        trailer->msgh_trailer_size < sizeof(*trailer))
        return 0;
    return (uint32_t)audit_token_to_pidversion(trailer->msgh_audit);
}

static uint64_t process_start_token(pid_t pid)
{
    struct proc_bsdinfo info = {0};
    const int size = proc_pidinfo(
        pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info));

    if (size != sizeof(info) || info.pbi_pid != (uint32_t)pid) return 0;
    return info.pbi_start_tvsec * UINT64_C(1000000) + info.pbi_start_tvusec;
}

static bool has_send_once_reply(const mach_msg_header_t *header)
{
    return header->msgh_remote_port != MACH_PORT_NULL &&
           MACH_MSGH_BITS_REMOTE(header->msgh_bits) ==
               MACH_MSG_TYPE_MOVE_SEND_ONCE;
}

static const char *import_request_rejection_reason(
    const struct alvr_native_source *source,
    const struct request_message *request,
    pid_t sender_pid,
    uint32_t sender_pidversion,
    uint64_t sender_start_token)
{
    if (request->header.msgh_size != sizeof(*request)) return "message-size";
    if (request->header.msgh_bits & MACH_MSGH_BITS_COMPLEX) return "complex-message";
    if (request->header.msgh_id != ALVR_IOSURFACE_MESSAGE_REQUEST) return "message-id";
    if (!has_send_once_reply(&request->header)) return "reply-right";
    if (sender_pid <= 0) return "audit-pid";
    if (!sender_pidversion) return "audit-pidversion";
    if (!sender_start_token) return "process-start-token";
    if (request->payload.protocol_version != ALVR_IOSURFACE_PROTOCOL_VERSION)
        return "protocol-version";
    if (request->payload.session_nonce != source->session_nonce) return "session-nonce";
    if (request->payload.client_pid != (uint32_t)sender_pid) return "client-pid";
    if (source->producer_pid && (uint32_t)sender_pid != source->producer_pid)
        return "producer-pid";
    if (source->producer_pidversion &&
        sender_pidversion != source->producer_pidversion)
        return "producer-pidversion";
    if (source->producer_start_token &&
        sender_start_token != source->producer_start_token)
        return "producer-start-token";
    return NULL;
}

static kern_return_t receive_message(mach_port_t port,
                                     union receive_message *message,
                                     mach_msg_timeout_t timeout_ms)
{
    memset(message, 0, sizeof(*message));
    return mach_msg(&message->request.header,
                    MACH_RCV_MSG | MACH_RCV_TIMEOUT | MACH_RCV_INTERRUPT |
                        MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) |
                        MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT),
                    0,
                    sizeof(*message),
                    port,
                    timeout_ms,
                    MACH_PORT_NULL);
}

static kern_return_t send_offer(
    mach_port_t reply_port,
    mach_port_t surface_port,
    const struct alvr_iosurface_offer *offer,
    mach_msg_timeout_t timeout_ms)
{
    struct offer_message message = {0};
    kern_return_t result;

    message.header.msgh_bits =
        MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0) |
        MACH_MSGH_BITS_COMPLEX;
    message.header.msgh_size = sizeof(message);
    message.header.msgh_remote_port = reply_port;
    message.header.msgh_id = ALVR_IOSURFACE_MESSAGE_OFFER;
    message.body.msgh_descriptor_count = 1;
    message.surface_port.name = surface_port;
    message.surface_port.disposition = MACH_MSG_TYPE_COPY_SEND;
    message.surface_port.type = MACH_MSG_PORT_DESCRIPTOR;
    message.payload = *offer;
    result = mach_msg(&message.header,
                      MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                      message.header.msgh_size,
                      0,
                      MACH_PORT_NULL,
                      timeout_ms,
                      MACH_PORT_NULL);
    if (result != KERN_SUCCESS) deallocate_port(&reply_port);
    return result;
}

static kern_return_t send_release(
    mach_port_t reply_port,
    const struct alvr_iosurface_slot_release *release)
{
    struct slot_release_message message = {0};
    kern_return_t result;

    message.header.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0);
    message.header.msgh_size = sizeof(message);
    message.header.msgh_remote_port = reply_port;
    message.header.msgh_id = ALVR_IOSURFACE_MESSAGE_SLOT_RELEASE;
    message.payload = *release;
    result = mach_msg(&message.header,
                      MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                      message.header.msgh_size,
                      0,
                      MACH_PORT_NULL,
                      release_send_timeout_ms,
                      MACH_PORT_NULL);
    if (result != KERN_SUCCESS) deallocate_port(&reply_port);
    return result;
}

static uint32_t read_sample(IOSurfaceRef surface,
                            uint32_t x,
                            uint32_t y,
                            uint8_t actual_bgra[4])
{
    const uint8_t *base;
    size_t bytes_per_row;

    if (x >= IOSurfaceGetWidth(surface) || y >= IOSurfaceGetHeight(surface))
        return ALVR_IOSURFACE_PROBE_METADATA_MISMATCH;
    if (IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL) != kIOReturnSuccess)
        return ALVR_IOSURFACE_PROBE_LOCK_FAILED;
    base = IOSurfaceGetBaseAddress(surface);
    bytes_per_row = IOSurfaceGetBytesPerRow(surface);
    if (!base)
    {
        IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
        return ALVR_IOSURFACE_PROBE_LOCK_FAILED;
    }
    memcpy(actual_bgra, base + (size_t)y * bytes_per_row + (size_t)x * 4, 4);
    IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
    return ALVR_IOSURFACE_PROBE_PASS;
}

static uint32_t find_nonblack_sample(IOSurfaceRef surface,
                                     uint8_t actual_bgra[4])
{
    const uint8_t *base;
    size_t width = IOSurfaceGetWidth(surface);
    size_t height = IOSurfaceGetHeight(surface);
    size_t bytes_per_row;

    memset(actual_bgra, 0, 4);
    if (IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL) != kIOReturnSuccess)
        return ALVR_IOSURFACE_PROBE_LOCK_FAILED;
    base = IOSurfaceGetBaseAddress(surface);
    bytes_per_row = IOSurfaceGetBytesPerRow(surface);
    if (!base)
    {
        IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
        return ALVR_IOSURFACE_PROBE_LOCK_FAILED;
    }

    for (size_t y = 8; y < height; y += 16)
    {
        for (size_t x = 8; x < width; x += 16)
        {
            const uint8_t *pixel = base + y * bytes_per_row + x * 4;
            if ((uint32_t)pixel[0] + pixel[1] + pixel[2] >= 96)
            {
                memcpy(actual_bgra, pixel, 4);
                IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
                return ALVR_IOSURFACE_PROBE_PASS;
            }
        }
    }
    for (size_t y = 0; y < height; ++y)
    {
        for (size_t x = 0; x < width; ++x)
        {
            const uint8_t *pixel = base + y * bytes_per_row + x * 4;
            if ((uint32_t)pixel[0] + pixel[1] + pixel[2] >= 96)
            {
                memcpy(actual_bgra, pixel, 4);
                IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
                return ALVR_IOSURFACE_PROBE_PASS;
            }
        }
    }
    IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
    return ALVR_IOSURFACE_PROBE_PASS;
}

void *alvr_native_source_create(const char *service_name,
                                uint64_t session_nonce,
                                uint32_t width,
                                uint32_t height,
                                char *error_buffer,
                                size_t error_capacity)
{
    struct alvr_native_source *source;
    kern_return_t result;

    if (!service_name || !*service_name || !session_nonce || !width || !height)
    {
        set_error(error_buffer, error_capacity, "invalid native source configuration");
        return NULL;
    }
    source = calloc(1, sizeof(*source));
    if (!source)
    {
        set_error(error_buffer, error_capacity, "native source allocation failed");
        return NULL;
    }
    source->service_name = strdup(service_name);
    source->session_nonce = session_nonce;
    source->width = width;
    source->height = height;
    if (!source->service_name)
    {
        set_error(error_buffer, error_capacity, "service name allocation failed");
        free(source);
        return NULL;
    }
    for (uint32_t index = 0; index < source_slot_count; ++index)
    {
        source->slots[index].surface = create_surface(width, height);
        if (!source->slots[index].surface)
        {
            set_error(error_buffer, error_capacity, "IOSurface pool creation failed");
            alvr_native_source_destroy(source);
            return NULL;
        }
        source->slots[index].surface_id =
            IOSurfaceGetID(source->slots[index].surface);
    }
    result = check_in_service(service_name, &source->receive_port);
    if (result != KERN_SUCCESS)
    {
        set_mach_error(
            error_buffer, error_capacity, "bootstrap_check_in", result);
        alvr_native_source_destroy(source);
        return NULL;
    }
    return source;
}

int alvr_native_source_accept(void *opaque_source,
                              uint32_t timeout_ms,
                              char *error_buffer,
                              size_t error_capacity)
{
    struct alvr_native_source *source = opaque_source;
    const uint64_t started_ms = monotonic_milliseconds();
    const uint64_t deadline_ms = started_ms ? started_ms + timeout_ms : 0;

    if (!source)
    {
        set_error(error_buffer, error_capacity, "native source is null");
        return -1;
    }
    for (uint32_t slot_index = 0; slot_index < source_slot_count; ++slot_index)
    {
        union receive_message received;
        mach_port_t surface_port = MACH_PORT_NULL;
        kern_return_t result;
        pid_t sender_pid = -1;
        uint32_t sender_pidversion = 0;
        uint64_t sender_start_token = 0;

        for (;;)
        {
            const mach_msg_timeout_t receive_timeout = deadline_ms
                ? remaining_timeout(deadline_ms, timeout_ms)
                : timeout_ms;
            const char *rejection_reason;

            if (!receive_timeout)
            {
                set_mach_error(
                    error_buffer, error_capacity, "import receive", MACH_RCV_TIMED_OUT);
                return -2;
            }
            result = receive_message(
                source->receive_port, &received, receive_timeout);
            if (result == MACH_RCV_TOO_LARGE)
            {
                fprintf(stderr,
                        "native_source rejected import request slot=%u "
                        "reason=message-too-large\n",
                        slot_index);
                continue;
            }
            if (result != KERN_SUCCESS)
            {
                set_mach_error(error_buffer, error_capacity, "import receive", result);
                return -2;
            }
            sender_pid = message_sender_pid(&received.request.header);
            sender_pidversion = message_sender_pidversion(&received.request.header);
            sender_start_token = process_start_token(sender_pid);
            rejection_reason = import_request_rejection_reason(
                source,
                &received.request,
                sender_pid,
                sender_pidversion,
                sender_start_token);
            if (!rejection_reason) break;

            fprintf(stderr,
                    "native_source rejected import request slot=%u reason=%s "
                    "nonce=%llu client_pid=%u sender_pid=%d\n",
                    slot_index,
                    rejection_reason,
                    (unsigned long long)received.request.payload.session_nonce,
                    received.request.payload.client_pid,
                    sender_pid);
            mach_msg_destroy(&received.request.header);
        }
        source->producer_pid = (uint32_t)sender_pid;
        source->producer_pidversion = sender_pidversion;
        source->producer_start_token = sender_start_token;

        surface_port = IOSurfaceCreateMachPort(
            source->slots[slot_index].surface);
        if (surface_port == MACH_PORT_NULL)
        {
            deallocate_port(&received.request.header.msgh_remote_port);
            set_error(error_buffer, error_capacity, "IOSurfaceCreateMachPort failed");
            return -4;
        }
        struct alvr_iosurface_offer offer = {0};
        const mach_msg_timeout_t send_timeout = deadline_ms
            ? remaining_timeout(deadline_ms, import_send_timeout_ms)
            : import_send_timeout_ms;
        if (!send_timeout)
        {
            deallocate_port(&received.request.header.msgh_remote_port);
            deallocate_port(&surface_port);
            set_mach_error(
                error_buffer, error_capacity, "offer send", MACH_SEND_TIMED_OUT);
            return -5;
        }
        offer.session_nonce = source->session_nonce;
        offer.frame_id = slot_index + 1;
        offer.protocol_version = ALVR_IOSURFACE_PROTOCOL_VERSION;
        offer.slot_index = slot_index;
        offer.surface_id = source->slots[slot_index].surface_id;
        offer.width = source->width;
        offer.height = source->height;
        offer.bytes_per_row = IOSurfaceGetBytesPerRow(
            source->slots[slot_index].surface);
        offer.pixel_format = IOSurfaceGetPixelFormat(
            source->slots[slot_index].surface);
        offer.producer_pid = getpid();
        result = send_offer(
            received.request.header.msgh_remote_port,
            surface_port,
            &offer,
            send_timeout);
        received.request.header.msgh_remote_port = MACH_PORT_NULL;
        deallocate_port(&surface_port);
        if (result != KERN_SUCCESS)
        {
            set_mach_error(error_buffer, error_capacity, "offer send", result);
            return -5;
        }
    }
    return 0;
}

uint32_t alvr_native_source_producer_pid(void *opaque_source)
{
    struct alvr_native_source *source = opaque_source;

    return source ? source->producer_pid : 0;
}

uint32_t alvr_native_source_producer_pidversion(void *opaque_source)
{
    struct alvr_native_source *source = opaque_source;

    return source ? source->producer_pidversion : 0;
}

uint64_t alvr_native_source_producer_start_token(void *opaque_source)
{
    struct alvr_native_source *source = opaque_source;

    return source ? source->producer_start_token : 0;
}

int alvr_native_source_next_frame(void *opaque_source,
                                  uint32_t timeout_ms,
                                  struct alvr_native_source_frame *output,
                                  char *error_buffer,
                                  size_t error_capacity)
{
    struct alvr_native_source *source = opaque_source;
    union receive_message received;
    const struct alvr_iosurface_frame_ready *frame;
    struct source_slot *slot;
    kern_return_t result;
    bool consumer_sample;
    bool fallback_pose;
    bool self_test;
    bool startup_barrier;
    pid_t sender_pid;
    uint32_t sender_pidversion;
    const uint64_t started_ms = monotonic_milliseconds();
    const uint64_t deadline_ms = started_ms ? started_ms + timeout_ms : 0;

    if (!source || !output)
    {
        set_error(error_buffer, error_capacity, "invalid native source frame arguments");
        return -1;
    }
    for (;;)
    {
        const mach_msg_timeout_t receive_timeout = deadline_ms
            ? remaining_timeout(deadline_ms, timeout_ms)
            : timeout_ms;
        const char *rejection_reason = NULL;

        if (!receive_timeout) return 1;
        result = receive_message(
            source->receive_port, &received, receive_timeout);
        if (result == MACH_RCV_TIMED_OUT || result == MACH_RCV_INTERRUPTED) return 1;
        if (result == MACH_RCV_TOO_LARGE)
        {
            fprintf(stderr,
                    "native_source rejected frame-ready "
                    "reason=message-too-large\n");
            continue;
        }
        if (result != KERN_SUCCESS)
        {
            set_mach_error(error_buffer, error_capacity, "frame receive", result);
            return -2;
        }
        sender_pid = message_sender_pid(&received.frame.header);
        sender_pidversion = message_sender_pidversion(&received.frame.header);
        if (received.frame.header.msgh_size != sizeof(struct frame_ready_message))
            rejection_reason = "message-size";
        else if (received.frame.header.msgh_bits & MACH_MSGH_BITS_COMPLEX)
            rejection_reason = "complex-message";
        else if (received.frame.header.msgh_id !=
                 ALVR_IOSURFACE_MESSAGE_FRAME_READY)
            rejection_reason = "message-id";
        else if (!has_send_once_reply(&received.frame.header))
            rejection_reason = "reply-right";
        else if (sender_pid <= 0)
            rejection_reason = "audit-pid";
        else if ((uint32_t)sender_pid != source->producer_pid)
            rejection_reason = "producer-pid";
        else if (sender_pidversion != source->producer_pidversion)
            rejection_reason = "producer-pidversion";
        if (!rejection_reason) break;

        fprintf(stderr,
                "native_source rejected frame-ready reason=%s sender_pid=%d "
                "producer_pid=%u sender_pidversion=%u producer_pidversion=%u\n",
                rejection_reason,
                sender_pid,
                source->producer_pid,
                sender_pidversion,
                source->producer_pidversion);
        mach_msg_destroy(&received.frame.header);
    }

    memset(output, 0, sizeof(*output));
    frame = &received.frame.payload;
    output->frame_id = frame->frame_id;
    output->video_timestamp_ns = frame->video_timestamp_ns;
    output->pose_timestamp_ns = frame->pose_timestamp_ns;
    output->pose_generation = frame->pose_generation;
    memcpy(output->pose, frame->pose, sizeof(output->pose));
    output->slot_index = frame->slot_index;
    output->generation = frame->generation;
    output->flags = frame->flags;
    output->surface_id = frame->surface_id;
    output->width = frame->width;
    output->height = frame->height;
    output->sample_x = frame->sample_x;
    output->sample_y = frame->sample_y;
    memcpy(output->expected_bgra, frame->expected_bgra, 4);
    output->reply_port = received.frame.header.msgh_remote_port;
    received.frame.header.msgh_remote_port = MACH_PORT_NULL;
    output->validation_status = ALVR_IOSURFACE_PROBE_PASS;

    if (frame->protocol_version != ALVR_IOSURFACE_PROTOCOL_VERSION ||
        frame->session_nonce != source->session_nonce ||
        frame->slot_index >= source_slot_count ||
        sender_pid <= 0 ||
        frame->producer_pid != (uint32_t)sender_pid ||
        frame->producer_pid != source->producer_pid ||
        (frame->flags & ~(ALVR_IOSURFACE_FRAME_SELF_TEST |
                          ALVR_IOSURFACE_FRAME_CONSUMER_SAMPLE |
                          ALVR_IOSURFACE_FRAME_FALLBACK_POSE |
                          ALVR_IOSURFACE_FRAME_STARTUP_BARRIER)))
    {
        output->validation_status = ALVR_IOSURFACE_PROBE_PROTOCOL_MISMATCH;
        return 0;
    }
    slot = &source->slots[frame->slot_index];
    self_test = (frame->flags & ALVR_IOSURFACE_FRAME_SELF_TEST) != 0;
    consumer_sample =
        (frame->flags & ALVR_IOSURFACE_FRAME_CONSUMER_SAMPLE) != 0;
    fallback_pose =
        (frame->flags & ALVR_IOSURFACE_FRAME_FALLBACK_POSE) != 0;
    startup_barrier =
        (frame->flags & ALVR_IOSURFACE_FRAME_STARTUP_BARRIER) != 0;
    if (self_test && (consumer_sample || fallback_pose))
        output->validation_status = ALVR_IOSURFACE_PROBE_PROTOCOL_MISMATCH;
    if (startup_barrier && (self_test || consumer_sample || fallback_pose))
        output->validation_status = ALVR_IOSURFACE_PROBE_PROTOCOL_MISMATCH;
    if (frame->surface_id != slot->surface_id || frame->width != source->width ||
        frame->height != source->height ||
        frame->generation <= slot->last_generation ||
        frame->frame_id <= source->last_frame_id ||
        (!self_test && !startup_barrier &&
         (!frame->video_timestamp_ns ||
          frame->video_timestamp_ns <= source->last_video_timestamp_ns ||
          !frame->pose_timestamp_ns ||
          (!fallback_pose &&
           (!frame->pose_generation ||
            frame->pose_generation <= source->last_pose_generation)) ||
          (fallback_pose && frame->pose_generation) ||
          !valid_pose_matrix(frame->pose))))
        output->validation_status = ALVR_IOSURFACE_PROBE_METADATA_MISMATCH;
    if (output->validation_status == ALVR_IOSURFACE_PROBE_PASS &&
        consumer_sample)
        output->validation_status = find_nonblack_sample(
            slot->surface, output->actual_bgra);
    else if (output->validation_status == ALVR_IOSURFACE_PROBE_PASS &&
             self_test)
        output->validation_status = read_sample(
            slot->surface,
            frame->sample_x,
            frame->sample_y,
            output->actual_bgra);
    if (output->validation_status == ALVR_IOSURFACE_PROBE_PASS &&
        self_test &&
        memcmp(output->actual_bgra, output->expected_bgra, 4) != 0)
        output->validation_status = ALVR_IOSURFACE_PROBE_PIXEL_MISMATCH;
    if (output->validation_status == ALVR_IOSURFACE_PROBE_PASS)
    {
        slot->last_generation = frame->generation;
        source->last_frame_id = frame->frame_id;
        if (!self_test && !startup_barrier)
        {
            source->last_video_timestamp_ns = frame->video_timestamp_ns;
            if (!fallback_pose)
                source->last_pose_generation = frame->pose_generation;
        }
    }
    return 0;
}

void *alvr_native_source_surface(void *opaque_source, uint32_t slot_index)
{
    struct alvr_native_source *source = opaque_source;

    if (!source || slot_index >= source_slot_count) return NULL;
    return source->slots[slot_index].surface;
}

int alvr_native_source_release(void *opaque_source,
                               struct alvr_native_source_frame *frame,
                               uint32_t status,
                               char *error_buffer,
                               size_t error_capacity)
{
    struct alvr_native_source *source = opaque_source;
    struct alvr_iosurface_slot_release release = {0};
    mach_port_t reply_port;
    kern_return_t result;

    if (!source || !frame || frame->reply_port == MACH_PORT_NULL)
    {
        set_error(error_buffer, error_capacity, "invalid native source release");
        return -1;
    }
    reply_port = frame->reply_port;
    frame->reply_port = MACH_PORT_NULL;
    release.session_nonce = source->session_nonce;
    release.frame_id = frame->frame_id;
    release.protocol_version = ALVR_IOSURFACE_PROTOCOL_VERSION;
    release.slot_index = frame->slot_index;
    release.generation = frame->generation;
    release.status = frame->slot_index < source_slot_count
        ? status
        : ALVR_IOSURFACE_PROBE_PROTOCOL_MISMATCH;
    release.surface_id = frame->surface_id;
    release.consumer_pid = getpid();
    memcpy(release.actual_bgra, frame->actual_bgra, 4);
    result = send_release(reply_port, &release);
    if (result != KERN_SUCCESS)
    {
        set_mach_error(error_buffer, error_capacity, "slot release", result);
        return -2;
    }
    return 0;
}

void alvr_native_source_destroy(void *opaque_source)
{
    struct alvr_native_source *source = opaque_source;

    if (!source) return;
    destroy_receive_port(&source->receive_port);
    for (uint32_t index = 0; index < source_slot_count; ++index)
    {
        if (source->slots[index].surface)
            CFRelease(source->slots[index].surface);
    }
    free(source->service_name);
    free(source);
}
