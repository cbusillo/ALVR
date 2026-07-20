use alvr_common::{DeviceMotion, Pose, ViewParams, glam::Mat4, inputs as inp};
use alvr_packets::{ButtonEntry, ButtonValue};
use anyhow::{Context, Result, ensure};
use memmap2::{MmapMut, MmapOptions};
use std::{
    fs::{File, OpenOptions},
    mem,
    path::Path,
    process, ptr,
    sync::atomic::{AtomicU32, AtomicU64, Ordering, fence},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const SHM_PATH: &str = "/tmp/alvr_frame_buffer.shm";
const SHM_MAGIC: u32 = 0x414C5652;
const SHM_VERSION: u32 = 7;
const NUM_BUFFERS: usize = 3;
const NUM_CONTROLLERS: usize = 2;

const CLIENT_STATE_WAITING: u32 = 0;
const CLIENT_STATE_CONNECTED: u32 = 1;
const CLIENT_STATE_STREAMING: u32 = 2;

const BUTTON_SYSTEM: u64 = 1 << 0;
const BUTTON_APPLICATION_MENU: u64 = 1 << 1;
const BUTTON_GRIP: u64 = 1 << 2;
const BUTTON_A: u64 = 1 << 7;
const BUTTON_TOUCHPAD: u64 = 1 << 32;
const BUTTON_TRIGGER: u64 = 1 << 33;

#[repr(C)]
struct FrameHeaderRaw {
    state: AtomicU32,
    width: u32,
    height: u32,
    stride: u32,
    timestamp_ns: u64,
    frame_number: u64,
    is_idr: u8,
    padding: [u8; 7],
    pose: [[f32; 4]; 3],
    producer_publish_wall_ns: u64,
    producer_capture_total_us: u32,
    producer_copy_resource_us: u32,
    producer_map_wait_us: u32,
    producer_copy_pixels_us: u32,
    producer_pair_copy_us: u32,
    producer_left_capture_us: u32,
    producer_right_capture_us: u32,
    producer_real_submit_us: u32,
}

#[repr(C)]
struct ControllerStateRaw {
    sequence: AtomicU32,
    connected: AtomicU32,
    packet_number: u32,
    reserved: u32,
    tracking_timestamp_ns: u64,
    motion_update_wall_ns: u64,
    input_update_wall_ns: u64,
    pose: [[f32; 4]; 3],
    linear_velocity: [f32; 3],
    angular_velocity: [f32; 3],
    buttons_pressed: u64,
    buttons_touched: u64,
    axes: [[f32; 2]; 5],
    padding: [u8; 8],
}

#[repr(C)]
struct SharedMemoryHeader {
    magic: u32,
    version: u32,
    initialized: AtomicU32,
    shutdown: AtomicU32,
    config_width: u32,
    config_height: u32,
    config_format: u32,
    config_set: AtomicU32,
    write_sequence: u64,
    read_sequence: u64,
    frames_written: u64,
    frames_encoded: u64,
    frames_dropped: u64,
    bridge_session_id: AtomicU64,
    bridge_heartbeat_ns: AtomicU64,
    view_config_set: AtomicU32,
    view_fov: [[f32; 4]; 2],
    view_eye_x_m: [f32; 2],
    hmd_pose_set: AtomicU32,
    hmd_pose_sequence: AtomicU32,
    frame_pose_sequence: AtomicU32,
    hmd_pose_timestamp_ns: u64,
    frame_pose_timestamp_ns: u64,
    frame_pose: [[f32; 4]; 3],
    hmd_pose: [[f32; 4]; 3],
    frame_headers: [FrameHeaderRaw; NUM_BUFFERS],
    controllers: [ControllerStateRaw; NUM_CONTROLLERS],
    telemetry_sequence: AtomicU32,
    client_state: AtomicU32,
    stream_contract_valid: AtomicU32,
    telemetry_reserved: u32,
    runtime_generation: AtomicU64,
    bridge_pid: AtomicU64,
    stream_epoch: AtomicU64,
    frames_transported: AtomicU64,
    connect_events: AtomicU64,
    disconnect_events: AtomicU64,
    contract_failure_events: AtomicU64,
}

const _: () = {
    assert!(mem::offset_of!(SharedMemoryHeader, write_sequence) == 32);
    assert!(mem::offset_of!(SharedMemoryHeader, view_config_set) == 88);
    assert!(mem::offset_of!(SharedMemoryHeader, hmd_pose_set) == 132);
    assert!(mem::offset_of!(SharedMemoryHeader, hmd_pose_timestamp_ns) == 144);
    assert!(mem::offset_of!(SharedMemoryHeader, frame_headers) == 256);
    assert!(mem::offset_of!(SharedMemoryHeader, controllers) == 640);
    assert!(mem::offset_of!(SharedMemoryHeader, telemetry_sequence) == 992);
    assert!(mem::size_of::<FrameHeaderRaw>() == 128);
    assert!(mem::size_of::<ControllerStateRaw>() == 176);
    assert!(mem::offset_of!(SharedMemoryHeader, connect_events) == 1040);
    assert!(mem::size_of::<SharedMemoryHeader>() == 1064);
};

pub(crate) struct TrackingFeedback {
    _file: File,
    mmap: MmapMut,
}

impl TrackingFeedback {
    pub(crate) fn create(runtime_generation: u64) -> Result<Self> {
        Self::create_at(Path::new(SHM_PATH), runtime_generation)
    }

    fn create_at(path: &Path, runtime_generation: u64) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);

        let file = options
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        #[cfg(unix)]
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.nlink() == 1,
            "shared memory path has unsafe type or ownership: {}",
            path.display()
        );
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", path.display()))?;
        let header_size = mem::size_of::<SharedMemoryHeader>();
        let existing_size = metadata.len();
        if existing_size < header_size as u64 {
            file.set_len(header_size as u64)
                .with_context(|| format!("failed to size {}", path.display()))?;
        }
        let mut mmap = unsafe { MmapOptions::new().len(header_size).map_mut(&file) }
            .context("failed to map OpenVR feedback")?;
        let compatible_header = existing_size >= header_size as u64
            && mmap[..4] == SHM_MAGIC.to_ne_bytes()
            && mmap[4..8] == SHM_VERSION.to_ne_bytes();
        if compatible_header {
            let header = unsafe { &*(mmap.as_ptr().cast::<SharedMemoryHeader>()) };
            header.shutdown.store(1, Ordering::SeqCst);
            header.initialized.store(0, Ordering::SeqCst);
            fence(Ordering::SeqCst);
        }
        mmap.fill(0);

        let mut feedback = Self { _file: file, mmap };
        let session_id = unix_time_ns() ^ u64::from(process::id());
        let heartbeat = unix_time_ns();
        let header = feedback.header_mut();
        header.shutdown.store(1, Ordering::SeqCst);
        header.initialized.store(0, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        header.magic = SHM_MAGIC;
        header.version = SHM_VERSION;
        header.config_width = 0;
        header.config_height = 0;
        header.config_format = 0;
        header.config_set.store(0, Ordering::Relaxed);
        let write_sequence = begin_feedback_write(&header.hmd_pose_sequence);
        header.view_config_set.store(0, Ordering::Relaxed);
        header.hmd_pose_set.store(0, Ordering::Relaxed);
        finish_feedback_write(&header.hmd_pose_sequence, write_sequence);
        reset_controllers(header);
        header
            .bridge_session_id
            .store(session_id.max(1), Ordering::Relaxed);
        header
            .bridge_heartbeat_ns
            .store(heartbeat, Ordering::Relaxed);
        let telemetry_sequence = begin_feedback_write(&header.telemetry_sequence);
        header
            .client_state
            .store(CLIENT_STATE_WAITING, Ordering::Relaxed);
        header.stream_contract_valid.store(0, Ordering::Relaxed);
        header.telemetry_reserved = 0;
        header
            .runtime_generation
            .store(runtime_generation, Ordering::Relaxed);
        header
            .bridge_pid
            .store(u64::from(process::id()), Ordering::Relaxed);
        header.stream_epoch.store(0, Ordering::Relaxed);
        header.frames_transported.store(0, Ordering::Relaxed);
        header.connect_events.store(0, Ordering::Relaxed);
        header.disconnect_events.store(0, Ordering::Relaxed);
        header.contract_failure_events.store(0, Ordering::Relaxed);
        finish_feedback_write(&header.telemetry_sequence, telemetry_sequence);
        header.shutdown.store(0, Ordering::Release);
        header.initialized.store(1, Ordering::Release);

        Ok(feedback)
    }

    pub(crate) fn refresh_heartbeat(&mut self) {
        self.header_mut()
            .bridge_heartbeat_ns
            .store(unix_time_ns(), Ordering::Relaxed);
    }

    pub(crate) fn publish_client_connected(&mut self, stream_epoch: u64, contract_valid: bool) {
        let header = self.header_mut();
        let sequence = begin_feedback_write(&header.telemetry_sequence);
        header
            .client_state
            .store(CLIENT_STATE_CONNECTED, Ordering::Relaxed);
        header
            .stream_contract_valid
            .store(u32::from(contract_valid), Ordering::Relaxed);
        header.stream_epoch.store(stream_epoch, Ordering::Relaxed);
        header.connect_events.fetch_add(1, Ordering::Relaxed);
        if !contract_valid {
            header
                .contract_failure_events
                .fetch_add(1, Ordering::Relaxed);
        }
        finish_feedback_write(&header.telemetry_sequence, sequence);
    }

    pub(crate) fn publish_client_disconnected(&mut self, stream_epoch: u64) {
        let header = self.header_mut();
        let sequence = begin_feedback_write(&header.telemetry_sequence);
        header
            .client_state
            .store(CLIENT_STATE_WAITING, Ordering::Relaxed);
        header.stream_contract_valid.store(0, Ordering::Relaxed);
        header.stream_epoch.store(stream_epoch, Ordering::Relaxed);
        header.disconnect_events.fetch_add(1, Ordering::Relaxed);
        finish_feedback_write(&header.telemetry_sequence, sequence);
    }

    pub(crate) fn publish_frame_transported(&mut self, stream_epoch: u64) -> bool {
        let header = self.header_mut();
        if header.stream_epoch.load(Ordering::Acquire) != stream_epoch
            || header.stream_contract_valid.load(Ordering::Acquire) == 0
        {
            return false;
        }
        let sequence = begin_feedback_write(&header.telemetry_sequence);
        header
            .client_state
            .store(CLIENT_STATE_STREAMING, Ordering::Relaxed);
        header.frames_transported.fetch_add(1, Ordering::Relaxed);
        finish_feedback_write(&header.telemetry_sequence, sequence);

        true
    }

    pub(crate) fn reset(&mut self) {
        let header = self.header_mut();
        let write_sequence = begin_feedback_write(&header.hmd_pose_sequence);
        header.view_config_set.store(0, Ordering::Relaxed);
        header.hmd_pose_set.store(0, Ordering::Relaxed);
        finish_feedback_write(&header.hmd_pose_sequence, write_sequence);
        reset_controllers(header);
    }

    pub(crate) fn publish_view_params(&mut self, params: [ViewParams; 2]) -> bool {
        if !valid_view_params(params) {
            return false;
        }

        let view_fov = params.map(|params| {
            [
                params.fov.left,
                params.fov.right,
                params.fov.up,
                params.fov.down,
            ]
        });
        let view_eye_x_m = params.map(|params| params.pose.position.x);
        let header = self.header_mut();
        let write_sequence = begin_feedback_write(&header.hmd_pose_sequence);
        unsafe {
            ptr::write_volatile(ptr::addr_of_mut!(header.view_fov), view_fov);
            ptr::write_volatile(ptr::addr_of_mut!(header.view_eye_x_m), view_eye_x_m);
        }
        header.view_config_set.store(1, Ordering::Relaxed);
        finish_feedback_write(&header.hmd_pose_sequence, write_sequence);

        true
    }

    pub(crate) fn publish_hmd_pose(&mut self, timestamp: Duration, pose: Pose) -> bool {
        if !valid_pose(pose) {
            return false;
        }

        let matrix = pose_to_matrix34(pose);
        let header = self.header_mut();
        let write_sequence = begin_feedback_write(&header.hmd_pose_sequence);
        unsafe {
            ptr::write_volatile(ptr::addr_of_mut!(header.hmd_pose), matrix);
            ptr::write_volatile(
                ptr::addr_of_mut!(header.hmd_pose_timestamp_ns),
                timestamp.as_nanos() as u64,
            );
        }
        header.hmd_pose_set.store(1, Ordering::Relaxed);
        finish_feedback_write(&header.hmd_pose_sequence, write_sequence);

        true
    }

    pub(crate) fn publish_controller_motion(
        &mut self,
        controller_index: usize,
        timestamp: Duration,
        motion: DeviceMotion,
    ) -> bool {
        if controller_index >= NUM_CONTROLLERS
            || !valid_pose(motion.pose)
            || !motion.linear_velocity.is_finite()
            || !motion.angular_velocity.is_finite()
        {
            return false;
        }

        let pose = pose_to_matrix34(motion.pose);
        let now = unix_time_ns();
        let controller = &mut self.header_mut().controllers[controller_index];
        let write_sequence = begin_feedback_write(&controller.sequence);
        controller.packet_number = controller.packet_number.wrapping_add(1).max(1);
        controller.tracking_timestamp_ns = timestamp.as_nanos() as u64;
        controller.motion_update_wall_ns = now;
        controller.pose = pose;
        controller.linear_velocity = motion.linear_velocity.to_array();
        controller.angular_velocity = motion.angular_velocity.to_array();
        controller.connected.store(1, Ordering::Relaxed);
        finish_feedback_write(&controller.sequence, write_sequence);

        true
    }

    pub(crate) fn publish_buttons(&mut self, entries: &[ButtonEntry]) -> usize {
        let now = unix_time_ns();
        let mut updated_controllers = 0;
        for controller_index in 0..NUM_CONTROLLERS {
            let mut matching_entries = entries.iter().filter(|entry| {
                controller_index_for_button(entry.path_id) == Some(controller_index)
            });
            let Some(first_entry) = matching_entries.next() else {
                continue;
            };

            let controller = &mut self.header_mut().controllers[controller_index];
            let write_sequence = begin_feedback_write(&controller.sequence);
            let mut changed = apply_button_entry(controller, first_entry);
            for entry in matching_entries {
                changed |= apply_button_entry(controller, entry);
            }
            if changed {
                controller.packet_number = controller.packet_number.wrapping_add(1).max(1);
                controller.input_update_wall_ns = now;
                updated_controllers += 1;
            }
            finish_feedback_write(&controller.sequence, write_sequence);
        }

        updated_controllers
    }

    fn header_mut(&mut self) -> &mut SharedMemoryHeader {
        unsafe { &mut *(self.mmap.as_mut_ptr() as *mut SharedMemoryHeader) }
    }
}

fn reset_controllers(header: &mut SharedMemoryHeader) {
    for controller in &mut header.controllers {
        let write_sequence = begin_feedback_write(&controller.sequence);
        controller.connected.store(0, Ordering::Relaxed);
        controller.packet_number = 0;
        controller.reserved = 0;
        controller.tracking_timestamp_ns = 0;
        controller.motion_update_wall_ns = 0;
        controller.input_update_wall_ns = 0;
        controller.pose = [[0.0; 4]; 3];
        controller.linear_velocity = [0.0; 3];
        controller.angular_velocity = [0.0; 3];
        controller.buttons_pressed = 0;
        controller.buttons_touched = 0;
        controller.axes = [[0.0; 2]; 5];
        controller.padding = [0; 8];
        finish_feedback_write(&controller.sequence, write_sequence);
    }
}

fn controller_index_for_button(path_id: u64) -> Option<usize> {
    let device_id = inp::BUTTON_INFO.get(&path_id)?.device_id;
    if device_id == *inp::HAND_LEFT_ID {
        Some(0)
    } else if device_id == *inp::HAND_RIGHT_ID {
        Some(1)
    } else {
        None
    }
}

fn apply_button_entry(controller: &mut ControllerStateRaw, entry: &ButtonEntry) -> bool {
    let path_id = entry.path_id;
    if is_button_pair(
        path_id,
        *inp::LEFT_SYSTEM_CLICK_ID,
        *inp::RIGHT_SYSTEM_CLICK_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_pressed,
            BUTTON_SYSTEM,
            button_active(entry.value),
        );
    }
    if is_button_pair(path_id, *inp::LEFT_MENU_CLICK_ID, *inp::RIGHT_MENU_CLICK_ID)
        || is_button_pair(path_id, *inp::LEFT_Y_CLICK_ID, *inp::RIGHT_B_CLICK_ID)
    {
        return set_button_mask(
            &mut controller.buttons_pressed,
            BUTTON_APPLICATION_MENU,
            button_active(entry.value),
        );
    }
    if is_button_pair(path_id, *inp::LEFT_X_CLICK_ID, *inp::RIGHT_A_CLICK_ID) {
        return set_button_mask(
            &mut controller.buttons_pressed,
            BUTTON_A,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_SQUEEZE_CLICK_ID,
        *inp::RIGHT_SQUEEZE_CLICK_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_pressed,
            BUTTON_GRIP,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_TRIGGER_CLICK_ID,
        *inp::RIGHT_TRIGGER_CLICK_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_pressed,
            BUTTON_TRIGGER,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_THUMBSTICK_CLICK_ID,
        *inp::RIGHT_THUMBSTICK_CLICK_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_pressed,
            BUTTON_TOUCHPAD,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_SYSTEM_TOUCH_ID,
        *inp::RIGHT_SYSTEM_TOUCH_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_touched,
            BUTTON_SYSTEM,
            button_active(entry.value),
        );
    }
    if is_button_pair(path_id, *inp::LEFT_MENU_TOUCH_ID, *inp::RIGHT_MENU_TOUCH_ID)
        || is_button_pair(path_id, *inp::LEFT_Y_TOUCH_ID, *inp::RIGHT_B_TOUCH_ID)
    {
        return set_button_mask(
            &mut controller.buttons_touched,
            BUTTON_APPLICATION_MENU,
            button_active(entry.value),
        );
    }
    if is_button_pair(path_id, *inp::LEFT_X_TOUCH_ID, *inp::RIGHT_A_TOUCH_ID) {
        return set_button_mask(
            &mut controller.buttons_touched,
            BUTTON_A,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_SQUEEZE_TOUCH_ID,
        *inp::RIGHT_SQUEEZE_TOUCH_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_touched,
            BUTTON_GRIP,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_TRIGGER_TOUCH_ID,
        *inp::RIGHT_TRIGGER_TOUCH_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_touched,
            BUTTON_TRIGGER,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_THUMBSTICK_TOUCH_ID,
        *inp::RIGHT_THUMBSTICK_TOUCH_ID,
    ) {
        return set_button_mask(
            &mut controller.buttons_touched,
            BUTTON_TOUCHPAD,
            button_active(entry.value),
        );
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_TRIGGER_VALUE_ID,
        *inp::RIGHT_TRIGGER_VALUE_ID,
    ) {
        let value = scalar_value(entry.value, false);
        let axis_changed = set_axis(&mut controller.axes[1][0], value);
        return set_button_mask(
            &mut controller.buttons_pressed,
            BUTTON_TRIGGER,
            value >= 0.5,
        ) || axis_changed;
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_SQUEEZE_VALUE_ID,
        *inp::RIGHT_SQUEEZE_VALUE_ID,
    ) {
        let value = scalar_value(entry.value, false);
        let axis_changed = set_axis(&mut controller.axes[2][0], value);
        return set_button_mask(&mut controller.buttons_pressed, BUTTON_GRIP, value >= 0.5)
            || axis_changed;
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_THUMBSTICK_X_ID,
        *inp::RIGHT_THUMBSTICK_X_ID,
    ) {
        return set_axis(&mut controller.axes[0][0], scalar_value(entry.value, true));
    }
    if is_button_pair(
        path_id,
        *inp::LEFT_THUMBSTICK_Y_ID,
        *inp::RIGHT_THUMBSTICK_Y_ID,
    ) {
        return set_axis(&mut controller.axes[0][1], scalar_value(entry.value, true));
    }

    false
}

fn is_button_pair(path_id: u64, left: u64, right: u64) -> bool {
    path_id == left || path_id == right
}

fn button_active(value: ButtonValue) -> bool {
    match value {
        ButtonValue::Binary(value) => value,
        ButtonValue::Scalar(value) => value.is_finite() && value >= 0.5,
    }
}

fn scalar_value(value: ButtonValue, signed: bool) -> f32 {
    let value = match value {
        ButtonValue::Binary(value) => {
            if value {
                1.0
            } else {
                0.0
            }
        }
        ButtonValue::Scalar(value) if value.is_finite() => value,
        ButtonValue::Scalar(_) => 0.0,
    };
    if signed {
        value.clamp(-1.0, 1.0)
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn set_button_mask(mask: &mut u64, button: u64, active: bool) -> bool {
    let previous = *mask;
    if active {
        *mask |= button;
    } else {
        *mask &= !button;
    }
    *mask != previous
}

fn set_axis(axis: &mut f32, value: f32) -> bool {
    if axis.to_bits() == value.to_bits() {
        false
    } else {
        *axis = value;
        true
    }
}

impl Drop for TrackingFeedback {
    fn drop(&mut self) {
        let header = self.header_mut();
        let sequence = begin_feedback_write(&header.telemetry_sequence);
        header
            .client_state
            .store(CLIENT_STATE_WAITING, Ordering::Relaxed);
        header.stream_contract_valid.store(0, Ordering::Relaxed);
        finish_feedback_write(&header.telemetry_sequence, sequence);
        header
            .bridge_heartbeat_ns
            .store(unix_time_ns(), Ordering::Relaxed);
        header.shutdown.store(1, Ordering::Release);
    }
}

fn begin_feedback_write(sequence: &AtomicU32) -> u32 {
    let sequence_value = sequence.load(Ordering::Relaxed);
    let write_sequence = if sequence_value.is_multiple_of(2) {
        sequence_value.wrapping_add(1)
    } else {
        sequence_value.wrapping_add(2)
    };
    sequence.store(write_sequence, Ordering::SeqCst);
    fence(Ordering::SeqCst);

    write_sequence
}

fn finish_feedback_write(sequence: &AtomicU32, write_sequence: u32) {
    fence(Ordering::Release);
    sequence.store(write_sequence.wrapping_add(1), Ordering::Release);
}

fn valid_view_params(params: [ViewParams; 2]) -> bool {
    params[0].pose.position.x < params[1].pose.position.x
        && params.iter().all(|params| {
            valid_pose(params.pose)
                && params.pose.position.x.abs() <= 0.2
                && params.fov.left < -0.001
                && params.fov.right > 0.001
                && params.fov.up > 0.001
                && params.fov.down < -0.001
                && [
                    params.fov.left,
                    params.fov.right,
                    params.fov.up,
                    params.fov.down,
                ]
                .into_iter()
                .all(|value| value.is_finite() && value.abs() < std::f32::consts::FRAC_PI_2)
        })
}

fn valid_pose(pose: Pose) -> bool {
    let orientation_len = pose.orientation.length_squared();
    pose.position.is_finite()
        && pose.orientation.is_finite()
        && (0.5..=1.5).contains(&orientation_len)
}

fn pose_to_matrix34(pose: Pose) -> [[f32; 4]; 3] {
    let columns =
        Mat4::from_rotation_translation(pose.orientation, pose.position).to_cols_array_2d();
    [
        [columns[0][0], columns[1][0], columns[2][0], columns[3][0]],
        [columns[0][1], columns[1][1], columns[2][1], columns[3][1]],
        [columns[0][2], columns[1][2], columns[2][2], columns[3][2]],
    ]
}

fn unix_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_common::{
        Fov,
        glam::{Quat, Vec3},
    };
    use std::fs;

    #[test]
    fn publishes_openvr_view_and_pose_feedback() {
        let path = std::env::temp_dir().join(format!(
            "alvr-tracking-feedback-{}-{}",
            process::id(),
            unix_time_ns()
        ));
        let mut feedback = TrackingFeedback::create_at(&path, 42).unwrap();
        let params = [
            ViewParams {
                pose: Pose {
                    orientation: Quat::IDENTITY,
                    position: Vec3::new(-0.032, 0.0, 0.0),
                },
                fov: Fov {
                    left: -1.0,
                    right: 0.9,
                    up: 0.8,
                    down: -0.7,
                },
            },
            ViewParams {
                pose: Pose {
                    orientation: Quat::IDENTITY,
                    position: Vec3::new(0.032, 0.0, 0.0),
                },
                fov: Fov {
                    left: -0.9,
                    right: 1.0,
                    up: 0.8,
                    down: -0.7,
                },
            },
        ];
        let pose = Pose {
            orientation: Quat::IDENTITY,
            position: Vec3::new(1.0, 2.0, 3.0),
        };

        assert!(feedback.publish_view_params(params));
        assert!(feedback.publish_hmd_pose(Duration::from_nanos(123), pose));

        let header = feedback.header_mut();
        assert_eq!(header.magic, SHM_MAGIC);
        assert_eq!(header.version, SHM_VERSION);
        assert_eq!(header.initialized.load(Ordering::Acquire), 1);
        assert_eq!(header.shutdown.load(Ordering::Acquire), 0);
        assert_eq!(header.runtime_generation.load(Ordering::Acquire), 42);
        assert_eq!(
            header.bridge_pid.load(Ordering::Acquire),
            u64::from(process::id())
        );
        assert_eq!(
            header.client_state.load(Ordering::Acquire),
            CLIENT_STATE_WAITING
        );
        assert_eq!(header.view_config_set.load(Ordering::Acquire), 1);
        assert_eq!(header.view_eye_x_m, [-0.032, 0.032]);
        assert_eq!(header.hmd_pose_set.load(Ordering::Acquire), 1);
        assert!(
            header
                .hmd_pose_sequence
                .load(Ordering::Acquire)
                .is_multiple_of(2)
        );
        assert_eq!(header.hmd_pose_timestamp_ns, 123);
        assert_eq!(header.hmd_pose[0][3], 1.0);
        assert_eq!(header.hmd_pose[1][3], 2.0);
        assert_eq!(header.hmd_pose[2][3], 3.0);

        drop(feedback);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn publishes_controller_motion_and_legacy_input_state() {
        let path = std::env::temp_dir().join(format!(
            "alvr-controller-feedback-{}-{}",
            process::id(),
            unix_time_ns()
        ));
        let mut feedback = TrackingFeedback::create_at(&path, 43).unwrap();
        let motion = DeviceMotion {
            pose: Pose {
                orientation: Quat::IDENTITY,
                position: Vec3::new(-0.25, 1.1, -0.4),
            },
            linear_velocity: Vec3::new(1.0, 2.0, 3.0),
            angular_velocity: Vec3::new(0.1, 0.2, 0.3),
        };
        let entries = [
            ButtonEntry {
                path_id: *inp::LEFT_THUMBSTICK_X_ID,
                value: ButtonValue::Scalar(-0.75),
            },
            ButtonEntry {
                path_id: *inp::LEFT_THUMBSTICK_Y_ID,
                value: ButtonValue::Scalar(0.5),
            },
            ButtonEntry {
                path_id: *inp::LEFT_TRIGGER_VALUE_ID,
                value: ButtonValue::Scalar(0.8),
            },
            ButtonEntry {
                path_id: *inp::LEFT_X_CLICK_ID,
                value: ButtonValue::Binary(true),
            },
        ];

        assert!(feedback.publish_controller_motion(0, Duration::from_nanos(456), motion));
        assert_eq!(feedback.publish_buttons(&entries), 1);

        {
            let controller = &feedback.header_mut().controllers[0];
            assert_eq!(controller.connected.load(Ordering::Acquire), 1);
            assert!(
                controller
                    .sequence
                    .load(Ordering::Acquire)
                    .is_multiple_of(2)
            );
            assert!(controller.packet_number >= 2);
            assert_eq!(controller.tracking_timestamp_ns, 456);
            assert!(controller.motion_update_wall_ns > 0);
            assert!(controller.input_update_wall_ns > 0);
            assert_eq!(controller.pose[0][3], -0.25);
            assert_eq!(controller.pose[1][3], 1.1);
            assert_eq!(controller.pose[2][3], -0.4);
            assert_eq!(controller.linear_velocity, [1.0, 2.0, 3.0]);
            assert_eq!(controller.angular_velocity, [0.1, 0.2, 0.3]);
            assert_eq!(controller.axes[0], [-0.75, 0.5]);
            assert_eq!(controller.axes[1][0], 0.8);
            assert_ne!(controller.buttons_pressed & BUTTON_TRIGGER, 0);
            assert_ne!(controller.buttons_pressed & BUTTON_A, 0);
        }

        feedback.reset();
        assert_eq!(
            feedback.header_mut().controllers[0]
                .connected
                .load(Ordering::Acquire),
            0
        );

        drop(feedback);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn preserves_existing_mapping_extent() {
        let path = std::env::temp_dir().join(format!(
            "alvr-tracking-feedback-restart-{}-{}",
            process::id(),
            unix_time_ns()
        ));
        let file = File::create(&path).unwrap();
        file.set_len(4096).unwrap();
        drop(file);

        let feedback = TrackingFeedback::create_at(&path, 44).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), 4096);
        assert_eq!(metadata.mode() & 0o777, 0o600);

        drop(feedback);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn publishes_generation_bound_client_transitions() {
        let path = std::env::temp_dir().join(format!(
            "alvr-client-telemetry-{}-{}",
            process::id(),
            unix_time_ns()
        ));
        let mut feedback = TrackingFeedback::create_at(&path, 45).unwrap();

        feedback.publish_client_connected(1, true);
        {
            let header = feedback.header_mut();
            assert_eq!(
                header.client_state.load(Ordering::Acquire),
                CLIENT_STATE_CONNECTED
            );
            assert_eq!(header.stream_contract_valid.load(Ordering::Acquire), 1);
            assert_eq!(header.stream_epoch.load(Ordering::Acquire), 1);
            assert_eq!(header.frames_transported.load(Ordering::Acquire), 0);
            assert_eq!(header.connect_events.load(Ordering::Acquire), 1);
            assert_eq!(header.disconnect_events.load(Ordering::Acquire), 0);
            assert_eq!(header.contract_failure_events.load(Ordering::Acquire), 0);
            assert!(
                header
                    .telemetry_sequence
                    .load(Ordering::Acquire)
                    .is_multiple_of(2)
            );
        }

        assert!(feedback.publish_frame_transported(1));
        assert!(!feedback.publish_frame_transported(2));
        {
            let header = feedback.header_mut();
            assert_eq!(
                header.client_state.load(Ordering::Acquire),
                CLIENT_STATE_STREAMING
            );
            assert_eq!(header.frames_transported.load(Ordering::Acquire), 1);
        }

        feedback.publish_client_disconnected(2);
        {
            let header = feedback.header_mut();
            assert_eq!(
                header.client_state.load(Ordering::Acquire),
                CLIENT_STATE_WAITING
            );
            assert_eq!(header.stream_contract_valid.load(Ordering::Acquire), 0);
            assert_eq!(header.stream_epoch.load(Ordering::Acquire), 2);
            assert_eq!(header.frames_transported.load(Ordering::Acquire), 1);
            assert_eq!(header.connect_events.load(Ordering::Acquire), 1);
            assert_eq!(header.disconnect_events.load(Ordering::Acquire), 1);
        }

        drop(feedback);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn latches_stream_contract_failures_across_disconnect() {
        let path = std::env::temp_dir().join(format!(
            "alvr-client-contract-failure-{}-{}",
            process::id(),
            unix_time_ns()
        ));
        let mut feedback = TrackingFeedback::create_at(&path, 47).unwrap();

        feedback.publish_client_connected(1, false);
        feedback.publish_client_disconnected(2);

        let header = feedback.header_mut();
        assert_eq!(
            header.client_state.load(Ordering::Acquire),
            CLIENT_STATE_WAITING
        );
        assert_eq!(header.connect_events.load(Ordering::Acquire), 1);
        assert_eq!(header.disconnect_events.load(Ordering::Acquire), 1);
        assert_eq!(header.contract_failure_events.load(Ordering::Acquire), 1);

        drop(feedback);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_symlinked_shared_memory_path() {
        let target = std::env::temp_dir().join(format!(
            "alvr-client-telemetry-target-{}-{}",
            process::id(),
            unix_time_ns()
        ));
        let link = std::env::temp_dir().join(format!(
            "alvr-client-telemetry-link-{}-{}",
            process::id(),
            unix_time_ns()
        ));
        File::create(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(TrackingFeedback::create_at(&link, 46).is_err());

        fs::remove_file(link).unwrap();
        fs::remove_file(target).unwrap();
    }
}
