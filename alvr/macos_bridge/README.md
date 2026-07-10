# ALVR macOS native surface contract

This crate isolates the first native macOS frame-source contract from the older
diagnostic bridge. It owns a bounded set of IOSurface-backed NV12
`CVPixelBuffer`s and keeps each lease alive until the matching VideoToolbox
output is observed.

```text
acquire lease
  -> producer writes the leased IOSurface/CVPixelBuffer
  -> submit frame metadata and the lease
  -> VideoToolbox emits the matching HEVC frame
  -> lease returns to the bounded pool
```

The contract requires strictly increasing frame IDs and video timestamps.
Pose timestamps are carried separately and must be nondecreasing, so a producer
can pair a frame with the tracking sample used to build its global view params.
Frame reordering is disabled and encoded outputs are matched to submitted leases
in FIFO order.

## Finite probe

The default probe allocates six IOSurface-backed buffers, initializes them once,
then changes only a small marker directly in each leased surface before encoding
180 HEVC frames at the proven `3664x1920@90` shape and 50 Mbps:

```bash
cargo run -p alvr_macos_bridge
```

Useful bounded overrides are:

```bash
ALVR_BRIDGE_FRAMES=270 \
ALVR_BRIDGE_BUFFER_COUNT=6 \
ALVR_BRIDGE_TELEMETRY_INTERVAL=90 \
cargo run -p alvr_macos_bridge --release
```

Each cadence line reports submitted and encoded totals, source-write and encode
submission timing, deadline misses, and the minimum number of available leases.
The final line succeeds only if every submitted frame was emitted and every
lease returned to the pool.

## Optional ALVR transport

Set `ALVR_BRIDGE_CONNECT=1` to initialize the current upstream
`ServerCoreContext`, process connection/IDR/view/tracking events, and send the
encoded HEVC stream through ALVR while the finite probe runs:

```bash
ALVR_BRIDGE_CONNECT=1 \
ALVR_BRIDGE_ROOT="$HOME/Library/Application Support/ALVR/macos_bridge" \
ALVR_BRIDGE_FRAMES=900 \
cargo run -p alvr_macos_bridge --release
```

Connect mode writes ALVR's `session.json`, `session_log.txt`, and
`crash_log.txt` beneath `ALVR_BRIDGE_ROOT`. Removing that probe root cleans up
the generated files.

## Deliberate limits

- The probe directly mutates a small surface marker; it is not a real Metal,
  CrossOver, OpenVR, or OpenXR producer and performs no reprojection.
- The first contract is HEVC Main, 8-bit video-range NV12 only.
- Hardware HEVC capability is required through VideoToolbox's encoder inventory.
  The encoder dependency does not expose the created session's
  `UsingHardwareAcceleratedVideoEncoder` property, so the check is capability
  preflight rather than per-session attestation.
- `ServerCoreContext::send_video_nal()` has one wire timestamp. The contract
  retains the separate pose timestamp used to resolve global view params, while
  ALVR transport receives the video timestamp and those resolved params.
- The probe does not adapt its surface shape to a connected client's negotiated
  resolution. A physical run must configure a compatible ALVR session.
- Producer fence import and real GPU texture handoff remain outside this slice.
