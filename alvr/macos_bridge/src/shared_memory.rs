use alvr_common::{
    Fov, Pose, ViewParams,
    glam::{Mat4, Quat, Vec3},
};
use anyhow::{Context, Result, bail};
use memmap2::MmapMut;
use std::{
    fs::{File, OpenOptions},
    mem,
    path::Path,
    process,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub const SHM_PATH: &str = "/tmp/alvr_frame_buffer.shm";
pub const SHM_MAGIC: u32 = 0x414C5652;
pub const SHM_VERSION: u32 = 5;
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 2048;
pub const BYTES_PER_PIXEL: u32 = 4;
pub const MAX_FRAME_SIZE: usize = (MAX_WIDTH * MAX_HEIGHT * BYTES_PER_PIXEL) as usize;
pub const NUM_BUFFERS: usize = 3;
pub const FORMAT_BGRA: u32 = 87; // DXGI_FORMAT_B8G8R8A8_UNORM
const MAX_BRIDGE_HEARTBEAT_AGE: Duration = Duration::from_secs(5);
const BRIDGE_HEARTBEAT_FUTURE_TOLERANCE: Duration = Duration::from_millis(250);

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    Empty = 0,
    Writing = 1,
    Ready = 2,
    Encoding = 3,
}

#[repr(C)]
pub struct FrameHeaderRaw {
    pub state: AtomicU32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp_ns: u64,
    pub frame_number: u64,
    pub is_idr: u8,
    pub padding: [u8; 7],
    pub pose: [[f32; 4]; 3],
    pub producer_publish_wall_ns: u64,
    pub producer_capture_total_us: u32,
    pub producer_copy_resource_us: u32,
    pub producer_map_wait_us: u32,
    pub producer_copy_pixels_us: u32,
    pub producer_pair_copy_us: u32,
    pub producer_left_capture_us: u32,
    pub producer_right_capture_us: u32,
    pub producer_real_submit_us: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp_ns: u64,
    pub frame_number: u64,
    pub is_idr: u8,
    pub pose: [[f32; 4]; 3],
    pub producer_publish_wall_ns: u64,
    pub producer_capture_total_us: u32,
    pub producer_copy_resource_us: u32,
    pub producer_map_wait_us: u32,
    pub producer_copy_pixels_us: u32,
    pub producer_pair_copy_us: u32,
    pub producer_left_capture_us: u32,
    pub producer_right_capture_us: u32,
    pub producer_real_submit_us: u32,
}

impl FrameHeader {
    fn from_raw(raw: &FrameHeaderRaw) -> Self {
        Self {
            width: raw.width,
            height: raw.height,
            stride: raw.stride,
            timestamp_ns: raw.timestamp_ns,
            frame_number: raw.frame_number,
            is_idr: raw.is_idr,
            pose: raw.pose,
            producer_publish_wall_ns: raw.producer_publish_wall_ns,
            producer_capture_total_us: raw.producer_capture_total_us,
            producer_copy_resource_us: raw.producer_copy_resource_us,
            producer_map_wait_us: raw.producer_map_wait_us,
            producer_copy_pixels_us: raw.producer_copy_pixels_us,
            producer_pair_copy_us: raw.producer_pair_copy_us,
            producer_left_capture_us: raw.producer_left_capture_us,
            producer_right_capture_us: raw.producer_right_capture_us,
            producer_real_submit_us: raw.producer_real_submit_us,
        }
    }
}

#[repr(C)]
pub struct SharedMemoryHeader {
    pub magic: u32,
    pub version: u32,
    pub initialized: u32,
    pub shutdown: u32,
    pub config_width: u32,
    pub config_height: u32,
    pub config_format: u32,
    pub config_set: AtomicU32,
    pub write_sequence: u64,
    pub read_sequence: u64,
    pub frames_written: u64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub bridge_session_id: u64,
    pub bridge_heartbeat_ns: u64,
    pub view_config_set: AtomicU32,
    pub view_fov: [[f32; 4]; 2],
    pub view_eye_x_m: [f32; 2],
    pub hmd_pose_set: AtomicU32,
    pub hmd_pose_sequence: AtomicU32,
    pub frame_pose_sequence: AtomicU32,
    pub hmd_pose_timestamp_ns: u64,
    pub frame_pose_timestamp_ns: u64,
    pub frame_pose: [[f32; 4]; 3],
    pub hmd_pose: [[f32; 4]; 3],
    pub frame_headers: [FrameHeaderRaw; NUM_BUFFERS],
}

const _: () = {
    assert!(mem::offset_of!(SharedMemoryHeader, write_sequence) == 32);
    assert!(mem::offset_of!(SharedMemoryHeader, hmd_pose_set) == 132);
    assert!(mem::offset_of!(SharedMemoryHeader, hmd_pose_timestamp_ns) == 144);
    assert!(mem::offset_of!(SharedMemoryHeader, frame_headers) == 256);
    assert!(mem::offset_of!(SharedMemoryHeader, view_config_set) == 88);
    assert!(mem::offset_of!(FrameHeaderRaw, producer_publish_wall_ns) == 88);
    assert!(mem::size_of::<FrameHeaderRaw>() == 128);
};

pub struct AcquiredFrame<'a> {
    pub buffer_index: usize,
    pub header: FrameHeader,
    pub pixels: &'a [u8],
}

pub struct SharedMemory {
    _file: File,
    mmap: MmapMut,
}

pub fn frame_offset(buffer_index: usize) -> usize {
    let header_size = mem::size_of::<SharedMemoryHeader>();
    let aligned_header = (header_size + 4095) & !4095;
    aligned_header + buffer_index * MAX_FRAME_SIZE
}

pub fn total_size() -> usize {
    frame_offset(NUM_BUFFERS)
}

impl SharedMemory {
    pub fn create() -> Result<Self> {
        let path = Path::new(SHM_PATH);
        let size = total_size();

        log::info!("creating shared memory at {SHM_PATH} ({size} bytes)");

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);

        let file = options
            .open(path)
            .context("failed to create shared memory file")?;
        file.set_len(size as u64)
            .context("failed to size shared memory file")?;

        let mut mmap =
            unsafe { MmapMut::map_mut(&file) }.context("failed to map shared memory file")?;
        initialize_mapping(&mut mmap)?;

        Ok(Self { _file: file, mmap })
    }

    pub fn open() -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(SHM_PATH)
            .context("failed to open shared memory file")?;
        let mmap =
            unsafe { MmapMut::map_mut(&file) }.context("failed to map shared memory file")?;

        let this = Self { _file: file, mmap };
        this.validate()?;
        Ok(this)
    }

    pub fn header(&self) -> &SharedMemoryHeader {
        unsafe { &*(self.mmap.as_ptr() as *const SharedMemoryHeader) }
    }

    pub fn header_mut(&mut self) -> &mut SharedMemoryHeader {
        unsafe { &mut *(self.mmap.as_mut_ptr() as *mut SharedMemoryHeader) }
    }

    pub fn validate(&self) -> Result<()> {
        let header = self.header();
        if header.magic != SHM_MAGIC {
            bail!("invalid shared memory magic: 0x{:08x}", header.magic);
        }
        if header.version != SHM_VERSION {
            bail!("unsupported shared memory version: {}", header.version);
        }
        Ok(())
    }

    pub fn refresh_bridge_heartbeat(&mut self) {
        self.header_mut().bridge_heartbeat_ns = unix_time_ns();
    }

    pub fn ensure_live_bridge(&self) -> Result<()> {
        let header = self.header();
        if header.initialized == 0 {
            bail!("shared-memory bridge is not initialized");
        }
        if header.shutdown != 0 {
            bail!("shared-memory bridge is shut down");
        }
        if header.bridge_session_id == 0 || header.bridge_heartbeat_ns == 0 {
            bail!("shared-memory bridge heartbeat is missing");
        }

        let now = unix_time_ns();
        let heartbeat = header.bridge_heartbeat_ns;
        let max_age = MAX_BRIDGE_HEARTBEAT_AGE.as_nanos() as u64;
        let future_tolerance = BRIDGE_HEARTBEAT_FUTURE_TOLERANCE.as_nanos() as u64;
        let heartbeat_live = if heartbeat <= now {
            now - heartbeat <= max_age
        } else {
            heartbeat - now <= future_tolerance
        };
        if !heartbeat_live {
            bail!("shared-memory bridge heartbeat is stale");
        }

        Ok(())
    }

    pub fn configure(&mut self, width: u32, height: u32, format: u32) -> Result<()> {
        validate_frame_shape(width, height, width * BYTES_PER_PIXEL)?;
        let header = self.header_mut();
        header.config_width = width;
        header.config_height = height;
        header.config_format = format;
        header.config_set.store(1, Ordering::Release);
        self.mmap.flush_async().ok();
        Ok(())
    }

    pub fn config(&self) -> Option<(u32, u32, u32)> {
        let header = self.header();
        (header.config_set.load(Ordering::Acquire) != 0).then_some((
            header.config_width,
            header.config_height,
            header.config_format,
        ))
    }

    pub fn view_params(&self) -> Option<[ViewParams; 2]> {
        let header = self.header();
        if header.view_config_set.load(Ordering::Acquire) == 0 {
            return None;
        }

        let build_view = |eye: usize| {
            let fov = header.view_fov[eye];
            Some(ViewParams {
                pose: Pose {
                    orientation: Quat::IDENTITY,
                    position: Vec3::new(header.view_eye_x_m[eye], 0.0, 0.0),
                },
                fov: Fov {
                    left: valid_fov_angle(fov[0])?,
                    right: valid_fov_angle(fov[1])?,
                    up: valid_fov_angle(fov[2])?,
                    down: valid_fov_angle(fov[3])?,
                },
            })
        };

        let params = [build_view(0)?, build_view(1)?];
        valid_view_params(params).then_some(params)
    }

    pub fn publish_view_params(&mut self, params: [ViewParams; 2]) -> bool {
        if !valid_view_params(params) {
            return false;
        }

        let header = self.header_mut();
        for (eye, params) in params.iter().enumerate() {
            header.view_fov[eye] = [
                params.fov.left,
                params.fov.right,
                params.fov.up,
                params.fov.down,
            ];
            header.view_eye_x_m[eye] = params.pose.position.x;
        }
        header.view_config_set.store(1, Ordering::Release);
        true
    }

    pub fn publish_hmd_pose(&mut self, timestamp: Duration, pose: Pose) -> bool {
        if !valid_pose(pose) {
            return false;
        }

        let matrix = pose_to_matrix34(pose);
        let header = self.header_mut();
        let sequence = header.hmd_pose_sequence.load(Ordering::Relaxed);
        let write_sequence = if sequence % 2 == 0 {
            sequence.wrapping_add(1)
        } else {
            sequence
        };
        header
            .hmd_pose_sequence
            .store(write_sequence, Ordering::Release);
        header.hmd_pose = matrix;
        header.hmd_pose_timestamp_ns = timestamp.as_nanos() as u64;
        header
            .hmd_pose_sequence
            .store(write_sequence.wrapping_add(1), Ordering::Release);
        header.hmd_pose_set.store(1, Ordering::Release);
        true
    }

    pub fn frame_pose(header: &FrameHeader) -> Option<Pose> {
        if !valid_pose_matrix(header.pose) {
            return None;
        }

        let pose = matrix34_to_pose(header.pose)?;
        valid_pose(pose).then_some(pose)
    }

    pub fn validate_config(width: u32, height: u32, format: u32) -> Result<()> {
        if format != FORMAT_BGRA {
            bail!("unsupported shared memory format: 0x{format:x}");
        }
        validate_frame_shape(width, height, width * BYTES_PER_PIXEL)
    }

    pub fn is_shutdown(&self) -> bool {
        self.header().shutdown != 0
    }

    pub fn try_acquire_frame(&mut self) -> Result<Option<AcquiredFrame<'_>>> {
        let header = self.header();
        let mut selected = None;
        for buffer_index in 0..NUM_BUFFERS {
            let frame_header = &header.frame_headers[buffer_index];
            if frame_header.state.load(Ordering::Acquire) == FrameState::Ready as u32 {
                let frame = FrameHeader::from_raw(frame_header);
                if selected.is_none_or(|(_, selected_frame): (usize, FrameHeader)| {
                    frame.frame_number < selected_frame.frame_number
                }) {
                    selected = Some((buffer_index, frame));
                }
            }
        }

        if let Some((buffer_index, frame)) = selected {
            let frame_header = &header.frame_headers[buffer_index];
            if frame_header
                .state
                .compare_exchange(
                    FrameState::Ready as u32,
                    FrameState::Encoding as u32,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                if let Err(error) = validate_frame_shape(frame.width, frame.height, frame.stride) {
                    frame_header
                        .state
                        .store(FrameState::Empty as u32, Ordering::Release);
                    return Err(error);
                }
                let size = frame.height as usize * frame.stride as usize;
                let offset = frame_offset(buffer_index);
                let pixels = &self.mmap[offset..offset + size];
                return Ok(Some(AcquiredFrame {
                    buffer_index,
                    header: frame,
                    pixels,
                }));
            }
        }

        Ok(None)
    }

    pub fn release_frame(&mut self, buffer_index: usize) {
        self.header().frame_headers[buffer_index]
            .state
            .store(FrameState::Empty as u32, Ordering::Release);

        let header = self.header_mut();
        header.frames_encoded = header.frames_encoded.wrapping_add(1);
        header.read_sequence = header.read_sequence.wrapping_add(1);
    }

    pub fn shutdown(&mut self) {
        self.header_mut().shutdown = 1;
        self.mmap.flush_async().ok();
    }

    pub fn write_test_frame(
        &mut self,
        frame_number: u64,
        width: u32,
        height: u32,
        timestamp_ns: u64,
        pixels: &[u8],
    ) -> Result<bool> {
        validate_frame_shape(width, height, width * BYTES_PER_PIXEL)?;
        let expected_size = width as usize * height as usize * BYTES_PER_PIXEL as usize;
        if pixels.len() != expected_size {
            bail!(
                "unexpected BGRA frame size: {} != {expected_size}",
                pixels.len()
            );
        }

        let Some(buffer_index) = self.acquire_write_buffer() else {
            self.header_mut().frames_dropped = self.header().frames_dropped.wrapping_add(1);
            return Ok(false);
        };

        let offset = frame_offset(buffer_index);
        self.mmap[offset..offset + pixels.len()].copy_from_slice(pixels);

        let frame_header = &mut self.header_mut().frame_headers[buffer_index];
        frame_header.width = width;
        frame_header.height = height;
        frame_header.stride = width * BYTES_PER_PIXEL;
        frame_header.timestamp_ns = timestamp_ns;
        frame_header.frame_number = frame_number;
        frame_header.is_idr = u8::from(frame_number == 0 || frame_number % 60 == 0);
        frame_header.pose = [[0.0; 4]; 3];
        frame_header.producer_publish_wall_ns = unix_time_ns();
        frame_header.producer_capture_total_us = 0;
        frame_header.producer_copy_resource_us = 0;
        frame_header.producer_map_wait_us = 0;
        frame_header.producer_copy_pixels_us = 0;
        frame_header.producer_pair_copy_us = 0;
        frame_header.producer_left_capture_us = 0;
        frame_header.producer_right_capture_us = 0;
        frame_header.producer_real_submit_us = 0;
        frame_header
            .state
            .store(FrameState::Ready as u32, Ordering::Release);

        let header = self.header_mut();
        header.write_sequence = header.write_sequence.wrapping_add(1);
        header.frames_written = header.frames_written.wrapping_add(1);
        Ok(true)
    }

    fn acquire_write_buffer(&self) -> Option<usize> {
        let sequence = self.header().write_sequence as usize;
        for attempt in 0..NUM_BUFFERS {
            let buffer_index = (sequence + attempt) % NUM_BUFFERS;
            if self.header().frame_headers[buffer_index]
                .state
                .compare_exchange(
                    FrameState::Empty as u32,
                    FrameState::Writing as u32,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Some(buffer_index);
            }
        }

        None
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn initialize_mapping(mmap: &mut MmapMut) -> Result<()> {
    mmap.fill(0);
    let header = unsafe { &mut *(mmap.as_mut_ptr() as *mut SharedMemoryHeader) };
    header.magic = SHM_MAGIC;
    header.version = SHM_VERSION;
    header.initialized = 1;
    header.bridge_session_id = unix_time_ns() ^ u64::from(process::id());
    header.bridge_heartbeat_ns = unix_time_ns();
    for frame_header in &mut header.frame_headers {
        frame_header.state = AtomicU32::new(FrameState::Empty as u32);
    }
    mmap.flush().context("failed to flush shared memory header")
}

pub fn unix_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default()
}

fn validate_frame_shape(width: u32, height: u32, stride: u32) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_WIDTH || height > MAX_HEIGHT {
        bail!("invalid frame dimensions: {width}x{height}");
    }
    let min_stride = width * BYTES_PER_PIXEL;
    if stride < min_stride {
        bail!("invalid frame stride: {stride} < {min_stride}");
    }
    let frame_size = height as usize * stride as usize;
    if frame_size > MAX_FRAME_SIZE {
        bail!("frame too large: {frame_size} > {MAX_FRAME_SIZE}");
    }
    Ok(())
}

fn valid_fov_angle(value: f32) -> Option<f32> {
    (value.is_finite() && value.abs() > 0.001 && value.abs() < std::f32::consts::FRAC_PI_2)
        .then_some(value)
}

pub(crate) fn valid_view_params(params: [ViewParams; 2]) -> bool {
    params.iter().all(|params| {
        valid_pose(params.pose)
            && params.pose.position.x.abs() <= 0.2
            && params.fov.left < -0.001
            && params.fov.right > 0.001
            && params.fov.up > 0.001
            && params.fov.down < -0.001
    })
}

fn valid_pose(pose: Pose) -> bool {
    let orientation_len = pose.orientation.length_squared();
    pose.position.is_finite()
        && pose.orientation.is_finite()
        && (0.5..=1.5).contains(&orientation_len)
}

fn valid_pose_matrix(matrix: [[f32; 4]; 3]) -> bool {
    if !matrix
        .iter()
        .flatten()
        .all(|value| value.is_finite() && value.abs() <= 1000.0)
    {
        return false;
    }

    let rows = [
        Vec3::new(matrix[0][0], matrix[0][1], matrix[0][2]),
        Vec3::new(matrix[1][0], matrix[1][1], matrix[1][2]),
        Vec3::new(matrix[2][0], matrix[2][1], matrix[2][2]),
    ];

    rows.iter()
        .all(|row| (0.5..=1.5).contains(&row.length_squared()))
        && rows[0].dot(rows[1]).abs() <= 0.2
        && rows[0].dot(rows[2]).abs() <= 0.2
        && rows[1].dot(rows[2]).abs() <= 0.2
}

fn matrix34_to_pose(matrix: [[f32; 4]; 3]) -> Option<Pose> {
    if !valid_pose_matrix(matrix) {
        return None;
    }

    let cols = [
        [matrix[0][0], matrix[1][0], matrix[2][0], 0.0],
        [matrix[0][1], matrix[1][1], matrix[2][1], 0.0],
        [matrix[0][2], matrix[1][2], matrix[2][2], 0.0],
        [matrix[0][3], matrix[1][3], matrix[2][3], 1.0],
    ];
    let transform = Mat4::from_cols_array_2d(&cols);
    let (_scale, orientation, position) = transform.to_scale_rotation_translation();
    Some(Pose {
        orientation,
        position,
    })
}

fn pose_to_matrix34(pose: Pose) -> [[f32; 4]; 3] {
    let cols = Mat4::from_rotation_translation(pose.orientation, pose.position).to_cols_array_2d();
    [
        [cols[0][0], cols[1][0], cols[2][0], cols[3][0]],
        [cols[0][1], cols[1][1], cols[2][1], cols[3][1]],
        [cols[0][2], cols[1][2], cols[2][2], cols[3][2]],
    ]
}
