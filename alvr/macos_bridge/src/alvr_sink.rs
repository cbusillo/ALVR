use crate::{EncodedFrame, FrameMetadata, tracking_feedback::TrackingFeedback};
use alvr_common::{HAND_LEFT_ID, HAND_RIGHT_ID, HEAD_ID, Pose, ViewParams};
use alvr_filesystem::Layout;
use alvr_server_core::{ServerCoreContext, ServerCoreEvent, ServerNegotiatedStreamingConfig};
use alvr_session::{CodecType, SessionConfig, SteamvrHmdInitConfig};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    io::ErrorKind,
    path::Path,
    sync::mpsc::{Receiver, TryRecvError},
    time::Duration,
};

const DECODER_BOOTSTRAP_FRAME_LIMIT: u32 = 3;
const NATIVE_SOCKET_BUFFER_BYTES: u64 = 8_000_000;

#[derive(Clone, Copy)]
struct TrackingClock {
    source_origin: Duration,
    video_origin: Duration,
}

#[derive(Default)]
struct DecoderBootstrap {
    submitted: u32,
}

impl DecoderBootstrap {
    fn reset(&mut self) {
        self.submitted = 0;
    }

    fn admit(&mut self, decoder_config_sent: bool) -> bool {
        if decoder_config_sent || self.submitted >= DECODER_BOOTSTRAP_FRAME_LIMIT {
            return false;
        }
        self.submitted += 1;
        true
    }
}

pub struct AlvrVideoSink {
    context: ServerCoreContext,
    events: Receiver<ServerCoreEvent>,
    force_keyframe: bool,
    shutdown_requested: bool,
    connected: bool,
    ever_connected: bool,
    expected_width: u32,
    expected_height: u32,
    expected_fps: u32,
    stream_epoch: u64,
    connection_error: Option<String>,
    local_view_params: Option<[ViewParams; 2]>,
    latest_tracking: Option<(Duration, Pose)>,
    tracking_clock: Option<TrackingClock>,
    last_pose_timestamp: Duration,
    decoder_config_sent: bool,
    decoder_bootstrap: DecoderBootstrap,
    tracking_feedback: TrackingFeedback,
    feedback_view_published: bool,
    feedback_pose_published: bool,
    feedback_view_logged: bool,
    feedback_pose_logged: bool,
    exact_frame_pose_logged: bool,
    feedback_controller_published: [bool; 2],
}

impl AlvrVideoSink {
    pub fn start(
        root: &Path,
        width: u32,
        height: u32,
        fps: u32,
        runtime_generation: u64,
    ) -> Result<Self> {
        ensure!(
            width > 0 && width.is_multiple_of(64),
            "ALVR stream width must be positive and divisible by 64"
        );
        ensure!(
            height > 0 && height.is_multiple_of(32),
            "ALVR stream height must be positive and divisible by 32"
        );
        ensure!(fps > 0, "ALVR stream FPS must be positive");
        fs::create_dir_all(root)?;
        let layout = Layout::new(root);
        ensure_native_session(&layout, width, height, fps)?;
        alvr_server_core::initialize_environment(layout.clone());
        alvr_server_core::init_logging(Some(layout.session_log()), Some(layout.crash_log()));

        let (context, events) = ServerCoreContext::new();
        context.start_connection();
        let tracking_feedback = TrackingFeedback::create(runtime_generation)?;

        Ok(Self {
            context,
            events,
            force_keyframe: true,
            shutdown_requested: false,
            connected: false,
            ever_connected: false,
            expected_width: width,
            expected_height: height,
            expected_fps: fps,
            stream_epoch: 0,
            connection_error: None,
            local_view_params: None,
            latest_tracking: None,
            tracking_clock: None,
            last_pose_timestamp: Duration::ZERO,
            decoder_config_sent: false,
            decoder_bootstrap: DecoderBootstrap::default(),
            tracking_feedback,
            feedback_view_published: false,
            feedback_pose_published: false,
            feedback_view_logged: false,
            feedback_pose_logged: false,
            exact_frame_pose_logged: false,
            feedback_controller_published: [false; 2],
        })
    }

    pub fn poll_events(&mut self) {
        self.tracking_feedback.refresh_heartbeat();
        loop {
            match self.events.try_recv() {
                Ok(ServerCoreEvent::ClientConnected(config)) => {
                    self.stream_epoch = self
                        .stream_epoch
                        .checked_add(1)
                        .expect("ALVR stream epoch overflow");
                    self.connected = true;
                    self.ever_connected = true;
                    self.connection_error = validate_stream_config(
                        &config,
                        self.expected_width,
                        self.expected_height,
                        self.expected_fps,
                    )
                    .err()
                    .map(|error| error.to_string());
                    eprintln!(
                        "alvr_sink connected epoch={} view={}x{} emulated={}x{} fps={:.3} codec={:?} foveated={} ten_bit={} gamma={:.3} hdr={} contract={}",
                        self.stream_epoch,
                        config.transcoding_view_resolution.x,
                        config.transcoding_view_resolution.y,
                        config.emulated_headset_view_resolution.x,
                        config.emulated_headset_view_resolution.y,
                        config.refresh_rate,
                        config.codec,
                        config.enable_foveated_encoding,
                        config.use_10bit_encoder,
                        config.encoding_gamma,
                        config.enable_hdr,
                        if self.connection_error.is_some() {
                            "fail"
                        } else {
                            "pass"
                        },
                    );
                    self.force_keyframe = true;
                    self.local_view_params = None;
                    self.latest_tracking = None;
                    self.tracking_clock = None;
                    self.last_pose_timestamp = Duration::ZERO;
                    self.decoder_config_sent = false;
                    self.decoder_bootstrap.reset();
                    self.tracking_feedback.reset();
                    self.tracking_feedback.publish_client_connected(
                        self.stream_epoch,
                        self.connection_error.is_none(),
                    );
                    self.feedback_view_published = false;
                    self.feedback_pose_published = false;
                    self.feedback_view_logged = false;
                    self.feedback_pose_logged = false;
                    self.exact_frame_pose_logged = false;
                    self.feedback_controller_published = [false; 2];
                }
                Ok(ServerCoreEvent::ClientDisconnected) => {
                    self.stream_epoch = self
                        .stream_epoch
                        .checked_add(1)
                        .expect("ALVR stream epoch overflow");
                    self.connected = false;
                    self.connection_error = None;
                    self.local_view_params = None;
                    self.latest_tracking = None;
                    self.tracking_clock = None;
                    self.last_pose_timestamp = Duration::ZERO;
                    self.decoder_config_sent = false;
                    self.decoder_bootstrap.reset();
                    self.tracking_feedback.reset();
                    self.tracking_feedback
                        .publish_client_disconnected(self.stream_epoch);
                    self.feedback_view_published = false;
                    self.feedback_pose_published = false;
                    self.feedback_view_logged = false;
                    self.feedback_pose_logged = false;
                    self.exact_frame_pose_logged = false;
                    self.feedback_controller_published = [false; 2];
                }
                Ok(ServerCoreEvent::RequestIDR) => self.force_keyframe = true,
                Ok(ServerCoreEvent::LocalViewParams(params)) => {
                    self.local_view_params = Some(params);
                    if !self.feedback_view_logged {
                        eprintln!(
                            "alvr_sink OpenVR view feedback candidate left_fov=[{:.6},{:.6},{:.6},{:.6}] right_fov=[{:.6},{:.6},{:.6},{:.6}] eye_x=[{:.6},{:.6}]",
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
                        self.feedback_view_logged = true;
                    }
                    if self.tracking_feedback.publish_view_params(params)
                        && !self.feedback_view_published
                    {
                        eprintln!(
                            "alvr_sink OpenVR view feedback ready eye_x=[{:.6},{:.6}]",
                            params[0].pose.position.x, params[1].pose.position.x,
                        );
                        self.feedback_view_published = true;
                    }
                }
                Ok(ServerCoreEvent::Tracking { poll_timestamp }) => {
                    if let Some(motion) = self.context.get_device_motion(*HEAD_ID, poll_timestamp) {
                        self.latest_tracking = Some((poll_timestamp, motion.pose));
                        let published = self
                            .tracking_feedback
                            .publish_hmd_pose(poll_timestamp, motion.pose);
                        if !published && !self.feedback_pose_logged {
                            eprintln!(
                                "alvr_sink OpenVR HMD pose feedback candidate rejected position={:?} orientation={:?}",
                                motion.pose.position, motion.pose.orientation,
                            );
                            self.feedback_pose_logged = true;
                        }
                        if published && !self.feedback_pose_published {
                            eprintln!(
                                "alvr_sink OpenVR HMD pose feedback ready timestamp_ns={}",
                                poll_timestamp.as_nanos(),
                            );
                            self.feedback_pose_published = true;
                        }
                    }
                    for (controller_index, device_id) in
                        [*HAND_LEFT_ID, *HAND_RIGHT_ID].into_iter().enumerate()
                    {
                        if let Some(motion) =
                            self.context.get_device_motion(device_id, poll_timestamp)
                        {
                            let published = self.tracking_feedback.publish_controller_motion(
                                controller_index,
                                poll_timestamp,
                                motion,
                            );
                            if published && !self.feedback_controller_published[controller_index] {
                                eprintln!(
                                    "alvr_sink OpenVR controller feedback ready hand={} timestamp_ns={} position={:?}",
                                    if controller_index == 0 {
                                        "left"
                                    } else {
                                        "right"
                                    },
                                    poll_timestamp.as_nanos(),
                                    motion.pose.position,
                                );
                                self.feedback_controller_published[controller_index] = true;
                            }
                        }
                    }
                }
                Ok(ServerCoreEvent::RawButtons(entries) | ServerCoreEvent::Buttons(entries)) => {
                    self.tracking_feedback.publish_buttons(&entries);
                }
                Ok(ServerCoreEvent::ShutdownPending | ServerCoreEvent::RestartPending) => {
                    self.shutdown_requested = true
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.shutdown_requested = true;
                    break;
                }
            }
        }
    }

    pub fn frame_metadata(
        &mut self,
        frame_id: u64,
        video_timestamp: Duration,
        frame_pose: Option<(u64, Duration, Pose)>,
    ) -> Result<Option<FrameMetadata>> {
        self.poll_events();
        ensure!(
            self.connection_error.is_none(),
            "{}",
            self.connection_error.as_deref().unwrap_or_default()
        );
        let Some(local_view_params) = self.local_view_params else {
            return Ok(None);
        };
        if !self.connected {
            return Ok(None);
        }
        let (tracking_timestamp, hmd_pose) = if let Some((generation, timestamp, pose)) = frame_pose
        {
            ensure!(
                generation != 0,
                "IOSurface frame pose generation must be nonzero"
            );
            if !self.exact_frame_pose_logged || frame_id.is_multiple_of(300) {
                eprintln!(
                    "alvr_sink exact frame pose frame_id={frame_id} generation={generation} pose_timestamp_ns={} video_timestamp_ns={}",
                    timestamp.as_nanos(),
                    video_timestamp.as_nanos(),
                );
                self.exact_frame_pose_logged = true;
            }
            (timestamp, pose)
        } else {
            let Some(tracking) = self.latest_tracking else {
                return Ok(None);
            };
            tracking
        };
        let pose_timestamp = map_pose_timestamp(
            &mut self.tracking_clock,
            tracking_timestamp,
            video_timestamp,
            self.last_pose_timestamp,
        );
        self.last_pose_timestamp = pose_timestamp;

        Ok(Some(FrameMetadata {
            frame_id,
            stream_epoch: self.stream_epoch,
            video_timestamp,
            pose_timestamp,
            global_view_params: local_view_params.map(|params| ViewParams {
                pose: hmd_pose * params.pose,
                fov: params.fov,
            }),
        }))
    }

    pub fn bootstrap_frame_metadata(
        &mut self,
        frame_id: u64,
        video_timestamp: Duration,
        source_pose_timestamp: Duration,
        hmd_pose: Pose,
        fallback_view_params: [ViewParams; 2],
    ) -> Result<Option<FrameMetadata>> {
        self.poll_events();
        ensure!(
            self.connection_error.is_none(),
            "{}",
            self.connection_error.as_deref().unwrap_or_default()
        );
        if !self.connected || !self.decoder_bootstrap.admit(self.decoder_config_sent) {
            return Ok(None);
        }

        let local_view_params = self.local_view_params.unwrap_or(fallback_view_params);
        let pose_timestamp = video_timestamp.max(self.last_pose_timestamp);
        self.last_pose_timestamp = pose_timestamp;
        eprintln!(
            "alvr_sink decoder bootstrap frame_id={frame_id} index={}/{} source_pose_timestamp_ns={} video_timestamp_ns={}",
            self.decoder_bootstrap.submitted,
            DECODER_BOOTSTRAP_FRAME_LIMIT,
            source_pose_timestamp.as_nanos(),
            video_timestamp.as_nanos(),
        );

        Ok(Some(FrameMetadata {
            frame_id,
            stream_epoch: self.stream_epoch,
            video_timestamp,
            pose_timestamp,
            global_view_params: local_view_params.map(|params| ViewParams {
                pose: hmd_pose * params.pose,
                fov: params.fov,
            }),
        }))
    }

    pub fn take_force_keyframe(&mut self) -> bool {
        self.poll_events();
        std::mem::take(&mut self.force_keyframe)
    }

    pub fn send(&mut self, mut frame: EncodedFrame) -> Result<bool> {
        self.poll_events();
        if !self.connected || frame.metadata.stream_epoch != self.stream_epoch {
            return Ok(false);
        }
        ensure!(
            self.connection_error.is_none(),
            "{}",
            self.connection_error.as_deref().unwrap_or_default()
        );

        if !self.decoder_config_sent
            && let Some(config_nals) = frame.decoder_config_nals.take()
        {
            ensure!(
                !config_nals.is_empty(),
                "VideoToolbox keyframe did not include HEVC decoder configuration"
            );
            self.context
                .set_video_config_nals(config_nals, CodecType::Hevc);
            self.decoder_config_sent = true;
        }
        if self.connected && !self.decoder_config_sent {
            self.force_keyframe = true;
            return Ok(false);
        }
        let transported = self.context.send_video_nal(
            frame.metadata.video_timestamp,
            frame.metadata.global_view_params,
            frame.is_keyframe,
            frame.nal_data,
        );
        if transported {
            ensure!(
                self.tracking_feedback
                    .publish_frame_transported(self.stream_epoch),
                "ALVR client telemetry stream epoch changed during transport"
            );
        }

        Ok(transported)
    }

    pub fn ever_connected(&self) -> bool {
        self.ever_connected
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub fn connection_error(&self) -> Option<&str> {
        self.connection_error.as_deref()
    }
}

fn map_pose_timestamp(
    clock: &mut Option<TrackingClock>,
    tracking_timestamp: Duration,
    video_timestamp: Duration,
    previous_pose_timestamp: Duration,
) -> Duration {
    if clock.is_none_or(|clock| tracking_timestamp < clock.source_origin) {
        *clock = Some(TrackingClock {
            source_origin: tracking_timestamp,
            video_origin: video_timestamp.max(previous_pose_timestamp),
        });
    }
    let clock = clock.expect("tracking clock must be initialized");
    let mapped = clock.video_origin + tracking_timestamp.saturating_sub(clock.source_origin);

    mapped.max(previous_pose_timestamp)
}

fn ensure_native_session(layout: &Layout, width: u32, height: u32, fps: u32) -> Result<()> {
    let session_path = layout.session();
    let mut session = match fs::read_to_string(&session_path) {
        Ok(contents) if !contents.trim().is_empty() => serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", session_path.display()))?,
        Ok(_) => serde_json::to_value(SessionConfig::default())?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            serde_json::to_value(SessionConfig::default())?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", session_path.display()));
        }
    };

    if configure_native_session(&mut session, width, height, fps)? {
        let temporary_path = session_path.with_extension("json.macos-bridge.tmp");
        fs::write(&temporary_path, serde_json::to_vec_pretty(&session)?)
            .with_context(|| format!("failed to write {}", temporary_path.display()))?;
        fs::rename(&temporary_path, &session_path).with_context(|| {
            format!(
                "failed to replace {} with the HEVC session",
                session_path.display()
            )
        })?;
    }

    Ok(())
}

fn configure_native_session(
    session: &mut Value,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<bool> {
    ensure!(
        width > 0 && width.is_multiple_of(64),
        "native stream width must be positive and divisible by 64"
    );
    ensure!(
        height > 0 && height.is_multiple_of(32),
        "native stream height must be positive and divisible by 32"
    );
    ensure!(fps > 0, "native stream FPS must be positive");
    let per_eye_width = width / 2;
    let original_session = session.clone();
    for (path, value) in [
        (
            "/session_settings/headset/controllers/enabled",
            Value::Bool(true),
        ),
        (
            "/session_settings/headset/controllers/content/tracked",
            Value::Bool(true),
        ),
        (
            "/session_settings/headset/controllers/content/emulation_mode/variant",
            Value::String("PSVR2Sense".into()),
        ),
        (
            "/session_settings/connection/server_buffer_config/send_size_bytes/variant",
            Value::String("Custom".into()),
        ),
        (
            "/session_settings/connection/server_buffer_config/send_size_bytes/Custom",
            Value::from(NATIVE_SOCKET_BUFFER_BYTES),
        ),
        (
            "/session_settings/connection/server_buffer_config/recv_size_bytes/variant",
            Value::String("Custom".into()),
        ),
        (
            "/session_settings/connection/server_buffer_config/recv_size_bytes/Custom",
            Value::from(NATIVE_SOCKET_BUFFER_BYTES),
        ),
        (
            "/session_settings/connection/client_buffer_config/send_size_bytes/variant",
            Value::String("Custom".into()),
        ),
        (
            "/session_settings/connection/client_buffer_config/send_size_bytes/Custom",
            Value::from(NATIVE_SOCKET_BUFFER_BYTES),
        ),
        (
            "/session_settings/connection/client_buffer_config/recv_size_bytes/variant",
            Value::String("Custom".into()),
        ),
        (
            "/session_settings/connection/client_buffer_config/recv_size_bytes/Custom",
            Value::from(NATIVE_SOCKET_BUFFER_BYTES),
        ),
        (
            "/session_settings/video/preferred_codec/variant",
            Value::String("Hevc".into()),
        ),
        (
            "/session_settings/video/transcoding_view_resolution/variant",
            Value::String("Absolute".into()),
        ),
        (
            "/session_settings/video/transcoding_view_resolution/Absolute/width",
            Value::from(per_eye_width),
        ),
        (
            "/session_settings/video/transcoding_view_resolution/Absolute/height/set",
            Value::Bool(true),
        ),
        (
            "/session_settings/video/transcoding_view_resolution/Absolute/height/content",
            Value::from(height),
        ),
        (
            "/session_settings/video/emulated_headset_view_resolution/variant",
            Value::String("Absolute".into()),
        ),
        (
            "/session_settings/video/emulated_headset_view_resolution/Absolute/width",
            Value::from(per_eye_width),
        ),
        (
            "/session_settings/video/emulated_headset_view_resolution/Absolute/height/set",
            Value::Bool(true),
        ),
        (
            "/session_settings/video/emulated_headset_view_resolution/Absolute/height/content",
            Value::from(height),
        ),
        ("/session_settings/video/preferred_fps", Value::from(fps)),
        (
            "/session_settings/video/foveated_encoding/enabled",
            Value::Bool(false),
        ),
        (
            "/session_settings/video/encoder_config/use_10bit/set",
            Value::Bool(true),
        ),
        (
            "/session_settings/video/encoder_config/use_10bit/content",
            Value::Bool(false),
        ),
        (
            "/session_settings/video/encoder_config/encoding_gamma/set",
            Value::Bool(true),
        ),
        (
            "/session_settings/video/encoder_config/encoding_gamma/content",
            Value::from(1.0),
        ),
        (
            "/session_settings/video/encoder_config/hdr/enable/set",
            Value::Bool(true),
        ),
        (
            "/session_settings/video/encoder_config/hdr/enable/content",
            Value::Bool(false),
        ),
    ] {
        let target = session
            .pointer_mut(path)
            .with_context(|| format!("session is missing {path}"))?;
        *target = value;
    }

    let mut session_config: SessionConfig = serde_json::from_value(session.clone())
        .context("failed to deserialize configured native session")?;
    let steamvr_hmd_init_config = SteamvrHmdInitConfig {
        eye_resolution_width: per_eye_width,
        eye_resolution_height: height,
        target_eye_resolution_width: per_eye_width,
        target_eye_resolution_height: height,
        refresh_rate: fps,
    };
    let restart_settings_hash = alvr_server_core::compute_restart_settings_hash(
        &steamvr_hmd_init_config,
        &session_config.to_settings(),
    );
    session_config.steamvr_hmd_init_config = steamvr_hmd_init_config;
    session_config.restart_settings_hash = restart_settings_hash;
    *session = serde_json::to_value(session_config)?;

    Ok(*session != original_session)
}

fn validate_stream_config(
    config: &ServerNegotiatedStreamingConfig,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<()> {
    let per_eye_width = width / 2;
    ensure!(
        config.codec == CodecType::Hevc,
        "ALVR negotiated {:?}, expected HEVC",
        config.codec
    );
    ensure!(
        config.transcoding_view_resolution.x == per_eye_width
            && config.transcoding_view_resolution.y == height,
        "ALVR negotiated transcoding view {}x{}, expected {}x{}",
        config.transcoding_view_resolution.x,
        config.transcoding_view_resolution.y,
        per_eye_width,
        height
    );
    ensure!(
        config.emulated_headset_view_resolution.x == per_eye_width
            && config.emulated_headset_view_resolution.y == height,
        "ALVR negotiated emulated view {}x{}, expected {}x{}",
        config.emulated_headset_view_resolution.x,
        config.emulated_headset_view_resolution.y,
        per_eye_width,
        height
    );
    ensure!(
        (config.refresh_rate - fps as f32).abs() < 0.1,
        "ALVR negotiated {:.3} Hz, expected {fps} Hz",
        config.refresh_rate
    );
    ensure!(
        !config.enable_foveated_encoding,
        "ALVR negotiated foveated encoding for an unfoveated native frame"
    );
    ensure!(
        !config.use_10bit_encoder,
        "ALVR negotiated 10-bit encoding for an 8-bit native frame"
    );
    ensure!(
        (config.encoding_gamma - 1.0).abs() < 0.001,
        "ALVR negotiated gamma {:.3}, expected 1.0",
        config.encoding_gamma
    );
    ensure!(
        !config.enable_hdr,
        "ALVR negotiated HDR for an SDR native frame"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_common::glam::UVec2;
    use alvr_session::H264Profile;
    use serde_json::json;

    #[test]
    fn configures_native_stream_without_replacing_other_session_data() {
        let mut session = serde_json::to_value(SessionConfig::default()).unwrap();
        session["client_connections"] = json!({
            "avp.local": {
                "display_name": "Apple Vision Pro",
                "current_ip": null,
                "manual_ips": [],
                "trusted": true,
                "connection_state": "Disconnected"
            }
        });

        assert!(configure_native_session(&mut session, 2752, 1792, 90).unwrap());
        assert_eq!(
            session.pointer("/session_settings/video/preferred_codec/variant"),
            Some(&Value::String("Hevc".into()))
        );
        assert_eq!(
            session.pointer("/session_settings/headset/controllers/content/emulation_mode/variant"),
            Some(&Value::String("PSVR2Sense".into()))
        );
        assert_eq!(
            session.pointer(
                "/session_settings/connection/server_buffer_config/send_size_bytes/variant"
            ),
            Some(&Value::String("Custom".into()))
        );
        assert_eq!(
            session.pointer(
                "/session_settings/connection/server_buffer_config/send_size_bytes/Custom"
            ),
            Some(&Value::from(NATIVE_SOCKET_BUFFER_BYTES))
        );
        assert_eq!(
            session.pointer(
                "/session_settings/connection/client_buffer_config/recv_size_bytes/Custom"
            ),
            Some(&Value::from(NATIVE_SOCKET_BUFFER_BYTES))
        );
        assert_eq!(
            session.pointer("/session_settings/video/transcoding_view_resolution/Absolute/width"),
            Some(&Value::from(1376))
        );
        assert_eq!(
            session.pointer(
                "/session_settings/video/transcoding_view_resolution/Absolute/height/content"
            ),
            Some(&Value::from(1792))
        );
        assert_eq!(
            session.pointer("/session_settings/video/preferred_fps"),
            Some(&Value::from(90.0))
        );
        assert_eq!(
            session.pointer("/session_settings/video/foveated_encoding/enabled"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            session.pointer("/client_connections/avp.local/trusted"),
            Some(&Value::Bool(true))
        );
        let session_config: SessionConfig = serde_json::from_value(session.clone()).unwrap();
        assert_eq!(
            session_config.steamvr_hmd_init_config.eye_resolution_width,
            1376
        );
        assert_eq!(
            session_config.steamvr_hmd_init_config.eye_resolution_height,
            1792
        );
        assert_eq!(session_config.steamvr_hmd_init_config.refresh_rate, 90);
        assert_eq!(
            session_config.restart_settings_hash,
            alvr_server_core::compute_restart_settings_hash(
                &session_config.steamvr_hmd_init_config,
                &session_config.to_settings(),
            )
        );
        assert!(!configure_native_session(&mut session, 2752, 1792, 90).unwrap());
    }

    #[test]
    fn validates_the_fixed_native_stream_contract() {
        let mut config = ServerNegotiatedStreamingConfig {
            transcoding_view_resolution: UVec2::new(1376, 1792),
            emulated_headset_view_resolution: UVec2::new(1376, 1792),
            refresh_rate: 90.0,
            enable_foveated_encoding: false,
            codec: CodecType::Hevc,
            h264_profile: H264Profile::High,
            use_10bit_encoder: false,
            encoding_gamma: 1.0,
            enable_hdr: false,
        };

        validate_stream_config(&config, 2752, 1792, 90).unwrap();
        config.enable_foveated_encoding = true;
        assert!(validate_stream_config(&config, 2752, 1792, 90).is_err());
    }

    #[test]
    fn maps_reused_tracking_without_stalling_video_time() {
        let source_origin = Duration::from_secs(208_000);
        let mut clock = None;

        let first = map_pose_timestamp(
            &mut clock,
            source_origin,
            Duration::from_secs(12),
            Duration::ZERO,
        );
        let reused = map_pose_timestamp(&mut clock, source_origin, Duration::from_secs(13), first);
        let advanced = map_pose_timestamp(
            &mut clock,
            source_origin + Duration::from_millis(11),
            Duration::from_secs(13) + Duration::from_millis(11),
            reused,
        );

        assert_eq!(first, Duration::from_secs(12));
        assert_eq!(reused, first);
        assert_eq!(
            advanced,
            Duration::from_secs(12) + Duration::from_millis(11)
        );
    }

    #[test]
    fn remaps_tracking_after_source_clock_reset() {
        let mut clock = None;
        let first = map_pose_timestamp(
            &mut clock,
            Duration::from_secs(100),
            Duration::from_secs(6),
            Duration::ZERO,
        );
        let remapped = map_pose_timestamp(
            &mut clock,
            Duration::from_secs(2),
            Duration::from_secs(5),
            first,
        );
        let advanced = map_pose_timestamp(
            &mut clock,
            Duration::from_secs(2) + Duration::from_millis(9),
            Duration::from_secs(5) + Duration::from_millis(9),
            remapped,
        );

        assert_eq!(first, Duration::from_secs(6));
        assert_eq!(remapped, Duration::from_secs(6));
        assert_eq!(advanced, Duration::from_secs(6) + Duration::from_millis(9));
    }

    #[test]
    fn bounds_decoder_bootstrap_per_stream_epoch() {
        let mut bootstrap = DecoderBootstrap::default();

        for _ in 0..DECODER_BOOTSTRAP_FRAME_LIMIT {
            assert!(bootstrap.admit(false));
        }
        assert!(!bootstrap.admit(false));
        bootstrap.reset();
        assert!(bootstrap.admit(false));
        assert!(!bootstrap.admit(true));
    }

    #[test]
    fn exact_pose_clock_starts_after_bootstrap_video_time() {
        let bootstrap_timestamp = Duration::from_secs(20);
        let mut clock = None;
        let exact_timestamp = map_pose_timestamp(
            &mut clock,
            Duration::from_secs(300),
            Duration::from_secs(20) + Duration::from_millis(11),
            bootstrap_timestamp,
        );

        assert_eq!(
            exact_timestamp,
            Duration::from_secs(20) + Duration::from_millis(11)
        );
    }
}
