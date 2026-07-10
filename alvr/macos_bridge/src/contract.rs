use alvr_common::ViewParams;
#[cfg(any(target_os = "macos", test))]
use std::collections::VecDeque;
use std::{error::Error, fmt, time::Duration};

#[derive(Clone, Copy)]
pub struct FrameMetadata {
    pub frame_id: u64,
    pub video_timestamp: Duration,
    pub pose_timestamp: Duration,
    pub global_view_params: [ViewParams; 2],
}

impl fmt::Debug for FrameMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameMetadata")
            .field("frame_id", &self.frame_id)
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
    video_timestamp: Duration,
    pose_timestamp: Duration,
}

#[cfg(any(target_os = "macos", test))]
pub(crate) struct PendingSubmission<T> {
    pub lease_id: SurfaceLeaseId,
    pub metadata: FrameMetadata,
    pub resource: T,
}

#[cfg(any(target_os = "macos", test))]
pub(crate) struct OrderedPending<T> {
    last_submission: Option<FrameOrderKey>,
    queue: VecDeque<PendingSubmission<T>>,
}

#[cfg(any(target_os = "macos", test))]
impl<T> Default for OrderedPending<T> {
    fn default() -> Self {
        Self {
            last_submission: None,
            queue: VecDeque::new(),
        }
    }
}

#[cfg(any(target_os = "macos", test))]
impl<T> OrderedPending<T> {
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
        if metadata.video_timestamp <= previous.video_timestamp {
            return Err(ContractError::VideoTimestampOutOfOrder {
                previous: previous.video_timestamp,
                next: metadata.video_timestamp,
            });
        }
        if metadata.pose_timestamp < previous.pose_timestamp {
            return Err(ContractError::PoseTimestampOutOfOrder {
                previous: previous.pose_timestamp,
                next: metadata.pose_timestamp,
            });
        }

        Ok(())
    }

    pub fn push_validated(
        &mut self,
        lease_id: SurfaceLeaseId,
        metadata: FrameMetadata,
        resource: T,
    ) {
        self.last_submission = Some(FrameOrderKey {
            frame_id: metadata.frame_id,
            video_timestamp: metadata.video_timestamp,
            pose_timestamp: metadata.pose_timestamp,
        });
        self.queue.push_back(PendingSubmission {
            lease_id,
            metadata,
            resource,
        });
    }

    pub fn pop_output(&mut self) -> Option<PendingSubmission<T>> {
        self.queue.pop_front()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(frame_id: u64, video_ms: u64, pose_ms: u64) -> FrameMetadata {
        FrameMetadata {
            frame_id,
            video_timestamp: Duration::from_millis(video_ms),
            pose_timestamp: Duration::from_millis(pose_ms),
            global_view_params: [ViewParams::DUMMY; 2],
        }
    }

    fn submit<T>(
        queue: &mut OrderedPending<T>,
        lease_id: SurfaceLeaseId,
        metadata: FrameMetadata,
        resource: T,
    ) -> Result<(), ContractError> {
        queue.validate(&metadata)?;
        queue.push_validated(lease_id, metadata, resource);
        Ok(())
    }

    #[test]
    fn preserves_independent_timestamps_and_fifo_lease_order() {
        let first_id = SurfaceLeaseId {
            surface_id: 41,
            generation: 1,
        };
        let second_id = SurfaceLeaseId {
            surface_id: 17,
            generation: 2,
        };
        let mut queue = OrderedPending::default();

        submit(&mut queue, first_id, metadata(10, 100, 80), "first").unwrap();
        submit(&mut queue, second_id, metadata(11, 111, 90), "second").unwrap();

        let first = queue.pop_output().unwrap();
        assert_eq!(first.lease_id, first_id);
        assert_eq!(first.metadata.video_timestamp, Duration::from_millis(100));
        assert_eq!(first.metadata.pose_timestamp, Duration::from_millis(80));
        assert_eq!(first.resource, "first");

        let second = queue.pop_output().unwrap();
        assert_eq!(second.lease_id, second_id);
        assert_eq!(second.metadata.video_timestamp, Duration::from_millis(111));
        assert_eq!(second.metadata.pose_timestamp, Duration::from_millis(90));
        assert_eq!(second.resource, "second");
        assert!(queue.pop_output().is_none());
    }

    #[test]
    fn rejects_metadata_regressions_without_consuming_a_lease() {
        let lease_id = SurfaceLeaseId {
            surface_id: 1,
            generation: 1,
        };
        let mut queue = OrderedPending::default();
        submit(&mut queue, lease_id, metadata(5, 50, 40), ()).unwrap();

        assert!(matches!(
            queue.validate(&metadata(5, 60, 50)),
            Err(ContractError::FrameIdOutOfOrder { .. })
        ));
        assert!(matches!(
            queue.validate(&metadata(6, 50, 50)),
            Err(ContractError::VideoTimestampOutOfOrder { .. })
        ));
        assert!(matches!(
            queue.validate(&metadata(6, 60, 39)),
            Err(ContractError::PoseTimestampOutOfOrder { .. })
        ));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn allows_reusing_one_pose_sample_for_multiple_video_frames() {
        let mut queue = OrderedPending::default();
        submit(
            &mut queue,
            SurfaceLeaseId {
                surface_id: 1,
                generation: 1,
            },
            metadata(1, 10, 8),
            (),
        )
        .unwrap();
        submit(
            &mut queue,
            SurfaceLeaseId {
                surface_id: 2,
                generation: 2,
            },
            metadata(2, 20, 8),
            (),
        )
        .unwrap();
    }
}
