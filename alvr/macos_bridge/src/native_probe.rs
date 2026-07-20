use crate::{
    AlvrVideoSink, FrameMetadata, HardwareEncoderSupport, NativeHevcEncoder,
    NativeHevcEncoderConfig, PoolStats, SurfacePool,
    metal::MetalConverter,
    native_source::{
        NativeSource, SOURCE_SLOT_COUNT, STATUS_COPY_FAILED, STATUS_FRAME_DROPPED, STATUS_PASS,
        STATUS_SESSION_CLOSED,
    },
    probe::{ProbeConfig, default_stereo_view_params, dispatch_outputs},
};
use anyhow::{Context, Result, ensure};
use std::{
    env, fmt,
    time::{Duration, Instant},
};

const PRODUCER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(600);
const EXACT_POSE_STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone)]
pub struct NativeSourceConfig {
    pub probe: ProbeConfig,
    pub service_name: String,
    pub session_nonce: u64,
    pub source_width: u32,
    pub source_height: u32,
}

impl NativeSourceConfig {
    pub fn from_env() -> Result<Self> {
        let probe = ProbeConfig::from_env()?;
        let config = Self {
            source_height: env_u32("ALVR_IOSURFACE_SOURCE_HEIGHT", probe.height)?,
            source_width: env_u32("ALVR_IOSURFACE_SOURCE_WIDTH", probe.width)?,
            service_name: env::var("ALVR_IOSURFACE_POOL_SERVICE")
                .context("ALVR_IOSURFACE_POOL_SERVICE is required for iosurface input")?,
            session_nonce: required_env_u64("ALVR_IOSURFACE_POOL_NONCE")?,
            probe,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.service_name.is_empty(),
            "IOSurface service name must not be empty"
        );
        ensure!(
            self.session_nonce != 0,
            "IOSurface session nonce must be nonzero"
        );
        ensure!(
            self.source_width > 0 && self.source_width.is_multiple_of(4),
            "IOSurface source width must be positive and divisible by four"
        );
        ensure!(
            self.source_height > 0 && self.source_height.is_multiple_of(2),
            "IOSurface source height must be positive and even"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NativeCadenceReport {
    pub fps: u32,
    pub received: u64,
    pub submitted: u64,
    pub encoded: u64,
    pub transported: u64,
    pub encoded_bytes: u64,
    pub transported_bytes: u64,
    pub keyframes: u64,
    pub keyframe_bytes: u64,
    pub max_frame_bytes: u64,
    pub video_span: Duration,
    pub dropped: u64,
    pub not_ready_drops: u64,
    pub pool_exhausted_drops: u64,
    pub black_consumer_samples: u64,
    pub visible_consumer_samples: u64,
    pub pose_paired: u64,
    pub pose_fallback: u64,
    pub pose_bootstrap: u64,
    pub pose_generation_gaps: u64,
    pub pose_timestamp_reuses: u64,
    pub conversion_average: Duration,
    pub conversion_max: Duration,
    pub conversion_gpu_average: Duration,
    pub conversion_gpu_max: Duration,
    pub pool_available: usize,
}

impl fmt::Display for NativeCadenceReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "native_source cadence received={} submitted={} encoded={} alvr_sent={} encoded_bytes={} transported_bytes={} encoded_mbps={:.3} keyframes={} keyframe_bytes={} max_frame_bytes={} video_span_ms={} dropped={} not_ready_drops={} pool_exhausted_drops={} black_consumer_samples={} visible_consumer_samples={} pose_paired={} pose_fallback={} pose_bootstrap={} pose_generation_gaps={} pose_timestamp_reuses={} conversion_avg_us={} conversion_max_us={} conversion_gpu_avg_us={} conversion_gpu_max_us={} pool_available={}",
            self.received,
            self.submitted,
            self.encoded,
            self.transported,
            self.encoded_bytes,
            self.transported_bytes,
            normalized_megabits_per_second(self.encoded_bytes, self.encoded, self.fps),
            self.keyframes,
            self.keyframe_bytes,
            self.max_frame_bytes,
            self.video_span.as_millis(),
            self.dropped,
            self.not_ready_drops,
            self.pool_exhausted_drops,
            self.black_consumer_samples,
            self.visible_consumer_samples,
            self.pose_paired,
            self.pose_fallback,
            self.pose_bootstrap,
            self.pose_generation_gaps,
            self.pose_timestamp_reuses,
            self.conversion_average.as_micros(),
            self.conversion_max.as_micros(),
            self.conversion_gpu_average.as_micros(),
            self.conversion_gpu_max.as_micros(),
            self.pool_available,
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NativeProbeSummary {
    pub fps: u32,
    pub self_tests: u64,
    pub received_frames: u64,
    pub submitted_frames: u64,
    pub encoded_frames: u64,
    pub transported_frames: u64,
    pub encoded_bytes: u64,
    pub transported_bytes: u64,
    pub keyframes: u64,
    pub keyframe_bytes: u64,
    pub max_frame_bytes: u64,
    pub video_span: Duration,
    pub dropped_frames: u64,
    pub not_ready_drops: u64,
    pub pool_exhausted_drops: u64,
    pub black_consumer_samples: u64,
    pub visible_consumer_samples: u64,
    pub pose_paired: u64,
    pub pose_fallback: u64,
    pub pose_bootstrap: u64,
    pub pose_generation_gaps: u64,
    pub pose_timestamp_reuses: u64,
    pub last_pose_generation: u64,
    pub wall_elapsed: Duration,
    pub conversion_average: Duration,
    pub conversion_max: Duration,
    pub conversion_gpu_average: Duration,
    pub conversion_gpu_max: Duration,
    pub pool_stats: PoolStats,
    pub hardware_support: HardwareEncoderSupport,
    pub connected_to_alvr: bool,
}

impl fmt::Display for NativeProbeSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "native_source summary self_tests={} received={} submitted={} encoded={} alvr_sent={} encoded_bytes={} transported_bytes={} encoded_mbps={:.3} keyframes={} keyframe_bytes={} max_frame_bytes={} video_span_ms={} dropped={} not_ready_drops={} pool_exhausted_drops={} black_consumer_samples={} visible_consumer_samples={} pose_paired={} pose_fallback={} pose_bootstrap={} pose_generation_gaps={} pose_timestamp_reuses={} last_pose_generation={} wall_ms={} conversion_avg_us={} conversion_max_us={} conversion_gpu_avg_us={} conversion_gpu_max_us={} pool_available={}/{} leases_acquired={} leases_recycled={} hardware_hevc={} alvr_connected={}",
            self.self_tests,
            self.received_frames,
            self.submitted_frames,
            self.encoded_frames,
            self.transported_frames,
            self.encoded_bytes,
            self.transported_bytes,
            normalized_megabits_per_second(self.encoded_bytes, self.encoded_frames, self.fps),
            self.keyframes,
            self.keyframe_bytes,
            self.max_frame_bytes,
            self.video_span.as_millis(),
            self.dropped_frames,
            self.not_ready_drops,
            self.pool_exhausted_drops,
            self.black_consumer_samples,
            self.visible_consumer_samples,
            self.pose_paired,
            self.pose_fallback,
            self.pose_bootstrap,
            self.pose_generation_gaps,
            self.pose_timestamp_reuses,
            self.last_pose_generation,
            self.wall_elapsed.as_millis(),
            self.conversion_average.as_micros(),
            self.conversion_max.as_micros(),
            self.conversion_gpu_average.as_micros(),
            self.conversion_gpu_max.as_micros(),
            self.pool_stats.available,
            self.pool_stats.capacity,
            self.pool_stats.acquired,
            self.pool_stats.recycled,
            self.hardware_support.hardware_accelerated,
            self.connected_to_alvr,
        )
    }
}

pub fn run_native_source_probe(
    config: NativeSourceConfig,
    mut report: impl FnMut(NativeCadenceReport),
) -> Result<NativeProbeSummary> {
    config.validate()?;
    let source = NativeSource::new(
        &config.service_name,
        config.session_nonce,
        config.source_width,
        config.source_height,
    )?;
    println!(
        "native_source launchd service checked in name={}",
        config.service_name
    );
    let converter = MetalConverter::new()?;
    let pool = SurfacePool::new(
        config.probe.width,
        config.probe.height,
        config.probe.buffer_count,
    )?;
    let (mut encoder, hardware_support) = NativeHevcEncoder::new(NativeHevcEncoderConfig {
        width: config.probe.width,
        height: config.probe.height,
        fps: config.probe.fps,
        bitrate_bps: config.probe.bitrate_bps,
    })?;
    let fallback_view_params = default_stereo_view_params(config.probe.width, config.probe.height);

    println!(
        "native_source awaiting producer handshake timeout_ms={}",
        PRODUCER_HANDSHAKE_TIMEOUT.as_millis()
    );
    let producer = source.accept_producer(PRODUCER_HANDSHAKE_TIMEOUT)?;
    println!(
        "{}",
        producer_handshake_message(
            &config.service_name,
            config.session_nonce,
            std::process::id(),
            producer.pid,
            producer.pid_version,
            producer.start_token,
            config.source_width,
            config.source_height,
        )
    );
    let mut self_test_slots = [false; SOURCE_SLOT_COUNT];
    for _ in 0..SOURCE_SLOT_COUNT {
        let frame = source
            .next_frame(Duration::from_secs(60))?
            .context("IOSurface producer did not send all startup self-tests")?;
        let validation_status = frame.validation_status();
        if validation_status != STATUS_PASS {
            let frame_id = frame.frame_id();
            let slot_index = frame.slot_index();
            let generation = frame.generation();
            let expected = frame.expected_bgra();
            let actual = frame.actual_bgra();
            frame.release(validation_status)?;
            anyhow::bail!(
                "IOSurface self-test {frame_id} slot={slot_index} generation={generation} failed validation status={validation_status} expected={expected:?} actual={actual:?}"
            );
        }
        ensure!(
            frame.is_self_test(),
            "received a production frame before startup self-tests completed"
        );
        let slot_index = usize::try_from(frame.slot_index())
            .context("IOSurface self-test slot index does not fit usize")?;
        ensure!(
            slot_index < SOURCE_SLOT_COUNT,
            "IOSurface self-test slot {slot_index} is out of range"
        );
        ensure!(
            !self_test_slots[slot_index],
            "IOSurface slot {slot_index} was self-tested more than once"
        );
        self_test_slots[slot_index] = true;
        frame.release(STATUS_PASS)?;
    }
    ensure!(
        self_test_slots.iter().all(|passed| *passed),
        "not every IOSurface slot passed startup self-test"
    );
    let mut sink = config
        .probe
        .connect_to_alvr
        .then(|| {
            AlvrVideoSink::start(
                &config.probe.alvr_root,
                config.probe.width,
                config.probe.height,
                config.probe.fps,
                config.session_nonce,
            )
        })
        .transpose()?;
    let startup_barrier = source
        .next_frame(Duration::from_secs(60))?
        .context("IOSurface producer did not send the startup barrier")?;
    let startup_barrier_status = startup_barrier.validation_status();
    ensure!(
        startup_barrier.is_startup_barrier(),
        "received a production frame before the startup barrier"
    );
    startup_barrier.release(startup_barrier_status)?;
    ensure!(
        startup_barrier_status == STATUS_PASS,
        "IOSurface startup barrier failed validation status={startup_barrier_status}"
    );
    println!("native_source producer startup barrier released");
    if sink.is_some() {
        println!("native_source ALVR client telemetry enabled");
    }
    println!(
        "native_source startup self-tests passed slots={}",
        SOURCE_SLOT_COUNT
    );
    let start = Instant::now();
    let mut last_frame_at = start;
    let self_tests = SOURCE_SLOT_COUNT as u64;
    let mut received = 0;
    let mut submitted = 0;
    let mut encoded = 0;
    let mut transported = 0;
    let mut encoded_bytes = 0u64;
    let mut transported_bytes = 0u64;
    let mut keyframes = 0u64;
    let mut keyframe_bytes = 0u64;
    let mut max_frame_bytes = 0u64;
    let mut first_submitted_video_timestamp = None;
    let mut last_submitted_video_timestamp = None;
    let mut dropped = 0;
    let mut not_ready_drops = 0;
    let mut pool_exhausted_drops = 0;
    let mut black_consumer_samples = 0;
    let mut visible_consumer_samples = 0;
    let mut pose_paired = 0;
    let mut pose_fallback = 0;
    let mut pose_bootstrap = 0;
    let mut pose_generation_gaps = 0;
    let mut pose_timestamp_reuses = 0;
    let mut last_pose_generation = 0;
    let mut last_pose_timestamp = None;
    let mut conversion_total = Duration::ZERO;
    let mut conversion_max = Duration::ZERO;
    let mut conversion_gpu_total = Duration::ZERO;
    let mut conversion_gpu_max = Duration::ZERO;
    let mut conversion_count = 0u64;
    let mut closing = false;
    let mut closing_timeouts = 0;
    let mut exact_pose_wait_started: Option<Instant> = None;

    macro_rules! report_cadence {
        () => {
            report(NativeCadenceReport {
                fps: config.probe.fps,
                received,
                submitted,
                encoded,
                transported,
                encoded_bytes,
                transported_bytes,
                keyframes,
                keyframe_bytes,
                max_frame_bytes,
                video_span: submitted_video_span(
                    first_submitted_video_timestamp,
                    last_submitted_video_timestamp,
                    config.probe.fps,
                ),
                dropped,
                not_ready_drops,
                pool_exhausted_drops,
                black_consumer_samples,
                visible_consumer_samples,
                pose_paired,
                pose_fallback,
                pose_bootstrap,
                pose_generation_gaps,
                pose_timestamp_reuses,
                conversion_average: conversion_average(conversion_total, conversion_count),
                conversion_max,
                conversion_gpu_average: conversion_average(conversion_gpu_total, conversion_count),
                conversion_gpu_max,
                pool_available: pool.stats().available,
            });
        };
    }

    loop {
        let dispatch = dispatch_outputs(encoder.drain_ready()?, &mut sink)?;
        encoded += dispatch.encoded;
        transported += dispatch.transported;
        encoded_bytes = encoded_bytes.saturating_add(dispatch.encoded_bytes);
        transported_bytes = transported_bytes.saturating_add(dispatch.transported_bytes);
        keyframes += dispatch.keyframes;
        keyframe_bytes = keyframe_bytes.saturating_add(dispatch.keyframe_bytes);
        max_frame_bytes = max_frame_bytes.max(dispatch.max_frame_bytes);
        if let Some(sink) = sink.as_mut() {
            sink.poll_events();
            if let Some(error) = sink.connection_error() {
                anyhow::bail!("ALVR stream contract failed: {error}");
            }
            if sink.shutdown_requested() {
                closing = true;
            }
        }

        let Some(frame) = source.next_frame(Duration::from_millis(250))? else {
            if closing {
                closing_timeouts += 1;
                if closing_timeouts >= 4 {
                    break;
                }
            } else {
                ensure!(
                    last_frame_at.elapsed() < Duration::from_secs(60),
                    "IOSurface producer was idle for 60 seconds"
                );
            }
            continue;
        };
        last_frame_at = Instant::now();
        closing_timeouts = 0;

        let validation_status = frame.validation_status();
        if validation_status != STATUS_PASS {
            let frame_id = frame.frame_id();
            let slot_index = frame.slot_index();
            let generation = frame.generation();
            let expected = frame.expected_bgra();
            let actual = frame.actual_bgra();
            frame.release(validation_status)?;
            anyhow::bail!(
                "IOSurface frame {frame_id} slot={slot_index} generation={generation} failed validation status={validation_status} expected={expected:?} actual={actual:?}"
            );
        }
        let consumer_sample = frame.is_consumer_sample();
        let visible_consumer_sample = frame.is_visible_consumer_sample();
        if frame.is_self_test() {
            frame.release(STATUS_COPY_FAILED)?;
            anyhow::bail!("received a duplicate IOSurface self-test after startup completed");
        }

        received += 1;
        if closing {
            frame.release(STATUS_SESSION_CLOSED)?;
            continue;
        }
        let frame_id = frame.frame_id();
        let video_timestamp = frame.video_timestamp();
        let (pose_generation, pose_timestamp, frame_pose) = frame.frame_pose()?;
        let fallback_pose = frame.is_fallback_pose();
        let mut decoder_bootstrap_frame = false;
        if frame.is_fallback_pose() {
            pose_fallback += 1;
        } else {
            exact_pose_wait_started = None;
            pose_paired += 1;
            if last_pose_generation != 0 && pose_generation > last_pose_generation + 1 {
                pose_generation_gaps += pose_generation - last_pose_generation - 1;
            }
            if last_pose_timestamp == Some(pose_timestamp) {
                pose_timestamp_reuses += 1;
            }
            last_pose_generation = pose_generation;
            last_pose_timestamp = Some(pose_timestamp);
        }
        let metadata = if let Some(sink) = sink.as_mut() {
            let metadata = if fallback_pose {
                sink.bootstrap_frame_metadata(
                    frame_id,
                    video_timestamp,
                    pose_timestamp,
                    frame_pose,
                    fallback_view_params,
                )?
            } else {
                sink.frame_metadata(
                    frame_id,
                    video_timestamp,
                    Some((pose_generation, pose_timestamp, frame_pose)),
                )?
            };
            let Some(metadata) = metadata else {
                dropped += 1;
                not_ready_drops += 1;
                let exact_pose_wait_elapsed =
                    exact_pose_wait_started.map(|started| started.elapsed());
                let exact_pose_wait_timed_out = fallback_pose
                    && exact_pose_wait_elapsed
                        .is_some_and(|elapsed| elapsed >= EXACT_POSE_STARTUP_TIMEOUT);
                frame.release(STATUS_FRAME_DROPPED)?;
                if received % config.probe.telemetry_interval == 0 || exact_pose_wait_timed_out {
                    report_cadence!();
                }
                if exact_pose_wait_timed_out {
                    anyhow::bail!(
                        "ALVR exact render pose did not become ready within {} seconds after decoder bootstrap: received={received} dropped={dropped} pose_bootstrap={pose_bootstrap} wait_ms={}",
                        EXACT_POSE_STARTUP_TIMEOUT.as_secs(),
                        exact_pose_wait_elapsed.unwrap_or_default().as_millis(),
                    );
                }
                continue;
            };
            if fallback_pose {
                pose_bootstrap += 1;
                decoder_bootstrap_frame = true;
                exact_pose_wait_started = Some(Instant::now());
            }
            metadata
        } else {
            FrameMetadata {
                frame_id,
                stream_epoch: 0,
                video_timestamp,
                pose_timestamp: video_timestamp,
                global_view_params: fallback_view_params,
            }
        };
        let Some(lease) = pool.try_acquire()? else {
            dropped += 1;
            pool_exhausted_drops += 1;
            frame.release(STATUS_FRAME_DROPPED)?;
            if received % config.probe.telemetry_interval == 0 {
                report_cadence!();
            }
            continue;
        };

        let conversion_timing =
            converter.convert(&frame, &lease, source.width(), source.height())?;
        conversion_total += conversion_timing.wall;
        conversion_max = conversion_max.max(conversion_timing.wall);
        conversion_gpu_total += conversion_timing.gpu;
        conversion_gpu_max = conversion_gpu_max.max(conversion_timing.gpu);
        conversion_count += 1;
        let close_after_frame = submitted + 1 >= config.probe.frame_count;
        frame.release(if close_after_frame {
            STATUS_SESSION_CLOSED
        } else {
            STATUS_PASS
        })?;

        let requested_keyframe = sink
            .as_mut()
            .is_some_and(AlvrVideoSink::take_force_keyframe);
        let force_keyframe = decoder_bootstrap_frame
            || submitted % u64::from(config.probe.fps) == 0
            || requested_keyframe;
        first_submitted_video_timestamp.get_or_insert(metadata.video_timestamp);
        last_submitted_video_timestamp = Some(metadata.video_timestamp);
        let outputs = encoder.submit(lease, metadata, force_keyframe)?;
        submitted += 1;
        if consumer_sample {
            if visible_consumer_sample {
                visible_consumer_samples += 1;
            } else {
                black_consumer_samples += 1;
                println!(
                    "native_source black consumer sample submitted frame_id={frame_id} count={black_consumer_samples}"
                );
            }
        }
        let dispatch = dispatch_outputs(outputs, &mut sink)?;
        encoded += dispatch.encoded;
        transported += dispatch.transported;
        encoded_bytes = encoded_bytes.saturating_add(dispatch.encoded_bytes);
        transported_bytes = transported_bytes.saturating_add(dispatch.transported_bytes);
        keyframes += dispatch.keyframes;
        keyframe_bytes = keyframe_bytes.saturating_add(dispatch.keyframe_bytes);
        max_frame_bytes = max_frame_bytes.max(dispatch.max_frame_bytes);

        if received % config.probe.telemetry_interval == 0 || close_after_frame {
            report_cadence!();
        }
        if close_after_frame {
            closing = true;
        }
    }

    let dispatch = dispatch_outputs(encoder.finish()?, &mut sink)?;
    encoded += dispatch.encoded;
    transported += dispatch.transported;
    encoded_bytes = encoded_bytes.saturating_add(dispatch.encoded_bytes);
    transported_bytes = transported_bytes.saturating_add(dispatch.transported_bytes);
    keyframes += dispatch.keyframes;
    keyframe_bytes = keyframe_bytes.saturating_add(dispatch.keyframe_bytes);
    max_frame_bytes = max_frame_bytes.max(dispatch.max_frame_bytes);
    let pool_stats = pool.stats();
    ensure!(
        submitted == config.probe.frame_count,
        "submitted {submitted} frames, expected {}",
        config.probe.frame_count
    );
    ensure!(
        encoded == submitted,
        "VideoToolbox emitted {encoded} frames for {submitted} submissions"
    );
    ensure!(
        pose_paired + pose_fallback == submitted + dropped,
        "render-pose accounting mismatch: paired={pose_paired} fallback={pose_fallback} submitted={submitted} dropped={dropped}"
    );
    ensure!(
        pool_stats.available == pool_stats.capacity && pool_stats.acquired == pool_stats.recycled,
        "native NV12 lease accounting mismatch: available={}/{} acquired={} recycled={}",
        pool_stats.available,
        pool_stats.capacity,
        pool_stats.acquired,
        pool_stats.recycled
    );
    ensure!(
        visible_content_observed(black_consumer_samples, visible_consumer_samples),
        "consumer sampling never observed visible content: black_samples={black_consumer_samples}"
    );
    let connected_to_alvr = sink.as_mut().is_some_and(|sink| {
        sink.poll_events();
        sink.ever_connected()
    });
    ensure!(
        !config.probe.connect_to_alvr || connected_to_alvr,
        "ALVR transport probe never reached ClientConnected"
    );
    ensure!(
        !config.probe.connect_to_alvr || transported > 0,
        "ALVR transport connected but no native-source frames were sent"
    );

    Ok(NativeProbeSummary {
        fps: config.probe.fps,
        self_tests,
        received_frames: received,
        submitted_frames: submitted,
        encoded_frames: encoded,
        transported_frames: transported,
        encoded_bytes,
        transported_bytes,
        keyframes,
        keyframe_bytes,
        max_frame_bytes,
        video_span: submitted_video_span(
            first_submitted_video_timestamp,
            last_submitted_video_timestamp,
            config.probe.fps,
        ),
        dropped_frames: dropped,
        not_ready_drops,
        pool_exhausted_drops,
        black_consumer_samples,
        visible_consumer_samples,
        pose_paired,
        pose_fallback,
        pose_bootstrap,
        pose_generation_gaps,
        pose_timestamp_reuses,
        last_pose_generation,
        wall_elapsed: start.elapsed(),
        conversion_average: conversion_average(conversion_total, conversion_count),
        conversion_max,
        conversion_gpu_average: conversion_average(conversion_gpu_total, conversion_count),
        conversion_gpu_max,
        pool_stats,
        hardware_support,
        connected_to_alvr,
    })
}

fn conversion_average(total: Duration, count: u64) -> Duration {
    if count == 0 {
        Duration::ZERO
    } else {
        total / u32::try_from(count).unwrap_or(u32::MAX)
    }
}

fn submitted_video_span(first: Option<Duration>, last: Option<Duration>, fps: u32) -> Duration {
    match (first, last) {
        (Some(first), Some(last)) => {
            last.saturating_sub(first) + Duration::from_secs_f64(1.0 / f64::from(fps))
        }
        _ => Duration::ZERO,
    }
}

fn normalized_megabits_per_second(bytes: u64, frames: u64, fps: u32) -> f64 {
    if frames == 0 {
        0.0
    } else {
        bytes as f64 * 8.0 * f64::from(fps) / frames as f64 / 1_000_000.0
    }
}

fn visible_content_observed(black_samples: u64, visible_samples: u64) -> bool {
    black_samples + visible_samples == 0 || visible_samples > 0
}

fn producer_handshake_message(
    service_name: &str,
    session_nonce: u64,
    bridge_pid: u32,
    producer_pid: u32,
    producer_pid_version: u32,
    producer_start_token: u64,
    source_width: u32,
    source_height: u32,
) -> String {
    format!(
        "native_source producer handshake accepted service={} nonce={} bridge_pid={} producer_pid={} producer_pidversion={} producer_start_token={} source={}x{}",
        service_name,
        session_nonce,
        bridge_pid,
        producer_pid,
        producer_pid_version,
        producer_start_token,
        source_width,
        source_height
    )
}

fn env_u32(name: &str, default: u32) -> Result<u32> {
    env::var(name).map_or(Ok(default), |value| {
        value.parse().with_context(|| format!("invalid {name}"))
    })
}

fn required_env_u64(name: &str) -> Result<u64> {
    let value = env::var(name).with_context(|| format!("{name} is required"))?;
    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("invalid {name}"))?;
    ensure!(parsed != 0, "{name} must be nonzero");
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_one_frame_interval_in_video_span() {
        let first = Duration::from_secs(10);
        let last = first + Duration::from_millis(22);
        let span = submitted_video_span(Some(first), Some(last), 90);

        assert!((span.as_secs_f64() - (0.022 + 1.0 / 90.0)).abs() < 0.000_001);
    }

    #[test]
    fn reports_encoded_megabits_per_second() {
        assert!((normalized_megabits_per_second(50_000, 72, 90) - 0.5).abs() < 0.001);
        assert_eq!(normalized_megabits_per_second(50_000, 0, 90), 0.0);
    }

    #[test]
    fn requires_eventual_visibility_after_consumer_sampling_starts() {
        assert!(visible_content_observed(0, 0));
        assert!(!visible_content_observed(2, 0));
        assert!(visible_content_observed(2, 1));
    }

    #[test]
    fn handshake_log_binds_nonce_and_authenticated_producer_pid() {
        assert_eq!(
            producer_handshake_message(
                "com.alvr.fixture",
                42,
                4321,
                9002,
                77,
                1_721_278_802_123_456,
                3240,
                1800,
            ),
            "native_source producer handshake accepted service=com.alvr.fixture nonce=42 bridge_pid=4321 producer_pid=9002 producer_pidversion=77 producer_start_token=1721278802123456 source=3240x1800"
        );
    }
}
