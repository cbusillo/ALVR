mod contract;

pub use contract::{ContractError, FrameMetadata, SurfaceLeaseId};

#[cfg(target_os = "macos")]
mod alvr_sink;
#[cfg(target_os = "macos")]
mod encoder;
#[cfg(target_os = "macos")]
mod probe;
#[cfg(target_os = "macos")]
mod surface;

#[cfg(target_os = "macos")]
pub use alvr_sink::AlvrVideoSink;
#[cfg(target_os = "macos")]
pub use encoder::{
    EncodedFrame, HardwareEncoderSupport, NativeHevcEncoder, NativeHevcEncoderConfig,
    hevc_hardware_support,
};
#[cfg(target_os = "macos")]
pub use probe::{CadenceReport, ProbeConfig, ProbeSummary, run_surface_probe};
#[cfg(target_os = "macos")]
pub use surface::{PoolStats, SurfaceLease, SurfacePool};
