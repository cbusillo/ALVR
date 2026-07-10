use crate::{EncodedFrame, FrameMetadata};
use alvr_common::{HEAD_ID, Pose, ViewParams};
use alvr_filesystem::Layout;
use alvr_server_core::{ServerCoreContext, ServerCoreEvent};
use alvr_session::{CodecType, SessionConfig};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    io::ErrorKind,
    path::Path,
    sync::mpsc::{Receiver, TryRecvError},
    time::Duration,
};

#[derive(Clone, Copy)]
struct TrackingClock {
    source_origin: Duration,
    local_origin: Duration,
}

pub struct AlvrVideoSink {
    context: ServerCoreContext,
    events: Receiver<ServerCoreEvent>,
    force_keyframe: bool,
    shutdown_requested: bool,
    connected: bool,
    ever_connected: bool,
    negotiated_codec: Option<CodecType>,
    local_view_params: Option<[ViewParams; 2]>,
    latest_tracking: Option<(Duration, Pose)>,
    tracking_clock: Option<TrackingClock>,
    last_pose_timestamp: Duration,
    decoder_config_sent: bool,
}

impl AlvrVideoSink {
    pub fn start(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let layout = Layout::new(root);
        ensure_hevc_session(&layout)?;
        alvr_server_core::initialize_environment(layout.clone());
        alvr_server_core::init_logging(Some(layout.session_log()), Some(layout.crash_log()));

        let (context, events) = ServerCoreContext::new();
        context.start_connection();

        Ok(Self {
            context,
            events,
            force_keyframe: true,
            shutdown_requested: false,
            connected: false,
            ever_connected: false,
            negotiated_codec: None,
            local_view_params: None,
            latest_tracking: None,
            tracking_clock: None,
            last_pose_timestamp: Duration::ZERO,
            decoder_config_sent: false,
        })
    }

    pub fn poll_events(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(ServerCoreEvent::ClientConnected(config)) => {
                    self.connected = true;
                    self.ever_connected = true;
                    self.negotiated_codec = Some(config.codec);
                    self.force_keyframe = true;
                    self.latest_tracking = None;
                    self.tracking_clock = None;
                    self.decoder_config_sent = false;
                }
                Ok(ServerCoreEvent::ClientDisconnected) => {
                    self.connected = false;
                    self.negotiated_codec = None;
                    self.latest_tracking = None;
                    self.tracking_clock = None;
                    self.decoder_config_sent = false;
                }
                Ok(ServerCoreEvent::RequestIDR) => self.force_keyframe = true,
                Ok(ServerCoreEvent::LocalViewParams(params)) => {
                    self.local_view_params = Some(params)
                }
                Ok(ServerCoreEvent::Tracking { poll_timestamp }) => {
                    if let Some(motion) = self.context.get_device_motion(*HEAD_ID, poll_timestamp) {
                        self.latest_tracking = Some((poll_timestamp, motion.pose));
                    }
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
        fallback_view_params: [ViewParams; 2],
    ) -> FrameMetadata {
        self.poll_events();
        let local_view_params = self.local_view_params.unwrap_or(fallback_view_params);
        let (source_pose_timestamp, global_view_params) =
            self.latest_tracking
                .map_or((None, local_view_params), |(pose_timestamp, hmd_pose)| {
                    (
                        Some(pose_timestamp),
                        local_view_params.map(|params| ViewParams {
                            pose: hmd_pose * params.pose,
                            fov: params.fov,
                        }),
                    )
                });
        let pose_timestamp = map_pose_timestamp(
            &mut self.tracking_clock,
            source_pose_timestamp,
            video_timestamp,
            self.last_pose_timestamp,
        );
        self.last_pose_timestamp = pose_timestamp;

        FrameMetadata {
            frame_id,
            video_timestamp,
            pose_timestamp,
            global_view_params,
        }
    }

    pub fn take_force_keyframe(&mut self) -> bool {
        self.poll_events();
        std::mem::take(&mut self.force_keyframe)
    }

    pub fn send(&mut self, mut frame: EncodedFrame) -> Result<bool> {
        self.poll_events();
        if !self.connected {
            return Ok(false);
        }
        if let Some(codec) = self.negotiated_codec {
            ensure!(
                codec == CodecType::Hevc,
                "connected ALVR client negotiated {codec:?}, but the native surface contract emits HEVC"
            );
        }

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
        self.context.send_video_nal(
            frame.metadata.video_timestamp,
            frame.metadata.global_view_params,
            frame.is_keyframe,
            frame.nal_data,
        );
        Ok(true)
    }

    pub fn ever_connected(&self) -> bool {
        self.ever_connected
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }
}

fn ensure_hevc_session(layout: &Layout) -> Result<()> {
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

    if set_hevc_preferred_codec(&mut session)? {
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

fn set_hevc_preferred_codec(session: &mut Value) -> Result<bool> {
    let variant = session
        .pointer_mut("/session_settings/video/preferred_codec/variant")
        .context("session is missing video.preferred_codec.variant")?;
    ensure!(
        variant.is_string(),
        "session video.preferred_codec.variant must be a string"
    );
    if variant.as_str() == Some("Hevc") {
        return Ok(false);
    }

    *variant = Value::String("Hevc".into());
    Ok(true)
}

fn map_pose_timestamp(
    clock: &mut Option<TrackingClock>,
    source_timestamp: Option<Duration>,
    video_timestamp: Duration,
    previous_pose_timestamp: Duration,
) -> Duration {
    let Some(source_timestamp) = source_timestamp else {
        return video_timestamp.max(previous_pose_timestamp);
    };

    if clock.is_none_or(|clock| source_timestamp < clock.source_origin) {
        *clock = Some(TrackingClock {
            source_origin: source_timestamp,
            local_origin: video_timestamp.max(previous_pose_timestamp),
        });
    }
    let clock = clock.expect("tracking clock must be initialized");
    let mapped = clock.local_origin + source_timestamp.saturating_sub(clock.source_origin);

    mapped.max(previous_pose_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn configures_hevc_without_replacing_other_session_data() {
        let mut session = json!({
            "client_connections": { "avp.local": { "trusted": true } },
            "session_settings": {
                "video": { "preferred_codec": { "variant": "H264" } }
            }
        });

        assert!(set_hevc_preferred_codec(&mut session).unwrap());
        assert_eq!(
            session.pointer("/session_settings/video/preferred_codec/variant"),
            Some(&Value::String("Hevc".into()))
        );
        assert_eq!(
            session.pointer("/client_connections/avp.local/trusted"),
            Some(&Value::Bool(true))
        );
        assert!(!set_hevc_preferred_codec(&mut session).unwrap());
    }

    #[test]
    fn default_session_schema_exposes_the_codec_variant_path() {
        let mut session = serde_json::to_value(SessionConfig::default()).unwrap();

        assert!(set_hevc_preferred_codec(&mut session).unwrap());
        assert_eq!(
            session.pointer("/session_settings/video/preferred_codec/variant"),
            Some(&Value::String("Hevc".into()))
        );
    }

    #[test]
    fn maps_tracking_and_fallback_timestamps_into_one_monotonic_clock() {
        let source_origin = Duration::from_secs(116_000);
        let mut clock = None;

        let first = map_pose_timestamp(
            &mut clock,
            Some(source_origin),
            Duration::from_secs(2),
            Duration::ZERO,
        );
        assert_eq!(first, Duration::from_secs(2));

        let reused = map_pose_timestamp(
            &mut clock,
            Some(source_origin),
            Duration::from_secs(3),
            first,
        );
        assert_eq!(reused, first);

        let advanced = map_pose_timestamp(
            &mut clock,
            Some(source_origin + Duration::from_millis(8)),
            Duration::from_secs(3),
            reused,
        );
        assert_eq!(advanced, Duration::from_millis(2008));

        let fallback = map_pose_timestamp(&mut clock, None, Duration::from_millis(2100), advanced);
        assert_eq!(fallback, Duration::from_millis(2100));

        clock = None;
        let reconnected = map_pose_timestamp(
            &mut clock,
            Some(Duration::from_secs(12)),
            Duration::from_millis(2200),
            fallback,
        );
        assert_eq!(reconnected, Duration::from_millis(2200));
    }
}
