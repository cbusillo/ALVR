#[cfg(target_os = "macos")]
mod bgra;
#[cfg(target_os = "macos")]
mod encoder;
#[cfg(target_os = "macos")]
mod shared_memory;

#[cfg(target_os = "macos")]
mod synthetic;

#[cfg(target_os = "macos")]
use std::{
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
    Fov, Pose, ViewParams,
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
    server_context.start_connection();

    let force_idr = Arc::new(AtomicBool::new(true));
    let shutdown = Arc::new(AtomicBool::new(false));
    let client_view_params = Arc::new(Mutex::new(bootstrap_view_params()?));
    thread::spawn({
        let force_idr = Arc::clone(&force_idr);
        let shutdown = Arc::clone(&shutdown);
        let client_view_params = Arc::clone(&client_view_params);
        move || event_loop(events_receiver, force_idr, shutdown, client_view_params)
    });

    let (mut frame_source, stream_shape) =
        FrameSource::new(&config, Arc::clone(&client_view_params))?;
    let fallback_view_params = default_stereo_view_params(
        stream_shape.width,
        stream_shape.height,
        config.right_eye_shift_x_px,
    );
    let mut encoder = HevcEncoder::new(
        stream_shape.width,
        stream_shape.height,
        config.bitrate_bps,
        config.fps,
    )?;
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let start = Instant::now();
    let mut frame_index = 0_u64;
    let mut source_frame_index = 0_u64;
    let mut empty_output_count = 0_u32;

    while !shutdown.load(Ordering::SeqCst)
        && config
            .frame_count
            .is_none_or(|frame_count| source_frame_index < frame_count)
    {
        let force_keyframe =
            frame_index % u64::from(config.fps) == 0 || force_idr.swap(false, Ordering::SeqCst);
        let Some(frame) = frame_source.next_frame(frame_index, start.elapsed())? else {
            if frame_source.is_finished() {
                log::info!("frame source finished after {source_frame_index} input frames");
                break;
            }
            thread::sleep(Duration::from_micros(500));
            continue;
        };
        source_frame_index += 1;

        if let Some(output) =
            encoder.encode_pixel_buffer(&frame.pixel_buffer, force_keyframe || frame.input_idr)?
        {
            empty_output_count = 0;
            let view_params = client_view_params
                .lock()
                .expect("client view params mutex poisoned")
                .or(frame.view_params)
                .unwrap_or(fallback_view_params);
            send_encoded_output(
                &server_context,
                &mut encoder,
                frame.timestamp,
                output,
                view_params,
            );
        } else {
            empty_output_count += 1;
            if empty_output_count == config.fps * 2 {
                log::warn!(
                    "VideoToolbox has not produced output for {empty_output_count} submitted frames"
                );
                empty_output_count = 0;
            }
        }

        frame_index += 1;
        thread::sleep(frame_interval);
    }

    for output in encoder.finish()? {
        send_encoded_output(
            &server_context,
            &mut encoder,
            start.elapsed(),
            output,
            fallback_view_params,
        );
    }

    Ok(())
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

    server_context.send_video_nal(timestamp, view_params, output.is_keyframe, output.nal_data);
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
fn event_loop(
    events_receiver: mpsc::Receiver<ServerCoreEvent>,
    force_idr: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
) {
    let mut logged_client_view_params = false;
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
            ServerCoreEvent::Tracking { .. } => {}
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
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeInput {
    Synthetic,
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
    timestamp: Duration,
    input_idr: bool,
    view_params: Option<[ViewParams; 2]>,
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

        Ok(Self {
            root,
            input,
            width,
            height,
            fps,
            bitrate_bps: env_u64("ALVR_BRIDGE_BITRATE_BPS", 20_000_000)?,
            frame_count: env_optional_u64("ALVR_BRIDGE_FRAMES")?,
            right_eye_shift_x_px: env_f32("ALVR_BRIDGE_RIGHT_EYE_SHIFT_X_PX", 0.0)?,
        })
    }
}

#[cfg(target_os = "macos")]
enum FrameSource {
    Synthetic {
        source: SyntheticFrameSource,
        converter: Nv12Frame,
    },
    SharedMemory {
        shm: SharedMemory,
        converter: Nv12Frame,
        expected_width: u32,
        expected_height: u32,
        last_log: Instant,
        client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
    },
}

#[cfg(target_os = "macos")]
impl FrameSource {
    fn new(
        config: &BridgeConfig,
        client_view_params: Arc<Mutex<Option<[ViewParams; 2]>>>,
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
            BridgeInput::SharedMemory => {
                let mut shm = SharedMemory::create()?;
                log::info!("waiting for shared-memory producer config");
                let (width, height, format) =
                    wait_for_shared_memory_config(&mut shm, &client_view_params)?;
                log::info!("shared memory configured: {width}x{height} format=0x{format:x}");

                Ok((
                    Self::SharedMemory {
                        shm,
                        converter: Nv12Frame::new(width, height)?,
                        expected_width: width,
                        expected_height: height,
                        last_log: Instant::now(),
                        client_view_params,
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
                    timestamp: fallback_timestamp,
                    input_idr: false,
                    view_params: None,
                }))
            }
            Self::SharedMemory {
                shm,
                converter,
                expected_width,
                expected_height,
                last_log,
                client_view_params,
            } => {
                if shm.is_shutdown() {
                    return Ok(None);
                }
                shm.refresh_bridge_heartbeat();
                publish_client_view_params(shm, client_view_params);

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
                let timestamp = Duration::from_nanos(frame.header.timestamp_ns);
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
                    timestamp,
                    input_idr,
                    view_params,
                }))
            }
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Self::Synthetic { .. } => false,
            Self::SharedMemory { shm, .. } => shm.is_shutdown(),
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
