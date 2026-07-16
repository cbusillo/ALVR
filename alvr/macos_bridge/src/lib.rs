mod contract;

pub use contract::{ContractError, FrameMetadata, SurfaceLeaseId};

#[cfg(target_os = "macos")]
mod alvr_sink;
#[cfg(target_os = "macos")]
mod encoder;
#[cfg(target_os = "macos")]
mod metal;
#[cfg(target_os = "macos")]
mod native_probe;
#[cfg(target_os = "macos")]
mod native_source;
#[cfg(target_os = "macos")]
mod probe;
#[cfg(target_os = "macos")]
mod surface;
#[cfg(target_os = "macos")]
mod tracking_feedback;

#[cfg(target_os = "macos")]
pub use alvr_sink::AlvrVideoSink;
#[cfg(target_os = "macos")]
pub use encoder::{
    EncodedFrame, HardwareEncoderSupport, NativeHevcEncoder, NativeHevcEncoderConfig,
    hevc_hardware_support,
};
#[cfg(target_os = "macos")]
pub use native_probe::{
    NativeCadenceReport, NativeProbeSummary, NativeSourceConfig, run_native_source_probe,
};
#[cfg(target_os = "macos")]
pub use probe::{CadenceReport, ProbeConfig, ProbeSummary, run_surface_probe};
#[cfg(target_os = "macos")]
pub use surface::{PoolStats, SurfaceLease, SurfacePool};
