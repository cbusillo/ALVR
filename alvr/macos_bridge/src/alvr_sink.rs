use crate::{EncodedFrame, FrameMetadata};
use alvr_common::{HEAD_ID, Pose, ViewParams};
use alvr_filesystem::Layout;
use alvr_server_core::{ServerCoreContext, ServerCoreEvent};
use alvr_session::CodecType;
use anyhow::{Result, ensure};
use std::{
    fs,
    path::Path,
    sync::mpsc::{Receiver, TryRecvError},
    time::Duration,
};

pub struct AlvrVideoSink {
    context: ServerCoreContext,
    events: Receiver<ServerCoreEvent>,
    force_keyframe: bool,
    shutdown_requested: bool,
    connected: bool,
    negotiated_codec: Option<CodecType>,
    local_view_params: Option<[ViewParams; 2]>,
    latest_tracking: Option<(Duration, Pose)>,
    decoder_config_sent: bool,
}

impl AlvrVideoSink {
    pub fn start(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let layout = Layout::new(root);
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
            negotiated_codec: None,
            local_view_params: None,
            latest_tracking: None,
            decoder_config_sent: false,
        })
    }

    pub fn poll_events(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(ServerCoreEvent::ClientConnected(config)) => {
                    self.connected = true;
                    self.negotiated_codec = Some(config.codec);
                    self.force_keyframe = true;
                }
                Ok(ServerCoreEvent::ClientDisconnected) => {
                    self.connected = false;
                    self.negotiated_codec = None;
                    self.latest_tracking = None;
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
        let (pose_timestamp, global_view_params) = self.latest_tracking.map_or(
            (video_timestamp, local_view_params),
            |(pose_timestamp, hmd_pose)| {
                (
                    pose_timestamp,
                    local_view_params.map(|params| ViewParams {
                        pose: hmd_pose * params.pose,
                        fov: params.fov,
                    }),
                )
            },
        );

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

    pub fn send(&mut self, mut frame: EncodedFrame) -> Result<()> {
        self.poll_events();
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
        self.context.send_video_nal(
            frame.metadata.video_timestamp,
            frame.metadata.global_view_params,
            frame.is_keyframe,
            frame.nal_data,
        );
        Ok(())
    }

    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }
}
