use alvr_common::{
    Pose,
    glam::{Mat3, Quat, Vec3},
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr::NonNull,
    time::Duration,
};

const ERROR_CAPACITY: usize = 512;
const VISIBLE_COLOR_THRESHOLD: u32 = 96;
pub const SOURCE_SLOT_COUNT: usize = 3;
pub const FRAME_FLAG_SELF_TEST: u32 = 1;
pub const FRAME_FLAG_CONSUMER_SAMPLE: u32 = 1 << 1;
pub const FRAME_FLAG_FALLBACK_POSE: u32 = 1 << 2;
pub const FRAME_FLAG_STARTUP_BARRIER: u32 = 1 << 3;
pub const STATUS_PASS: u32 = 0;
pub const STATUS_COPY_FAILED: u32 = 8;
pub const STATUS_SESSION_CLOSED: u32 = 9;
pub const STATUS_FRAME_DROPPED: u32 = 10;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawSourceFrame {
    frame_id: u64,
    video_timestamp_ns: u64,
    pose_timestamp_ns: u64,
    pose_generation: u64,
    pose: [[f32; 4]; 3],
    slot_index: u32,
    generation: u32,
    flags: u32,
    surface_id: u32,
    width: u32,
    height: u32,
    sample_x: u32,
    sample_y: u32,
    expected_bgra: [u8; 4],
    actual_bgra: [u8; 4],
    reply_port: u32,
    validation_status: u32,
}

const _: () = assert!(size_of::<RawSourceFrame>() == 128);

unsafe extern "C" {
    fn alvr_native_source_create(
        service_name: *const c_char,
        session_nonce: u64,
        width: u32,
        height: u32,
        error_buffer: *mut c_char,
        error_capacity: usize,
    ) -> *mut c_void;
    fn alvr_native_source_accept(
        source: *mut c_void,
        timeout_ms: u32,
        error_buffer: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn alvr_native_source_producer_pid(source: *mut c_void) -> u32;
    fn alvr_native_source_producer_pidversion(source: *mut c_void) -> u32;
    fn alvr_native_source_producer_start_token(source: *mut c_void) -> u64;
    fn alvr_native_source_next_frame(
        source: *mut c_void,
        timeout_ms: u32,
        output: *mut RawSourceFrame,
        error_buffer: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn alvr_native_source_surface(source: *mut c_void, slot_index: u32) -> *mut c_void;
    fn alvr_native_source_release(
        source: *mut c_void,
        frame: *mut RawSourceFrame,
        status: u32,
        error_buffer: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn alvr_native_source_destroy(source: *mut c_void);
}

pub struct NativeSource {
    source: NonNull<c_void>,
    width: u32,
    height: u32,
}

pub struct AuthenticatedProducer {
    pub pid: u32,
    pub pid_version: u32,
    pub start_token: u64,
}

pub struct NativeSourceFrame<'a> {
    source: &'a NativeSource,
    raw: RawSourceFrame,
    released: bool,
}

impl NativeSource {
    pub fn new(service_name: &str, session_nonce: u64, width: u32, height: u32) -> Result<Self> {
        ensure!(
            session_nonce != 0,
            "IOSurface session nonce must be nonzero"
        );
        ensure!(
            width > 0 && height > 0,
            "IOSurface source dimensions must be nonzero"
        );
        let service_name = CString::new(service_name)?;
        let mut error = [0 as c_char; ERROR_CAPACITY];
        let source = unsafe {
            alvr_native_source_create(
                service_name.as_ptr(),
                session_nonce,
                width,
                height,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        NonNull::new(source)
            .map(|source| Self {
                source,
                width,
                height,
            })
            .ok_or_else(|| anyhow!(error_message(&error)))
    }

    pub fn accept_producer(&self, timeout: Duration) -> Result<AuthenticatedProducer> {
        let mut error = [0 as c_char; ERROR_CAPACITY];
        let status = unsafe {
            alvr_native_source_accept(
                self.source.as_ptr(),
                timeout_millis(timeout)?,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        ensure!(
            status == 0,
            "IOSurface producer handshake failed: {}",
            error_message(&error)
        );
        let producer_pid = unsafe { alvr_native_source_producer_pid(self.source.as_ptr()) };
        let producer_pid_version =
            unsafe { alvr_native_source_producer_pidversion(self.source.as_ptr()) };
        let producer_start_token =
            unsafe { alvr_native_source_producer_start_token(self.source.as_ptr()) };
        ensure!(
            producer_pid != 0 && producer_pid_version != 0 && producer_start_token != 0,
            "IOSurface producer handshake returned incomplete authenticated identity"
        );
        Ok(AuthenticatedProducer {
            pid: producer_pid,
            pid_version: producer_pid_version,
            start_token: producer_start_token,
        })
    }

    pub fn next_frame(&self, timeout: Duration) -> Result<Option<NativeSourceFrame<'_>>> {
        let mut raw = RawSourceFrame::default();
        let mut error = [0 as c_char; ERROR_CAPACITY];
        let status = unsafe {
            alvr_native_source_next_frame(
                self.source.as_ptr(),
                timeout_millis(timeout)?,
                &mut raw,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        match status {
            0 => Ok(Some(NativeSourceFrame {
                source: self,
                raw,
                released: false,
            })),
            1 => Ok(None),
            _ => Err(anyhow!(
                "IOSurface frame receive failed: {}",
                error_message(&error)
            )),
        }
    }

    pub fn surface(&self, slot_index: u32) -> Result<NonNull<c_void>> {
        ensure!(
            slot_index < SOURCE_SLOT_COUNT as u32,
            "IOSurface slot index is out of range"
        );
        NonNull::new(unsafe { alvr_native_source_surface(self.source.as_ptr(), slot_index) })
            .context("native source returned a null IOSurface")
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for NativeSource {
    fn drop(&mut self) {
        unsafe { alvr_native_source_destroy(self.source.as_ptr()) }
    }
}

impl NativeSourceFrame<'_> {
    pub fn frame_id(&self) -> u64 {
        self.raw.frame_id
    }

    pub fn video_timestamp(&self) -> Duration {
        Duration::from_nanos(self.raw.video_timestamp_ns)
    }

    pub fn frame_pose(&self) -> Result<(u64, Duration, Pose)> {
        ensure!(
            self.raw.pose_timestamp_ns != 0
                && (self.is_fallback_pose() == (self.raw.pose_generation == 0)),
            "IOSurface frame is missing its render-pose metadata"
        );
        let pose = pose_from_matrix34(self.raw.pose);
        ensure!(
            pose.position.is_finite()
                && pose.orientation.is_finite()
                && (0.5..=1.5).contains(&pose.orientation.length_squared()),
            "IOSurface frame render pose is invalid"
        );
        Ok((
            self.raw.pose_generation,
            Duration::from_nanos(self.raw.pose_timestamp_ns),
            pose,
        ))
    }

    pub fn slot_index(&self) -> u32 {
        self.raw.slot_index
    }

    pub fn generation(&self) -> u32 {
        self.raw.generation
    }

    pub fn is_self_test(&self) -> bool {
        self.raw.flags & FRAME_FLAG_SELF_TEST != 0
    }

    pub fn is_consumer_sample(&self) -> bool {
        self.raw.flags & FRAME_FLAG_CONSUMER_SAMPLE != 0
    }

    pub fn is_visible_consumer_sample(&self) -> bool {
        is_visible_consumer_sample(&self.raw)
    }

    pub fn is_fallback_pose(&self) -> bool {
        self.raw.flags & FRAME_FLAG_FALLBACK_POSE != 0
    }

    pub fn is_startup_barrier(&self) -> bool {
        self.raw.flags & FRAME_FLAG_STARTUP_BARRIER != 0
    }

    pub fn validation_status(&self) -> u32 {
        self.raw.validation_status
    }

    pub fn actual_bgra(&self) -> [u8; 4] {
        self.raw.actual_bgra
    }

    pub fn expected_bgra(&self) -> [u8; 4] {
        self.raw.expected_bgra
    }

    pub(crate) fn surface(&self) -> Result<NonNull<c_void>> {
        self.source.surface(self.raw.slot_index)
    }

    pub fn release(mut self, status: u32) -> Result<()> {
        self.release_inner(status)
    }

    fn release_inner(&mut self, status: u32) -> Result<()> {
        if self.released {
            return Ok(());
        }
        let mut error = [0 as c_char; ERROR_CAPACITY];
        let result = unsafe {
            alvr_native_source_release(
                self.source.source.as_ptr(),
                &mut self.raw,
                status,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        self.released = true;
        ensure!(
            result == 0,
            "IOSurface slot release failed: {}",
            error_message(&error)
        );
        Ok(())
    }
}

impl Drop for NativeSourceFrame<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release_inner(STATUS_COPY_FAILED);
        }
    }
}

fn timeout_millis(timeout: Duration) -> Result<u32> {
    u32::try_from(timeout.as_millis()).map_err(|_| anyhow!("Mach timeout exceeds u32 milliseconds"))
}

fn error_message(buffer: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn is_visible_consumer_sample(frame: &RawSourceFrame) -> bool {
    frame.flags & FRAME_FLAG_CONSUMER_SAMPLE != 0
        && frame.actual_bgra[..3]
            .iter()
            .map(|component| u32::from(*component))
            .sum::<u32>()
            >= VISIBLE_COLOR_THRESHOLD
}

fn pose_from_matrix34(matrix: [[f32; 4]; 3]) -> Pose {
    let rotation = Mat3::from_cols(
        Vec3::new(matrix[0][0], matrix[1][0], matrix[2][0]),
        Vec3::new(matrix[0][1], matrix[1][1], matrix[2][1]),
        Vec3::new(matrix[0][2], matrix[1][2], matrix[2][2]),
    );
    Pose {
        orientation: Quat::from_mat3(&rotation).normalize(),
        position: Vec3::new(matrix[0][3], matrix[1][3], matrix[2][3]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_openvr_matrix_to_pose() {
        let pose = pose_from_matrix34([
            [0.0, 0.0, 1.0, 1.0],
            [0.0, 1.0, 0.0, 2.0],
            [-1.0, 0.0, 0.0, 3.0],
        ]);

        assert_eq!(pose.position, Vec3::new(1.0, 2.0, 3.0));
        assert!(pose.orientation.is_finite());
        assert!((pose.orientation.length_squared() - 1.0).abs() < 0.0001);
    }

    #[test]
    fn classifies_visible_consumer_samples_by_rgb_only() {
        let visible = RawSourceFrame {
            flags: FRAME_FLAG_CONSUMER_SAMPLE,
            actual_bgra: [32, 32, 32, 0],
            ..Default::default()
        };
        assert!(is_visible_consumer_sample(&visible));

        let alpha_only = RawSourceFrame {
            actual_bgra: [0, 0, 0, 255],
            ..visible
        };
        assert!(!is_visible_consumer_sample(&alpha_only));

        let metadata_only = RawSourceFrame {
            flags: 0,
            ..visible
        };
        assert!(!is_visible_consumer_sample(&metadata_only));
    }
}
