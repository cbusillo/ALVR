#[cfg(target_os = "macos")]
mod bgra;
#[cfg(target_os = "macos")]
mod encoder;
#[cfg(target_os = "macos")]
mod iosurface_3d;
#[cfg(target_os = "macos")]
mod iosurface_synthetic;
#[cfg(target_os = "macos")]
mod shared_memory;

#[cfg(target_os = "macos")]
mod synthetic;

#[cfg(target_os = "macos")]
use std::{
    collections::VecDeque,
    env, fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use alvr_common::{
    Fov, HEAD_ID, Pose, ViewParams,
    glam::{Quat, Vec3},
};
#[cfg(target_os = "macos")]
use alvr_server_core::{ServerCoreContext, ServerCoreEvent};
#[cfg(target_os = "macos")]
use alvr_session::CodecType;
#[cfg(target_os = "macos")]
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use bgra::{Nv12Frame, Nv12PixelBuffer, fill_bgra_test_pattern};
#[cfg(target_os = "macos")]
use encoder::{EncodedOutput, HevcEncoder};
#[cfg(target_os = "macos")]
use iosurface_3d::Iosurface3dFrameSource;
#[cfg(target_os = "macos")]
use iosurface_synthetic::IosurfaceSyntheticFrameSource;
#[cfg(target_os = "macos")]
use shared_memory::{FORMAT_BGRA, SharedMemory, unix_time_ns, valid_view_params};
#[cfg(target_os = "macos")]
use synthetic::SyntheticFrameSource;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("alvr_macos_bridge is only supported on macOS");
}

#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    let config = BridgeConfig::from_env()?;

    if config.input == BridgeInput::SharedMemoryWriter {
        return run_shared_memory_writer(&config);
    }

    fs::create_dir_all(&config.root)
        .with_context(|| format!("failed to create {}", config.root.display()))?;

    let layout = alvr_filesystem::Layout::new(&config.root);
    alvr_server_core::initialize_environment(layout.clone());
    alvr_server_core::init_logging_headless(Some(layout.session_log()), Some(layout.crash_log()));

    log::info!("starting ALVR macOS bridge with {config:?}");

    let (server_context, events_receiver) = ServerCoreContext::new();
    let server_context = Arc::new(server_context);

    let shared_memory = if config.input == BridgeInput::SharedMemory {
        Some(Arc::new(Mutex::new(SharedMemory::create()?)))
    } else {
        None
    };

    let force_idr = Arc::new(AtomicBool::new(true));
    let shutdown = Arc::new(AtomicBool::new(false));
    let client_view_params = Arc::new(Mutex::new(bootstrap_view_params()?));
    let latest_tracking = Arc::new(Mutex::new(None));
    thread::spawn({
        let force_idr = Arc::clone(&force_idr);
        let shutdown = Arc::clone(&shutdown);
        let client_view_params = Arc::clone(&client_view_params);
        let latest_tracking = Arc::clone(&latest_tracking);
        let shared_memory = shared_memory.as_ref().map(Arc::clone);
        let server_context = Arc::clone(&server_context);
        move || {
            event_loop(
                server_context,
                events_receiver,
                force_idr,
                shutdown,
                client_view_params,
                latest_tracking,
                shared_memory,
            )
        }
    });

    let (mut frame_source, stream_shape) = FrameSource::new(
        &config,
        Arc::clone(&client_view_params),
        Arc::clone(&latest_tracking),
        shared_memory,
    )?;
    let fallback_view_params =
        default_stereo_view_params(stream_shape.width, stream_shape.height, 0.0);
    let mut encoder = HevcEncoder::new(
        stream_shape.width,
        stream_shape.height,
        config.bitrate_bps,
        config.fps,
    )?;
    let pace_output = config.input != BridgeInput::SharedMemory;
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let start = Instant::now();
    let mut frame_index = 0_u64;
    let mut source_frame_index = 0_u64;
    let mut empty_output_count = 0_u32;
    let mut logged_frame_view_params = false;
    let mut logged_global_view_params = false;
    let mut cadence_stats = CadenceStats::new(config.fps);
    let mut pending_frame_metadata = VecDeque::new();

    log::info!("frame source and encoder ready; starting ALVR client connection");
    server_context.start_connection();

    while !shutdown.load(Ordering::SeqCst)
        && config
            .frame_count
            .is_none_or(|frame_count| source_frame_index < frame_count)
    {
        let force_keyframe =
            frame_index % u64::from(config.fps) == 0 || force_idr.swap(false, Ordering::SeqCst);
        let source_start = Instant::now();
        let Some(frame) = frame_source.next_frame(frame_index, start.elapsed())? else {
            if frame_source.is_finished() {
                log::info!("frame source finished after {source_frame_index} input frames");
                break;
            }
            let drained_outputs = drain_ready_outputs(
                &mut frame_source,
                &mut pending_frame_metadata,
                &server_context,
                &mut encoder,
                start.elapsed(),
                fallback_view_params,
            )?;
            if drained_outputs > 0 {
                empty_output_count = 0;
                cadence_stats.record_emitted(drained_outputs);
                continue;
            }
            thread::sleep(Duration::from_micros(500));
            continue;
        };
        let source_elapsed = source_start.elapsed();
        source_frame_index += 1;
        let (pixel_buffer, input_idr, frame_metadata) = frame.into_encoder_parts(
            &client_view_params,
            fallback_view_params,
            stream_shape.width,
            config.right_eye_shift_x_px,
            &mut logged_frame_view_params,
            &mut logged_global_view_params,
        );
        pending_frame_metadata.push_back(frame_metadata);

        let encode_start = Instant::now();
        let output = encoder.encode_pixel_buffer(&pixel_buffer, force_keyframe || input_idr)?;
        let encode_elapsed = encode_start.elapsed();

        let emitted_outputs = if let Some(output) = output {
            empty_output_count = 0;
            send_pending_encoded_output(
                &mut frame_source,
                &mut pending_frame_metadata,
                &server_context,
                &mut encoder,
                output,
                start.elapsed(),
                fallback_view_params,
            );
            1 + drain_ready_outputs(
                &mut frame_source,
                &mut pending_frame_metadata,
                &server_context,
                &mut encoder,
                start.elapsed(),
                fallback_view_params,
            )?
        } else {
            empty_output_count += 1;
            if empty_output_count == config.fps * 2 {
                log::warn!(
                    "VideoToolbox has not produced output for {empty_output_count} submitted frames"
                );
                empty_output_count = 0;
            }
            0
        };
        cadence_stats.record(source_elapsed, encode_elapsed, emitted_outputs);

        frame_index += 1;
        if pace_output {
            let next_frame_deadline = start + frame_interval.mul_f64(frame_index as f64);
            if let Some(sleep_duration) = next_frame_deadline.checked_duration_since(Instant::now())
            {
                thread::sleep(sleep_duration);
            } else {
                cadence_stats.record_deadline_miss(Instant::now() - next_frame_deadline);
            }
        }
    }

    for output in encoder.finish()? {
        send_pending_encoded_output(
            &mut frame_source,
            &mut pending_frame_metadata,
            &server_context,
            &mut encoder,
            output,
            start.elapsed(),
            fallback_view_params,
        );
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn send_pending_encoded_output(
    frame_source: &mut FrameSource,
    pending_frame_metadata: &mut VecDeque<EncodedFrameMetadata>,
    server_context: &ServerCoreContext,
    encoder: &mut HevcEncoder,
    output: EncodedOutput,
    fallback_timestamp: Duration,
    fallback_view_params: [ViewParams; 2],
) {
    let mut metadata = pending_frame_metadata.pop_front().unwrap_or_else(|| {
        EncodedFrameMetadata::fallback(fallback_timestamp, fallback_view_params)
    });
    if let Some(buffer) = metadata.recycle_buffer.take() {
        frame_source.recycle_buffer(buffer);
    }
    send_encoded_output(
        server_context,
        encoder,
        metadata.timestamp,
        output,
        metadata.view_params,
    );
}

#[cfg(target_os = "macos")]
fn drain_ready_outputs(
    frame_source: &mut FrameSource,
    pending_frame_metadata: &mut VecDeque<EncodedFrameMetadata>,
    server_context: &ServerCoreContext,
    encoder: &mut HevcEncoder,
    fallback_timestamp: Duration,
    fallback_view_params: [ViewParams; 2],
) -> Result<u64> {
    let mut count = 0;
    while let Some(output) = encoder.drain_output()? {
        count += 1;
        send_pending_encoded_output(
            frame_source,
            pending_frame_metadata,
            server_context,
            encoder,
            output,
            fallback_timestamp,
            fallback_view_params,
        );
    }
    Ok(count)
}

#[cfg(target_os = "macos")]
fn send_encoded_output(
    server_context: &ServerCoreContext,
    encoder: &mut HevcEncoder,
    timestamp: Duration,
    output: EncodedOutput,
    view_params: [ViewParams; 2],
) {
    if let Some(config_nals) = output.config_nals
        && !encoder.config_sent()
    {
        log::info!("sending HEVC decoder config ({} bytes)", config_nals.len());
        server_context.set_video_config_nals(config_nals, CodecType::Hevc);
        encoder.mark_config_sent();
    }

    if output.is_keyframe {
        log_view_params_contract("encoded frame contract view_params", timestamp, view_params);
    }

    server_context.send_video_nal(timestamp, view_params, output.is_keyframe, output.nal_data);
}

#[cfg(target_os = "macos")]
fn log_view_params_contract(label: &str, timestamp: Duration, view_params: [ViewParams; 2]) {
    log::info!(
        "{label} timestamp_ns={} pose_space=resolved-for-send left_fov=[{:.6} {:.6} {:.6} {:.6}] right_fov=[{:.6} {:.6} {:.6} {:.6}] left_pose_pos=[{:.6} {:.6} {:.6}] left_pose_orientation_xyzw=[{:.6} {:.6} {:.6} {:.6}] right_pose_pos=[{:.6} {:.6} {:.6}] right_pose_orientation_xyzw=[{:.6} {:.6} {:.6} {:.6}]",
        timestamp.as_nanos(),
        view_params[0].fov.left,
        view_params[0].fov.right,
        view_params[0].fov.up,
        view_params[0].fov.down,
        view_params[1].fov.left,
        view_params[1].fov.right,
        view_params[1].fov.up,
        view_params[1].fov.down,
        view_params[0].pose.position.x,
        view_params[0].pose.position.y,
        view_params[0].pose.position.z,
        view_params[0].pose.orientation.x,
        view_params[0].pose.orientation.y,
        view_params[0].pose.orientation.z,
        view_params[0].pose.orientation.w,
        view_params[1].pose.position.x,
        view_params[1].pose.position.y,
        view_params[1].pose.position.z,
        view_params[1].pose.orientation.x,
        view_params[1].pose.orientation.y,
        view_params[1].pose.orientation.z,
        view_params[1].pose.orientation.w,
    );
}

#[cfg(target_os = "macos")]
struct CadenceStats {
    interval: u64,
    submitted: u64,
    emitted: u64,
    source_total: Duration,
    source_max: Duration,
    encode_total: Duration,
    encode_max: Duration,
    wall_start: Instant,
    deadline_miss_count: u64,
    deadline_miss_max: Duration,
}

#[cfg(target_os = "macos")]
impl CadenceStats {
    fn new(fps: u32) -> Self {
        Self {
            interval: u64::from(fps).max(1),
            submitted: 0,
            emitted: 0,
            source_total: Duration::ZERO,
            source_max: Duration::ZERO,
            encode_total: Duration::ZERO,
            encode_max: Duration::ZERO,
            wall_start: Instant::now(),
            deadline_miss_count: 0,
            deadline_miss_max: Duration::ZERO,
        }
    }

    fn record_deadline_miss(&mut self, miss: Duration) {
        self.deadline_miss_count += 1;
        self.deadline_miss_max = self.deadline_miss_max.max(miss);
    }

    fn record_emitted(&mut self, emitted: u64) {
        self.emitted += emitted;
    }

    fn record(&mut self, source_elapsed: Duration, encode_elapsed: Duration, emitted: u64) {
        self.submitted += 1;
        self.emitted += emitted;
        self.source_total += source_elapsed;
        self.source_max = self.source_max.max(source_elapsed);
        self.encode_total += encode_elapsed;
        self.encode_max = self.encode_max.max(encode_elapsed);

        if self.submitted % self.interval == 0 {
            let source_avg_us = self.source_total.as_micros() / u128::from(self.interval);
            let encode_avg_us = self.encode_total.as_micros() / u128::from(self.interval);
            log::info!(
                "bridge cadence frames={} emitted={} wall_ms={} timing_us source_avg={} source_max={} encode_avg={} encode_max={} deadline_misses={} deadline_miss_max_us={}",
                self.submitted,
                self.emitted,
                self.wall_start.elapsed().as_millis(),
                source_avg_us,
                self.source_max.as_micros(),
                encode_avg_us,
                self.encode_max.as_micros(),
                self.deadline_miss_count,
                self.deadline_miss_max.as_micros()
            );
            self.source_total = Duration::ZERO;
            self.source_max = Duration::ZERO;
            self.encode_total = Duration::ZERO;
            self.encode_max = Duration::ZERO;
            self.deadline_miss_count = 0;
            self.deadline_miss_max = Duration::ZERO;
        }
    }
}

#[cfg(target_os = "macos")]
fn default_stereo_view_params(
    width: u32,
    height: u32,
    right_eye_shift_x_px: f32,
) -> [ViewParams; 2] {
    let eye_width = width / 2;
    let horizontal_half_fov = std::f32::consts::FRAC_PI_4;
    let vertical_half_fov = (horizontal_half_fov.tan() * height as f32 / eye_width as f32).atan();
    let fov = Fov {
        left: -horizontal_half_fov,
        right: horizontal_half_fov,
        up: vertical_half_fov,
        down: -vertical_half_fov,
    };
    let right_fov = horizontal_shifted_fov(fov, eye_width, right_eye_shift_x_px);
    let half_ipd_m = 0.032;

    [
        ViewParams {
            pose: Pose {
                orientation: Quat::IDENTITY,
                position: Vec3::new(-half_ipd_m, 0.0, 0.0),
            },
            fov,
        },
        ViewParams {
            pose: Pose {
                orientation: Quat::IDENTITY,
                position: Vec3::new(half_ipd_m, 0.0, 0.0),
            },
            fov: right_fov,
        },
    ]
}

#[cfg(target_os = "macos")]
fn bootstrap_view_params() -> Result<Option<[ViewParams; 2]>> {
    let Some(raw_fov) = env::var_os("ALVR_BRIDGE_BOOTSTRAP_VIEW_FOV") else {
        return Ok(None);
    };
    let raw_fov = raw_fov.to_string_lossy();
    let mut values = Vec::new();
    for value in raw_fov.split(',') {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        values.push(
            value.parse::<f32>().with_context(|| {
                format!("invalid ALVR_BRIDGE_BOOTSTRAP_VIEW_FOV value: {value}")
            })?,
        );
    }
    anyhow::ensure!(
        values.len() == 8,
        "ALVR_BRIDGE_BOOTSTRAP_VIEW_FOV must contain 8 comma-separated values: left.l,left.r,left.u,left.d,right.l,right.r,right.u,right.d"
    );

    let mut eye_x_m = [-0.032_f32, 0.032_f32];
    if let Some(raw_eye_x) = env::var_os("ALVR_BRIDGE_BOOTSTRAP_EYE_X_M") {
        let raw_eye_x = raw_eye_x.to_string_lossy();
        let parsed = raw_eye_x
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value.parse::<f32>().with_context(|| {
                    format!("invalid ALVR_BRIDGE_BOOTSTRAP_EYE_X_M value: {value}")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(
            parsed.len() == 2,
            "ALVR_BRIDGE_BOOTSTRAP_EYE_X_M must contain 2 comma-separated values: left,right"
        );
        eye_x_m = [parsed[0], parsed[1]];
    }

    let params = [
        ViewParams {
            pose: Pose {
                orientation: Quat::IDENTITY,
                position: Vec3::new(eye_x_m[0], 0.0, 0.0),
            },
            fov: Fov {
                left: values[0],
                right: values[1],
                up: values[2],
                down: values[3],
            },
        },
        ViewParams {
            pose: Pose {
                orientation: Quat::IDENTITY,
                position: Vec3::new(eye_x_m[1], 0.0, 0.0),
            },
            fov: Fov {
                left: values[4],
                right: values[5],
                up: values[6],
                down: values[7],
            },
        },
    ];

    anyhow::ensure!(
        valid_view_params(params),
        "ALVR_BRIDGE_BOOTSTRAP_VIEW_FOV/ALVR_BRIDGE_BOOTSTRAP_EYE_X_M did not produce valid view params"
    );
    log::info!("using bootstrap shared-memory view params from environment");
    Ok(Some(params))
}

#[cfg(target_os = "macos")]
fn horizontal_shifted_fov(fov: Fov, eye_width: u32, shift_x_px: f32) -> Fov {
    if shift_x_px == 0.0 {
        return fov;
    }

    let left_tan = fov.left.tan();
    let right_tan = fov.right.tan();
    let tan_shift = (shift_x_px / eye_width as f32) * (right_tan - left_tan);

    Fov {
        left: (left_tan + tan_shift).atan(),
        right: (right_tan + tan_shift).atan(),
        up: fov.up,
        down: fov.down,
    }
}

#[cfg(target_os = "macos")]
fn horizontally_shifted_view_params(
    mut view_params: [ViewParams; 2],
    eye_width: u32,
    right_eye_shift_x_px: f32,
) -> [ViewParams; 2] {
    if right_eye_shift_x_px != 0.0 {
        view_params[1].fov =
            horizontal_shifted_fov(view_params[1].fov, eye_width, right_eye_shift_x_px);
    }

    view_params
}

#[cfg(target_os = "macos")]
fn apply_diagnostic_eye_offset(view_params: &mut [ViewParams; 2], eye_offset_m: f32) {
    let half_offset = eye_offset_m * 0.5;
    view_params[0].pose.position.x = -half_offset;
    view_params[1].pose.position.x = half_offset;
}

#[cfg(target_os = "macos")]
fn global_view_params(hmd_pose: Pose, local_view_params: [ViewParams; 2]) -> [ViewParams; 2] {
    local_view_params.map(|params| ViewParams {
        pose: hmd_pose * params.pose,
        fov: params.fov,
    })
}

#[cfg(target_os = "macos")]
fn event_loop(
    server_context: Arc<ServerCoreContext>,
    events_receiver: mpsc::Receiver<ServerCoreEvent>,
    force_idr: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
    latest_tracking: Arc<Mutex<Option<(Duration, Pose)>>>,
    shared_memory: Option<Arc<Mutex<SharedMemory>>>,
) {
    let mut logged_client_view_params = false;
    let mut logged_hmd_pose = false;
    while let Ok(event) = events_receiver.recv() {
        match event {
            ServerCoreEvent::ClientConnected(_) => {
                log::info!("client connected; requesting immediate IDR");
                force_idr.store(true, Ordering::SeqCst);
            }
            ServerCoreEvent::ClientDisconnected => log::info!("client disconnected"),
            ServerCoreEvent::RequestIDR => {
                log::info!("client requested IDR");
                force_idr.store(true, Ordering::SeqCst);
            }
            ServerCoreEvent::ShutdownPending => {
                log::info!("shutdown requested");
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
            ServerCoreEvent::RestartPending => {
                log::info!("restart requested");
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
            ServerCoreEvent::LocalViewParams(params) => {
                if !logged_client_view_params {
                    log::info!(
                        "using client local view params left_fov=[{:.4} {:.4} {:.4} {:.4}] right_fov=[{:.4} {:.4} {:.4} {:.4}] eye_x=[{:.4} {:.4}]",
                        params[0].fov.left,
                        params[0].fov.right,
                        params[0].fov.up,
                        params[0].fov.down,
                        params[1].fov.left,
                        params[1].fov.right,
                        params[1].fov.up,
                        params[1].fov.down,
                        params[0].pose.position.x,
                        params[1].pose.position.x,
                    );
                    logged_client_view_params = true;
                }
                *client_view_params
                    .lock()
                    .expect("client view params mutex poisoned") = Some(params);
            }
            ServerCoreEvent::Tracking { poll_timestamp } => {
                if let Some(motion) = server_context.get_device_motion(*HEAD_ID, poll_timestamp) {
                    *latest_tracking
                        .lock()
                        .expect("latest tracking mutex poisoned") =
                        Some((poll_timestamp, motion.pose));
                    if let Some(shared_memory) = &shared_memory {
                        if shared_memory
                            .lock()
                            .expect("shared memory mutex poisoned")
                            .publish_hmd_pose(poll_timestamp, motion.pose)
                            && !logged_hmd_pose
                        {
                            log::info!("publishing AVP HMD pose to shared memory");
                            logged_hmd_pose = true;
                        }
                    }
                }
            }
            ServerCoreEvent::Buttons(_) => {}
            ServerCoreEvent::Battery(_) => {}
            ServerCoreEvent::PlayspaceSync(_) => {}
            ServerCoreEvent::SetOpenvrProperty { .. } => {}
            ServerCoreEvent::CaptureFrame => {}
            ServerCoreEvent::GameRenderLatencyFeedback(_) => {}
            ServerCoreEvent::ProximityState(_) => {}
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct BridgeConfig {
    root: PathBuf,
    input: BridgeInput,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    frame_count: Option<u64>,
    right_eye_shift_x_px: f32,
    diagnostic_eye_offset_m: Option<f32>,
    diagnostic_forward_z_sign: f32,
    pattern_right_eye_shift_x_px: i32,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeInput {
    Synthetic,
    Iosurface3d,
    IosurfaceReprojection,
    IosurfaceSynthetic,
    SharedMemory,
    SharedMemoryWriter,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
struct StreamShape {
    width: u32,
    height: u32,
}

#[cfg(target_os = "macos")]
struct BridgeFrame {
    pixel_buffer: Nv12PixelBuffer,
    recycle_buffer: Option<Nv12PixelBuffer>,
    timestamp: Duration,
    hmd_pose: Option<Pose>,
    input_idr: bool,
    view_params: Option<[ViewParams; 2]>,
}

#[cfg(target_os = "macos")]
struct EncodedFrameMetadata {
    timestamp: Duration,
    view_params: [ViewParams; 2],
    recycle_buffer: Option<Nv12PixelBuffer>,
}

#[cfg(target_os = "macos")]
impl EncodedFrameMetadata {
    fn fallback(timestamp: Duration, view_params: [ViewParams; 2]) -> Self {
        Self {
            timestamp,
            view_params,
            recycle_buffer: None,
        }
    }
}

#[cfg(target_os = "macos")]
impl BridgeFrame {
    fn into_encoder_parts(
        self,
        client_view_params: &Arc<Mutex<Option<[ViewParams; 2]>>>,
        fallback_view_params: [ViewParams; 2],
        stream_width: u32,
        right_eye_shift_x_px: f32,
        logged_frame_view_params: &mut bool,
        logged_global_view_params: &mut bool,
    ) -> (Nv12PixelBuffer, bool, EncodedFrameMetadata) {
        if self.view_params.is_some() && !*logged_frame_view_params {
            log::info!("using frame-local view params for encoded frames");
            *logged_frame_view_params = true;
        }
        if self.hmd_pose.is_some() && !*logged_global_view_params {
            log::info!("using frame-local HMD pose/timestamp for encoded frames");
        }

        let mut view_params = self
            .view_params
            .or_else(|| {
                *client_view_params
                    .lock()
                    .expect("client view params mutex poisoned")
            })
            .unwrap_or(fallback_view_params);
        view_params =
            horizontally_shifted_view_params(view_params, stream_width / 2, right_eye_shift_x_px);

        if let Some(hmd_pose) = self.hmd_pose {
            view_params = global_view_params(hmd_pose, view_params);
            if !*logged_global_view_params {
                log::info!("using AVP HMD pose composed with local eye params for encoded frames");
                *logged_global_view_params = true;
            }
        }

        let metadata = EncodedFrameMetadata {
            timestamp: self.timestamp,
            view_params,
            recycle_buffer: self.recycle_buffer,
        };

        (self.pixel_buffer, self.input_idr, metadata)
    }
}

#[cfg(target_os = "macos")]
impl BridgeConfig {
    fn from_env() -> Result<Self> {
        let root = env::var_os("ALVR_BRIDGE_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join("Library/Application Support/ALVR/macos_bridge"))
            })
            .context("set ALVR_BRIDGE_ROOT or HOME")?;

        let input = env_input()?;
        let width = env_u32("ALVR_BRIDGE_WIDTH", 2560)?;
        let height = env_u32("ALVR_BRIDGE_HEIGHT", 720)?;
        let fps = env_u32("ALVR_BRIDGE_FPS", 60)?;

        anyhow::ensure!(
            width > 0 && width % 4 == 0,
            "ALVR_BRIDGE_WIDTH must be divisible by 4 for side-by-side NV12"
        );
        anyhow::ensure!(
            width >= 640,
            "ALVR_BRIDGE_WIDTH must be at least 640 for the synthetic pattern"
        );
        anyhow::ensure!(
            height > 0 && height % 2 == 0,
            "ALVR_BRIDGE_HEIGHT must be even"
        );
        anyhow::ensure!(fps > 0, "ALVR_BRIDGE_FPS must be greater than 0");

        let diagnostic_eye_offset_m = if matches!(
            input,
            BridgeInput::Iosurface3d | BridgeInput::IosurfaceReprojection
        ) {
            env_optional_f32("ALVR_BRIDGE_DIAGNOSTIC_EYE_OFFSET_M")?
        } else {
            if env::var_os("ALVR_BRIDGE_DIAGNOSTIC_EYE_OFFSET_M").is_some() {
                log::warn!(
                    "ignoring ALVR_BRIDGE_DIAGNOSTIC_EYE_OFFSET_M because input is not iosurface-3d or iosurface-reprojection"
                );
            }
            None
        };

        Ok(Self {
            root,
            input,
            width,
            height,
            fps,
            bitrate_bps: env_u64("ALVR_BRIDGE_BITRATE_BPS", 20_000_000)?,
            frame_count: env_optional_u64("ALVR_BRIDGE_FRAMES")?,
            right_eye_shift_x_px: env_f32("ALVR_BRIDGE_RIGHT_EYE_SHIFT_X_PX", 0.0)?,
            diagnostic_eye_offset_m,
            diagnostic_forward_z_sign: env_f32("ALVR_BRIDGE_DIAGNOSTIC_FORWARD_Z_SIGN", -1.0)?,
            pattern_right_eye_shift_x_px: env_i32("ALVR_BRIDGE_PATTERN_RIGHT_EYE_SHIFT_X_PX", 0)?,
        })
    }
}

#[cfg(target_os = "macos")]
enum FrameSource {
    Synthetic {
        source: SyntheticFrameSource,
        converter: Nv12Frame,
    },
    IosurfaceSynthetic {
        source: IosurfaceSyntheticFrameSource,
    },
    IosurfaceReprojection {
        source: IosurfaceSyntheticFrameSource,
        client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
        latest_tracking: Arc<Mutex<Option<(Duration, Pose)>>>,
        fallback_view_params: [ViewParams; 2],
        diagnostic_eye_offset_m: Option<f32>,
        logged_diagnostic_eye_offset: bool,
        logged_tracking: bool,
    },
    Iosurface3d {
        source: Iosurface3dFrameSource,
        client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
        latest_tracking: Arc<Mutex<Option<(Duration, Pose)>>>,
        fallback_view_params: [ViewParams; 2],
        right_eye_shift_x_px: f32,
        diagnostic_eye_offset_m: Option<f32>,
        diagnostic_forward_z_sign: f32,
        logged_diagnostic_eye_offset: bool,
    },
    SharedMemory {
        shm: Arc<Mutex<SharedMemory>>,
        converter: Nv12Frame,
        expected_width: u32,
        expected_height: u32,
        last_log: Instant,
        client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
        latest_tracking: Arc<Mutex<Option<(Duration, Pose)>>>,
        logged_tracking_timestamp: bool,
    },
}

#[cfg(target_os = "macos")]
impl FrameSource {
    fn new(
        config: &BridgeConfig,
        client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
        latest_tracking: Arc<Mutex<Option<(Duration, Pose)>>>,
        shared_memory: Option<Arc<Mutex<SharedMemory>>>,
    ) -> Result<(Self, StreamShape)> {
        match config.input {
            BridgeInput::Synthetic => Ok((
                Self::Synthetic {
                    source: SyntheticFrameSource::new(config.width, config.height),
                    converter: Nv12Frame::new(config.width, config.height)?,
                },
                StreamShape {
                    width: config.width,
                    height: config.height,
                },
            )),
            BridgeInput::IosurfaceSynthetic => Ok((
                Self::IosurfaceSynthetic {
                    source: IosurfaceSyntheticFrameSource::new(
                        config.width,
                        config.height,
                        config.pattern_right_eye_shift_x_px,
                    )?,
                },
                StreamShape {
                    width: config.width,
                    height: config.height,
                },
            )),
            BridgeInput::IosurfaceReprojection => Ok((
                Self::IosurfaceReprojection {
                    source: IosurfaceSyntheticFrameSource::new(config.width, config.height, 0)?,
                    client_view_params,
                    latest_tracking,
                    fallback_view_params: default_stereo_view_params(
                        config.width,
                        config.height,
                        0.0,
                    ),
                    diagnostic_eye_offset_m: config.diagnostic_eye_offset_m,
                    logged_diagnostic_eye_offset: false,
                    logged_tracking: false,
                },
                StreamShape {
                    width: config.width,
                    height: config.height,
                },
            )),
            BridgeInput::Iosurface3d => Ok((
                Self::Iosurface3d {
                    source: Iosurface3dFrameSource::new(config.width, config.height)?,
                    client_view_params,
                    latest_tracking,
                    fallback_view_params: default_stereo_view_params(
                        config.width,
                        config.height,
                        0.0,
                    ),
                    right_eye_shift_x_px: config.right_eye_shift_x_px,
                    diagnostic_eye_offset_m: config.diagnostic_eye_offset_m,
                    diagnostic_forward_z_sign: config.diagnostic_forward_z_sign,
                    logged_diagnostic_eye_offset: false,
                },
                StreamShape {
                    width: config.width,
                    height: config.height,
                },
            )),
            BridgeInput::SharedMemory => {
                let shm = shared_memory.context("shared-memory input requires shared mapping")?;
                log::info!("waiting for shared-memory producer config");
                let (width, height, format) = wait_for_shared_memory_config(
                    &mut shm.lock().expect("shared memory mutex poisoned"),
                    &client_view_params,
                )?;
                log::info!("shared memory configured: {width}x{height} format=0x{format:x}");

                Ok((
                    Self::SharedMemory {
                        shm,
                        converter: Nv12Frame::new(width, height)?,
                        expected_width: width,
                        expected_height: height,
                        last_log: Instant::now(),
                        client_view_params,
                        latest_tracking,
                        logged_tracking_timestamp: false,
                    },
                    StreamShape { width, height },
                ))
            }
            BridgeInput::SharedMemoryWriter => unreachable!(),
        }
    }

    fn next_frame(
        &mut self,
        frame_index: u64,
        fallback_timestamp: Duration,
    ) -> Result<Option<BridgeFrame>> {
        match self {
            Self::Synthetic { source, converter } => {
                let (y, uv) = source.frame(frame_index);
                Ok(Some(BridgeFrame {
                    pixel_buffer: converter.pixel_buffer_from_nv12_planes(y, uv)?,
                    recycle_buffer: None,
                    timestamp: fallback_timestamp,
                    hmd_pose: None,
                    input_idr: false,
                    view_params: None,
                }))
            }
            Self::IosurfaceSynthetic { source } => {
                let Some(frame) = source.frame(frame_index)? else {
                    return Ok(None);
                };
                if frame_index < 10 || frame_index % 120 == 0 {
                    log::info!(
                        "filled IOSurface synthetic frame {frame_index} timing_us fill={}",
                        frame.fill_elapsed.as_micros()
                    );
                }
                Ok(Some(BridgeFrame {
                    pixel_buffer: frame.pixel_buffer,
                    recycle_buffer: Some(frame.recycle_buffer),
                    timestamp: fallback_timestamp,
                    hmd_pose: None,
                    input_idr: false,
                    view_params: None,
                }))
            }
            Self::IosurfaceReprojection {
                source,
                client_view_params,
                latest_tracking,
                fallback_view_params,
                diagnostic_eye_offset_m,
                logged_diagnostic_eye_offset,
                logged_tracking,
            } => {
                let Some(frame) = source.frame(frame_index)? else {
                    return Ok(None);
                };
                let mut local_view_params = client_view_params
                    .lock()
                    .expect("client view params mutex poisoned")
                    .unwrap_or(*fallback_view_params);
                if let Some(eye_offset_m) = *diagnostic_eye_offset_m {
                    apply_diagnostic_eye_offset(&mut local_view_params, eye_offset_m);
                    if !*logged_diagnostic_eye_offset {
                        log::info!(
                            "using diagnostic reprojection eye offset {:.4}m for iosurface-reprojection view params",
                            eye_offset_m
                        );
                        *logged_diagnostic_eye_offset = true;
                    }
                }
                let tracking = *latest_tracking
                    .lock()
                    .expect("latest tracking mutex poisoned");
                let (timestamp, hmd_pose, has_tracking) = tracking
                    .map(|(timestamp, pose)| (timestamp, pose, true))
                    .unwrap_or((fallback_timestamp, Pose::IDENTITY, false));
                if has_tracking && !*logged_tracking {
                    log::info!("using AVP HMD pose/timestamp for iosurface-reprojection frames");
                    *logged_tracking = true;
                }
                if frame_index < 10 || frame_index % 120 == 0 {
                    log::info!(
                        "filled IOSurface reprojection frame {frame_index} timing_us fill={} tracking={has_tracking}",
                        frame.fill_elapsed.as_micros()
                    );
                }
                Ok(Some(BridgeFrame {
                    pixel_buffer: frame.pixel_buffer,
                    recycle_buffer: Some(frame.recycle_buffer),
                    timestamp,
                    hmd_pose: has_tracking.then_some(hmd_pose),
                    input_idr: false,
                    view_params: Some(local_view_params),
                }))
            }
            Self::Iosurface3d {
                source,
                client_view_params,
                latest_tracking,
                fallback_view_params,
                right_eye_shift_x_px,
                diagnostic_eye_offset_m,
                diagnostic_forward_z_sign,
                logged_diagnostic_eye_offset,
            } => {
                let mut local_view_params = client_view_params
                    .lock()
                    .expect("client view params mutex poisoned")
                    .unwrap_or(*fallback_view_params);
                if let Some(eye_offset_m) = *diagnostic_eye_offset_m {
                    apply_diagnostic_eye_offset(&mut local_view_params, eye_offset_m);
                    if !*logged_diagnostic_eye_offset {
                        log::info!(
                            "using diagnostic 3D eye offset {:.4}m for iosurface-3d view params",
                            eye_offset_m
                        );
                        *logged_diagnostic_eye_offset = true;
                    }
                }
                let render_view_params = horizontally_shifted_view_params(
                    local_view_params,
                    source.width() / 2,
                    *right_eye_shift_x_px,
                );
                let tracking = *latest_tracking
                    .lock()
                    .expect("latest tracking mutex poisoned");
                let (timestamp, hmd_pose, has_tracking) = tracking
                    .map(|(timestamp, pose)| (timestamp, pose, true))
                    .unwrap_or((fallback_timestamp, Pose::IDENTITY, false));
                let Some(frame) = source.frame(
                    frame_index,
                    hmd_pose,
                    render_view_params,
                    *diagnostic_forward_z_sign,
                    has_tracking,
                )?
                else {
                    return Ok(None);
                };
                if frame_index < 10 || frame_index % 120 == 0 {
                    log::info!(
                        "filled IOSurface 3D frame {frame_index} timing_us fill={} tracking={has_tracking}",
                        frame.fill_elapsed.as_micros()
                    );
                }
                Ok(Some(BridgeFrame {
                    pixel_buffer: frame.pixel_buffer,
                    recycle_buffer: Some(frame.recycle_buffer),
                    timestamp,
                    hmd_pose: has_tracking.then_some(hmd_pose),
                    input_idr: false,
                    view_params: Some(local_view_params),
                }))
            }
            Self::SharedMemory {
                shm,
                converter,
                expected_width,
                expected_height,
                last_log,
                client_view_params,
                latest_tracking,
                logged_tracking_timestamp,
            } => {
                let mut shm = shm.lock().expect("shared memory mutex poisoned");
                if shm.is_shutdown() {
                    return Ok(None);
                }
                shm.refresh_bridge_heartbeat();
                publish_client_view_params(&mut shm, client_view_params);

                if let Some((width, height, format)) = shm.config()
                    && last_log.elapsed() > Duration::from_secs(1)
                {
                    log::info!("shared memory configured: {width}x{height} format=0x{format:x}");
                    *last_log = Instant::now();
                }

                let view_params = shm.view_params();
                let Some(frame) = shm.try_acquire_frame()? else {
                    return Ok(None);
                };

                let frame_shape_ok = frame.header.width == *expected_width
                    && frame.header.height == *expected_height
                    && frame.header.stride == *expected_width * 4;
                if !frame_shape_ok {
                    let actual_width = frame.header.width;
                    let actual_height = frame.header.height;
                    let actual_stride = frame.header.stride;
                    let buffer_index = frame.buffer_index;
                    shm.release_frame(buffer_index);
                    anyhow::bail!(
                        "shared-memory frame shape changed: {actual_width}x{actual_height} stride {actual_stride} != {}x{} stride {}",
                        expected_width,
                        expected_height,
                        *expected_width * 4
                    );
                }

                let producer_publish_wall_ns = frame.header.producer_publish_wall_ns;
                let bridge_read_wall_ns = unix_time_ns();
                let publish_to_read_us = if producer_publish_wall_ns != 0
                    && bridge_read_wall_ns >= producer_publish_wall_ns
                {
                    Some((bridge_read_wall_ns - producer_publish_wall_ns) / 1_000)
                } else {
                    None
                };

                let convert_start = Instant::now();
                let pixel_buffer = converter.pixel_buffer_from_bgra(
                    frame.pixels,
                    frame.header.width,
                    frame.header.height,
                    frame.header.stride,
                )?;
                let convert_us = convert_start.elapsed().as_micros() as u64;
                let producer_timestamp = Duration::from_nanos(frame.header.timestamp_ns);
                let frame_pose = SharedMemory::frame_pose(&frame.header);
                let tracking = *latest_tracking
                    .lock()
                    .expect("latest tracking mutex poisoned");
                let (hmd_pose, timestamp) = if let Some(frame_pose) = frame_pose {
                    (Some(frame_pose), producer_timestamp)
                } else if let Some((timestamp, pose)) = tracking {
                    (Some(pose), timestamp)
                } else {
                    (None, producer_timestamp)
                };
                if timestamp != producer_timestamp && !*logged_tracking_timestamp {
                    log::info!(
                        "using latest AVP tracking timestamp for shared-memory encoded frames"
                    );
                    *logged_tracking_timestamp = true;
                }
                let input_idr = frame.header.is_idr != 0;
                let buffer_index = frame.buffer_index;
                let frame_number = frame.header.frame_number;
                let capture_total_us = frame.header.producer_capture_total_us;
                let copy_resource_us = frame.header.producer_copy_resource_us;
                let map_wait_us = frame.header.producer_map_wait_us;
                let copy_pixels_us = frame.header.producer_copy_pixels_us;
                let pair_copy_us = frame.header.producer_pair_copy_us;
                let left_capture_us = frame.header.producer_left_capture_us;
                let right_capture_us = frame.header.producer_right_capture_us;
                let real_submit_us = frame.header.producer_real_submit_us;
                shm.release_frame(buffer_index);

                if frame_number < 10 || frame_number % 120 == 0 {
                    let publish_to_read = publish_to_read_us
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "n/a".to_owned());
                    log::info!(
                        "read shared-memory frame {frame_number} timing_us real_submit={real_submit_us} producer_capture={capture_total_us} left={left_capture_us} right={right_capture_us} copy_resource={copy_resource_us} map_wait={map_wait_us} copy_pixels={copy_pixels_us} pair_copy={pair_copy_us} publish_to_read={publish_to_read} bgra_to_nv12={convert_us}"
                    );
                }

                Ok(Some(BridgeFrame {
                    pixel_buffer,
                    recycle_buffer: None,
                    timestamp,
                    hmd_pose,
                    input_idr,
                    view_params,
                }))
            }
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Self::Synthetic { .. } => false,
            Self::IosurfaceSynthetic { .. } => false,
            Self::IosurfaceReprojection { .. } => false,
            Self::Iosurface3d { .. } => false,
            Self::SharedMemory { shm, .. } => shm
                .lock()
                .expect("shared memory mutex poisoned")
                .is_shutdown(),
        }
    }

    fn recycle_buffer(&mut self, buffer: Nv12PixelBuffer) {
        match self {
            Self::IosurfaceSynthetic { source, .. } => source.recycle(buffer),
            Self::IosurfaceReprojection { source, .. } => source.recycle(buffer),
            Self::Iosurface3d { source, .. } => source.recycle(buffer),
            Self::Synthetic { .. } | Self::SharedMemory { .. } => {}
        }
    }
}

#[cfg(target_os = "macos")]
fn wait_for_shared_memory_config(
    shm: &mut SharedMemory,
    client_view_params: &Arc<Mutex<Option<[ViewParams; 2]>>>,
) -> Result<(u32, u32, u32)> {
    let timeout = Duration::from_secs(120);
    let start = Instant::now();
    let mut last_log = Instant::now();
    let mut last_invalid_log = Instant::now() - Duration::from_secs(1);

    while start.elapsed() < timeout {
        shm.refresh_bridge_heartbeat();
        publish_client_view_params(shm, client_view_params);
        if let Some(config) = shm.config() {
            let (width, height, format) = config;
            match SharedMemory::validate_config(width, height, format)
                .and_then(|()| validate_stream_shape(width, height))
            {
                Ok(()) => return Ok(config),
                Err(error) => {
                    if last_invalid_log.elapsed() >= Duration::from_secs(1) {
                        log::warn!(
                            "ignoring invalid shared-memory producer config {width}x{height} format=0x{format:x}: {error:#}"
                        );
                        last_invalid_log = Instant::now();
                    }
                }
            }
        }
        if shm.is_shutdown() {
            anyhow::bail!("shared-memory producer shut down before publishing config");
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            log::info!("still waiting for shared-memory producer config");
            last_log = Instant::now();
        }
        thread::sleep(Duration::from_millis(10));
    }

    anyhow::bail!("timed out waiting for shared-memory producer config after {timeout:?}")
}

#[cfg(target_os = "macos")]
fn publish_client_view_params(
    shm: &mut SharedMemory,
    client_view_params: &Arc<Mutex<Option<[ViewParams; 2]>>>,
) {
    if let Some(params) = *client_view_params
        .lock()
        .expect("client view params mutex poisoned")
    {
        shm.publish_view_params(params);
    }
}

#[cfg(target_os = "macos")]
fn validate_stream_shape(width: u32, height: u32) -> Result<()> {
    anyhow::ensure!(
        width > 0 && width % 4 == 0,
        "shared-memory width must be divisible by 4 for side-by-side NV12: {width}"
    );
    anyhow::ensure!(
        height > 0 && height % 2 == 0,
        "shared-memory height must be even: {height}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_shared_memory_writer(config: &BridgeConfig) -> Result<()> {
    let mut shm = SharedMemory::open()
        .context("shared-memory writer requires the bridge reader to be running first")?;
    shm.ensure_live_bridge()?;
    shm.configure(config.width, config.height, FORMAT_BGRA)?;

    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let frame_count = config.frame_count.unwrap_or(u64::MAX);
    let mut bgra = vec![0; config.width as usize * config.height as usize * 4];
    let start = Instant::now();

    for frame_index in 0..frame_count {
        fill_bgra_test_pattern(&mut bgra, config.width, config.height, frame_index);
        let mut wrote = false;
        let write_deadline = Instant::now() + frame_interval;
        while Instant::now() < write_deadline {
            shm.ensure_live_bridge()?;
            if shm.write_test_frame(
                frame_index,
                config.width,
                config.height,
                start.elapsed().as_nanos() as u64,
                &bgra,
            )? {
                wrote = true;
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        if !wrote {
            log::warn!("shared-memory writer dropped frame {frame_index}");
        }
        thread::sleep(frame_interval);
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn env_u32(name: &str, default: u32) -> Result<u32> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

#[cfg(target_os = "macos")]
fn env_input() -> Result<BridgeInput> {
    match env::var("ALVR_BRIDGE_INPUT")
        .unwrap_or_else(|_| "synthetic".to_string())
        .as_str()
    {
        "synthetic" => Ok(BridgeInput::Synthetic),
        "iosurface-3d" | "iosurface_3d" | "gpu-3d" | "gpu_3d" => Ok(BridgeInput::Iosurface3d),
        "iosurface-reprojection" | "iosurface_reprojection" | "reprojection" => {
            Ok(BridgeInput::IosurfaceReprojection)
        }
        "iosurface-synthetic" | "iosurface_synthetic" | "gpu-synthetic" | "gpu_synthetic" => {
            Ok(BridgeInput::IosurfaceSynthetic)
        }
        "shared-memory" | "shared_memory" | "shm" => Ok(BridgeInput::SharedMemory),
        "shared-memory-writer" | "shared_memory_writer" | "shm-writer" => {
            Ok(BridgeInput::SharedMemoryWriter)
        }
        other => anyhow::bail!("unsupported ALVR_BRIDGE_INPUT={other}"),
    }
}

#[cfg(target_os = "macos")]
fn env_u64(name: &str, default: u64) -> Result<u64> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

#[cfg(target_os = "macos")]
fn env_f32(name: &str, default: f32) -> Result<f32> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

#[cfg(target_os = "macos")]
fn env_optional_f32(name: &str) -> Result<Option<f32>> {
    env::var(name)
        .map(|value| {
            value
                .parse()
                .map(Some)
                .with_context(|| format!("invalid {name}"))
        })
        .unwrap_or(Ok(None))
}

#[cfg(target_os = "macos")]
fn env_i32(name: &str, default: i32) -> Result<i32> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

#[cfg(target_os = "macos")]
fn env_optional_u64(name: &str) -> Result<Option<u64>> {
    env::var(name)
        .map(|value| {
            value
                .parse()
                .map(Some)
                .with_context(|| format!("invalid {name}"))
        })
        .unwrap_or(Ok(None))
}
