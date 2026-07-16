use alvr_common::ViewParams;
use std::{error::Error, fmt, time::Duration};

#[derive(Clone, Copy)]
pub struct FrameMetadata {
    pub frame_id: u64,
    pub stream_epoch: u64,
    pub video_timestamp: Duration,
    pub pose_timestamp: Duration,
    pub global_view_params: [ViewParams; 2],
}

impl fmt::Debug for FrameMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameMetadata")
            .field("frame_id", &self.frame_id)
            .field("stream_epoch", &self.stream_epoch)
            .field("video_timestamp", &self.video_timestamp)
            .field("pose_timestamp", &self.pose_timestamp)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceLeaseId {
    pub surface_id: u32,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractError {
    FrameIdOutOfOrder { previous: u64, next: u64 },
    StreamEpochOutOfOrder { previous: u64, next: u64 },
    VideoTimestampOutOfOrder { previous: Duration, next: Duration },
    PoseTimestampOutOfOrder { previous: Duration, next: Duration },
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameIdOutOfOrder { previous, next } => write!(
                formatter,
                "frame IDs must increase strictly: previous={previous} next={next}"
            ),
            Self::StreamEpochOutOfOrder { previous, next } => write!(
                formatter,
                "stream epochs must not decrease: previous={previous} next={next}"
            ),
            Self::VideoTimestampOutOfOrder { previous, next } => write!(
                formatter,
                "video timestamps must increase strictly: previous={previous:?} next={next:?}"
            ),
            Self::PoseTimestampOutOfOrder { previous, next } => write!(
                formatter,
                "pose timestamps must not decrease: previous={previous:?} next={next:?}"
            ),
        }
    }
}

impl Error for ContractError {}

#[derive(Clone, Copy)]
#[cfg(any(target_os = "macos", test))]
struct FrameOrderKey {
    frame_id: u64,
    stream_epoch: u64,
    video_timestamp: Duration,
    pose_timestamp: Duration,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
pub(crate) struct FrameOrderValidator {
    last_submission: Option<FrameOrderKey>,
}

#[cfg(any(target_os = "macos", test))]
impl FrameOrderValidator {
    pub fn validate(&self, metadata: &FrameMetadata) -> Result<(), ContractError> {
        let Some(previous) = self.last_submission else {
            return Ok(());
        };

        if metadata.frame_id <= previous.frame_id {
            return Err(ContractError::FrameIdOutOfOrder {
                previous: previous.frame_id,
                next: metadata.frame_id,
            });
        }
        if metadata.stream_epoch < previous.stream_epoch {
            return Err(ContractError::StreamEpochOutOfOrder {
                previous: previous.stream_epoch,
                next: metadata.stream_epoch,
            });
        }
        if metadata.stream_epoch == previous.stream_epoch
            && metadata.video_timestamp <= previous.video_timestamp
        {
            return Err(ContractError::VideoTimestampOutOfOrder {
                previous: previous.video_timestamp,
                next: metadata.video_timestamp,
            });
        }
        if metadata.stream_epoch == previous.stream_epoch
            && metadata.pose_timestamp < previous.pose_timestamp
        {
            return Err(ContractError::PoseTimestampOutOfOrder {
                previous: previous.pose_timestamp,
                next: metadata.pose_timestamp,
            });
        }

        Ok(())
    }

    pub fn record_validated(&mut self, metadata: FrameMetadata) {
        self.last_submission = Some(FrameOrderKey {
            frame_id: metadata.frame_id,
            stream_epoch: metadata.stream_epoch,
            video_timestamp: metadata.video_timestamp,
            pose_timestamp: metadata.pose_timestamp,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_for_epoch(
        frame_id: u64,
        stream_epoch: u64,
        video_ms: u64,
        pose_ms: u64,
    ) -> FrameMetadata {
        FrameMetadata {
            frame_id,
            stream_epoch,
            video_timestamp: Duration::from_millis(video_ms),
            pose_timestamp: Duration::from_millis(pose_ms),
            global_view_params: [ViewParams::DUMMY; 2],
        }
    }

    fn metadata(frame_id: u64, video_ms: u64, pose_ms: u64) -> FrameMetadata {
        metadata_for_epoch(frame_id, 0, video_ms, pose_ms)
    }

    fn submit(
        validator: &mut FrameOrderValidator,
        metadata: FrameMetadata,
    ) -> Result<(), ContractError> {
        validator.validate(&metadata)?;
        validator.record_validated(metadata);
        Ok(())
    }

    #[test]
    fn accepts_independent_monotonic_timestamps() {
        let mut validator = FrameOrderValidator::default();

        submit(&mut validator, metadata(10, 100, 80)).unwrap();
        submit(&mut validator, metadata(11, 111, 90)).unwrap();
    }

    #[test]
    fn rejects_metadata_regressions_without_advancing() {
        let mut validator = FrameOrderValidator::default();
        submit(&mut validator, metadata(5, 50, 40)).unwrap();

        assert!(matches!(
            validator.validate(&metadata(5, 60, 50)),
            Err(ContractError::FrameIdOutOfOrder { .. })
        ));
        assert!(matches!(
            validator.validate(&metadata(6, 50, 50)),
            Err(ContractError::VideoTimestampOutOfOrder { .. })
        ));
        assert!(matches!(
            validator.validate(&metadata(6, 60, 39)),
            Err(ContractError::PoseTimestampOutOfOrder { .. })
        ));

        submit(&mut validator, metadata(6, 60, 40)).unwrap();
    }

    #[test]
    fn allows_reusing_one_pose_sample_for_multiple_video_frames() {
        let mut validator = FrameOrderValidator::default();
        submit(&mut validator, metadata(1, 10, 8)).unwrap();
        submit(&mut validator, metadata(2, 20, 8)).unwrap();
    }

    #[test]
    fn allows_timestamp_reset_for_a_new_stream_epoch() {
        let mut validator = FrameOrderValidator::default();
        submit(&mut validator, metadata_for_epoch(1, 4, 100, 90)).unwrap();
        submit(&mut validator, metadata_for_epoch(2, 5, 10, 8)).unwrap();

        assert!(matches!(
            validator.validate(&metadata_for_epoch(3, 4, 110, 100)),
            Err(ContractError::StreamEpochOutOfOrder { .. })
        ));
    }
}
