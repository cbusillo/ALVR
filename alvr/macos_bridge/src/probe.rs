use crate::{
    AlvrVideoSink, EncodedFrame, FrameMetadata, HardwareEncoderSupport, NativeHevcEncoder,
    NativeHevcEncoderConfig, PoolStats, SurfacePool,
};
use alvr_common::{Fov, Pose, ViewParams, glam::Vec3};
use anyhow::{Context, Result, ensure};
use std::{
    env, fmt,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub frame_count: u64,
    pub buffer_count: usize,
    pub telemetry_interval: u64,
    pub connect_to_alvr: bool,
    pub alvr_root: PathBuf,
}

impl ProbeConfig {
    pub fn from_env() -> Result<Self> {
        let fps = env_u32("ALVR_BRIDGE_FPS", 90)?;
        let alvr_root = match env::var_os("ALVR_BRIDGE_ROOT") {
            Some(root) => PathBuf::from(root),
            None => env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is required unless ALVR_BRIDGE_ROOT is set")?
                .join("Library/Application Support/ALVR/macos_bridge"),
        };
        let config = Self {
            width: env_u32("ALVR_BRIDGE_WIDTH", 3664)?,
            height: env_u32("ALVR_BRIDGE_HEIGHT", 1920)?,
            fps,
            bitrate_bps: env_u64("ALVR_BRIDGE_BITRATE_BPS", 50_000_000)?,
            frame_count: env_u64("ALVR_BRIDGE_FRAMES", 180)?,
            buffer_count: env_usize("ALVR_BRIDGE_BUFFER_COUNT", 6)?,
            telemetry_interval: env_u64("ALVR_BRIDGE_TELEMETRY_INTERVAL", u64::from(fps))?,
            connect_to_alvr: env_bool("ALVR_BRIDGE_CONNECT", false)?,
            alvr_root,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.width > 0 && self.width % 4 == 0,
            "probe width must be divisible by four for side-by-side NV12"
        );
        ensure!(
            self.height > 0 && self.height % 2 == 0,
            "probe height must be even"
        );
        ensure!(self.fps > 0, "probe FPS must be greater than zero");
        ensure!(
            self.bitrate_bps > 0,
            "probe bitrate must be greater than zero"
        );
        ensure!(
            self.frame_count > 0,
            "probe frame count must be greater than zero"
        );
        ensure!(
            self.buffer_count >= 2,
            "probe buffer count must be at least two"
        );
        ensure!(
            self.telemetry_interval > 0 && self.telemetry_interval <= u64::from(u32::MAX),
            "telemetry interval must be between 1 and {}",
            u32::MAX
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CadenceReport {
    pub submitted: u64,
    pub encoded: u64,
    pub wall_elapsed: Duration,
    pub source_write_average: Duration,
    pub source_write_max: Duration,
    pub encode_submit_average: Duration,
    pub encode_submit_max: Duration,
    pub deadline_misses: u64,
    pub deadline_miss_max: Duration,
    pub minimum_available_leases: usize,
}

impl fmt::Display for CadenceReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "surface_probe cadence submitted={} encoded={} wall_ms={} source_write_avg_us={} source_write_max_us={} encode_submit_avg_us={} encode_submit_max_us={} deadline_misses={} deadline_miss_max_us={} min_available_leases={}",
            self.submitted,
            self.encoded,
            self.wall_elapsed.as_millis(),
            self.source_write_average.as_micros(),
            self.source_write_max.as_micros(),
            self.encode_submit_average.as_micros(),
            self.encode_submit_max.as_micros(),
            self.deadline_misses,
            self.deadline_miss_max.as_micros(),
            self.minimum_available_leases,
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProbeSummary {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub requested_frames: u64,
    pub submitted_frames: u64,
    pub encoded_frames: u64,
    pub wall_elapsed: Duration,
    pub deadline_misses: u64,
    pub deadline_miss_max: Duration,
    pub pool_stats: PoolStats,
    pub hardware_support: HardwareEncoderSupport,
    pub connected_to_alvr: bool,
    pub last_video_timestamp: Duration,
    pub last_pose_timestamp: Duration,
}

impl fmt::Display for ProbeSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "surface_probe summary shape={}x{} fps={} requested={} submitted={} encoded={} wall_ms={} achieved_fps={:.3} hardware_hevc={} deadline_misses={} deadline_miss_max_us={} pool_available={}/{} leases_acquired={} leases_recycled={} alvr_connected={} last_video_timestamp_ns={} last_pose_timestamp_ns={}",
            self.width,
            self.height,
            self.fps,
            self.requested_frames,
            self.submitted_frames,
            self.encoded_frames,
            self.wall_elapsed.as_millis(),
            self.submitted_frames as f64 / self.wall_elapsed.as_secs_f64(),
            self.hardware_support.hardware_accelerated,
            self.deadline_misses,
            self.deadline_miss_max.as_micros(),
            self.pool_stats.available,
            self.pool_stats.capacity,
            self.pool_stats.acquired,
            self.pool_stats.recycled,
            self.connected_to_alvr,
            self.last_video_timestamp.as_nanos(),
            self.last_pose_timestamp.as_nanos(),
        )
    }
}

pub fn run_surface_probe(
    config: ProbeConfig,
    mut report: impl FnMut(CadenceReport),
) -> Result<ProbeSummary> {
    config.validate()?;
    let pool = SurfacePool::new(config.width, config.height, config.buffer_count)?;
    let (mut encoder, hardware_support) = NativeHevcEncoder::new(NativeHevcEncoderConfig {
        width: config.width,
        height: config.height,
        fps: config.fps,
        bitrate_bps: config.bitrate_bps,
    })?;
    let mut sink = config
        .connect_to_alvr
        .then(|| AlvrVideoSink::start(&config.alvr_root))
        .transpose()?;
    let fallback_view_params = default_stereo_view_params(config.width, config.height);
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let start = Instant::now();
    let mut cadence = CadenceAccumulator::new(config.telemetry_interval, config.buffer_count);
    let mut submitted = 0;
    let mut encoded = 0;
    let mut total_deadline_misses = 0;
    let mut total_deadline_miss_max = Duration::ZERO;
    let mut last_video_timestamp = Duration::ZERO;
    let mut last_pose_timestamp = Duration::ZERO;

    for frame_id in 0..config.frame_count {
        if sink.as_mut().is_some_and(|sink| {
            sink.poll_events();
            sink.shutdown_requested()
        }) {
            break;
        }

        let target = start + frame_interval.mul_f64(frame_id as f64);
        if let Some(sleep_duration) = target.checked_duration_since(Instant::now()) {
            thread::sleep(sleep_duration);
        }

        encoded += dispatch_outputs(encoder.drain_ready()?, &mut sink)?;
        let acquire_deadline = Instant::now() + Duration::from_secs(1);
        let mut lease = loop {
            if let Some(lease) = pool.try_acquire()? {
                break lease;
            }
            encoded += dispatch_outputs(encoder.drain_ready()?, &mut sink)?;
            ensure!(
                Instant::now() < acquire_deadline,
                "surface pool remained exhausted for one second with {} encoder frames pending",
                encoder.pending_count()
            );
            thread::sleep(Duration::from_micros(100));
        };
        cadence.observe_available(pool.stats().available);

        let source_start = Instant::now();
        lease.write_probe_marker(frame_id)?;
        let source_elapsed = source_start.elapsed();

        let video_timestamp = start.elapsed();
        let metadata = sink.as_mut().map_or(
            FrameMetadata {
                frame_id,
                video_timestamp,
                pose_timestamp: video_timestamp,
                global_view_params: fallback_view_params,
            },
            |sink| sink.frame_metadata(frame_id, video_timestamp, fallback_view_params),
        );
        last_video_timestamp = metadata.video_timestamp;
        last_pose_timestamp = metadata.pose_timestamp;
        let force_keyframe = frame_id % u64::from(config.fps) == 0
            || sink
                .as_mut()
                .is_some_and(AlvrVideoSink::take_force_keyframe);

        let encode_start = Instant::now();
        let outputs = encoder.submit(lease, metadata, force_keyframe)?;
        let encode_elapsed = encode_start.elapsed();
        submitted += 1;
        encoded += dispatch_outputs(outputs, &mut sink)?;

        let next_deadline = start + frame_interval.mul_f64(submitted as f64);
        let deadline_miss = Instant::now().checked_duration_since(next_deadline);
        if let Some(miss) = deadline_miss {
            total_deadline_misses += 1;
            total_deadline_miss_max = total_deadline_miss_max.max(miss);
        }
        if let Some(cadence_report) = cadence.record(
            submitted,
            encoded,
            start.elapsed(),
            source_elapsed,
            encode_elapsed,
            deadline_miss,
        ) {
            report(cadence_report);
        }
    }

    encoded += dispatch_outputs(encoder.finish()?, &mut sink)?;
    if let Some(cadence_report) = cadence.finish(submitted, encoded, start.elapsed()) {
        report(cadence_report);
    }

    let pool_stats = pool.stats();
    ensure!(
        encoded == submitted,
        "VideoToolbox emitted {encoded} frames for {submitted} submissions"
    );
    ensure!(
        pool_stats.available == pool_stats.capacity,
        "{} of {} surface leases were not recycled",
        pool_stats.capacity - pool_stats.available,
        pool_stats.capacity
    );
    ensure!(
        pool_stats.acquired == pool_stats.recycled,
        "surface lease accounting mismatch: acquired={} recycled={}",
        pool_stats.acquired,
        pool_stats.recycled
    );
    let connected_to_alvr = sink.as_mut().is_some_and(|sink| {
        sink.poll_events();
        sink.connected()
    });

    Ok(ProbeSummary {
        width: config.width,
        height: config.height,
        fps: config.fps,
        requested_frames: config.frame_count,
        submitted_frames: submitted,
        encoded_frames: encoded,
        wall_elapsed: start.elapsed(),
        deadline_misses: total_deadline_misses,
        deadline_miss_max: total_deadline_miss_max,
        pool_stats,
        hardware_support,
        connected_to_alvr,
        last_video_timestamp,
        last_pose_timestamp,
    })
}

fn dispatch_outputs(outputs: Vec<EncodedFrame>, sink: &mut Option<AlvrVideoSink>) -> Result<u64> {
    let count = outputs.len() as u64;
    if let Some(sink) = sink {
        for output in outputs {
            sink.send(output)?;
        }
    }
    Ok(count)
}

struct CadenceAccumulator {
    interval: u64,
    window_frames: u64,
    source_total: Duration,
    source_max: Duration,
    encode_total: Duration,
    encode_max: Duration,
    deadline_misses: u64,
    deadline_miss_max: Duration,
    minimum_available: usize,
    capacity: usize,
}

impl CadenceAccumulator {
    fn new(interval: u64, capacity: usize) -> Self {
        Self {
            interval,
            window_frames: 0,
            source_total: Duration::ZERO,
            source_max: Duration::ZERO,
            encode_total: Duration::ZERO,
            encode_max: Duration::ZERO,
            deadline_misses: 0,
            deadline_miss_max: Duration::ZERO,
            minimum_available: capacity,
            capacity,
        }
    }

    fn observe_available(&mut self, available: usize) {
        self.minimum_available = self.minimum_available.min(available);
    }

    fn record(
        &mut self,
        submitted: u64,
        encoded: u64,
        wall_elapsed: Duration,
        source_elapsed: Duration,
        encode_elapsed: Duration,
        deadline_miss: Option<Duration>,
    ) -> Option<CadenceReport> {
        self.window_frames += 1;
        self.source_total += source_elapsed;
        self.source_max = self.source_max.max(source_elapsed);
        self.encode_total += encode_elapsed;
        self.encode_max = self.encode_max.max(encode_elapsed);
        if let Some(miss) = deadline_miss {
            self.deadline_misses += 1;
            self.deadline_miss_max = self.deadline_miss_max.max(miss);
        }

        (self.window_frames == self.interval)
            .then(|| self.take_report(submitted, encoded, wall_elapsed))
    }

    fn finish(
        &mut self,
        submitted: u64,
        encoded: u64,
        wall_elapsed: Duration,
    ) -> Option<CadenceReport> {
        (self.window_frames > 0).then(|| self.take_report(submitted, encoded, wall_elapsed))
    }

    fn take_report(
        &mut self,
        submitted: u64,
        encoded: u64,
        wall_elapsed: Duration,
    ) -> CadenceReport {
        let window_frames = self.window_frames;
        let report = CadenceReport {
            submitted,
            encoded,
            wall_elapsed,
            source_write_average: self.source_total / window_frames as u32,
            source_write_max: self.source_max,
            encode_submit_average: self.encode_total / window_frames as u32,
            encode_submit_max: self.encode_max,
            deadline_misses: self.deadline_misses,
            deadline_miss_max: self.deadline_miss_max,
            minimum_available_leases: self.minimum_available,
        };
        self.window_frames = 0;
        self.source_total = Duration::ZERO;
        self.source_max = Duration::ZERO;
        self.encode_total = Duration::ZERO;
        self.encode_max = Duration::ZERO;
        self.deadline_misses = 0;
        self.deadline_miss_max = Duration::ZERO;
        self.minimum_available = self.capacity;
        report
    }
}

fn default_stereo_view_params(width: u32, height: u32) -> [ViewParams; 2] {
    let eye_width = width / 2;
    let horizontal_half_fov = std::f32::consts::FRAC_PI_4;
    let vertical_half_fov = (horizontal_half_fov.tan() * height as f32 / eye_width as f32).atan();
    let fov = Fov {
        left: -horizontal_half_fov,
        right: horizontal_half_fov,
        up: vertical_half_fov,
        down: -vertical_half_fov,
    };
    [
        ViewParams {
            pose: Pose {
                orientation: alvr_common::glam::Quat::IDENTITY,
                position: Vec3::new(-0.032, 0.0, 0.0),
            },
            fov,
        },
        ViewParams {
            pose: Pose {
                orientation: alvr_common::glam::Quat::IDENTITY,
                position: Vec3::new(0.032, 0.0, 0.0),
            },
            fov,
        },
    ]
}

fn env_u32(name: &str, default: u32) -> Result<u32> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

fn env_usize(name: &str, default: usize) -> Result<usize> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    env::var(name)
        .map(|value| match value.as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            _ => anyhow::bail!("invalid {name}: expected 0/1, false/true, or no/yes"),
        })
        .unwrap_or(Ok(default))
}
