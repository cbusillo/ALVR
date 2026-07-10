use anyhow::{Context, Result, anyhow};

use alvr_common::{
    Pose, ViewParams,
    glam::{Mat4, Vec2, Vec3, Vec4},
};
use std::{ffi::c_void, ptr};

#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}

#[repr(C)]
struct VImageYpCbCrPixelRange {
    yp_bias: i32,
    cb_cr_bias: i32,
    yp_range_max: i32,
    cb_cr_range_max: i32,
    yp_max: i32,
    yp_min: i32,
    cb_cr_max: i32,
    cb_cr_min: i32,
}

#[repr(C, align(16))]
struct VImageArgbToYpCbCr {
    opaque: [u8; 128],
}

type VImageError = isize;
type VImageFlags = u32;
type CVPixelBufferRef = *mut c_void;
type CVReturn = i32;

const KV_IMAGE_NO_FLAGS: VImageFlags = 0;
const KV_IMAGE_ARGB_8888: u32 = 0;
const KV_IMAGE_420_YP8_CBCR8: u32 = 4;
const K_CV_PIXEL_FORMAT_TYPE_420V: u32 = u32::from_be_bytes(*b"420v");
const K_CV_RETURN_SUCCESS: CVReturn = 0;
const BGRA_PERMUTE_MAP: [u8; 4] = [3, 2, 1, 0];
const VIDEO_RANGE_8_BIT: VImageYpCbCrPixelRange = VImageYpCbCrPixelRange {
    yp_bias: 16,
    cb_cr_bias: 128,
    yp_range_max: 235,
    cb_cr_range_max: 240,
    yp_max: 255,
    yp_min: 0,
    cb_cr_max: 255,
    cb_cr_min: 1,
};

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    // Apple declares this as `const vImage_ARGBToYpCbCrMatrix *`, so the dynamic symbol stores the
    // matrix pointer value. Passing the address of this variable breaks vImage conversion.
    static kvImage_ARGBToYpCbCrMatrix_ITU_R_601_4: *const c_void;

    fn vImageConvert_ARGBToYpCbCr_GenerateConversion(
        matrix: *const c_void,
        pixel_range: *const VImageYpCbCrPixelRange,
        out_info: *mut VImageArgbToYpCbCr,
        in_argb_type: u32,
        out_yp_cb_cr_type: u32,
        flags: VImageFlags,
    ) -> VImageError;

    fn vImageConvert_ARGB8888To420Yp8_CbCr8(
        src: *const VImageBuffer,
        dest_yp: *const VImageBuffer,
        dest_cb_cr: *const VImageBuffer,
        info: *const VImageArgbToYpCbCr,
        permute_map: *const u8,
        flags: VImageFlags,
    ) -> VImageError;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFBooleanTrue: *const c_void;

    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferIOSurfacePropertiesKey: *const c_void;
    static kCVPixelBufferMetalCompatibilityKey: *const c_void;

    fn CVPixelBufferCreate(
        allocator: *const c_void,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_attributes: *const c_void,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> CVReturn;

    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> CVReturn;
    fn CVPixelBufferUnlockBaseAddress(
        pixel_buffer: CVPixelBufferRef,
        unlock_flags: u64,
    ) -> CVReturn;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
}

pub struct Nv12PixelBuffer {
    ptr: CVPixelBufferRef,
}

impl Nv12PixelBuffer {
    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    pub fn fill_test_pattern(
        &self,
        width: u32,
        height: u32,
        frame_index: u64,
        right_eye_shift_x_px: i32,
    ) -> Result<()> {
        let width = width as usize;
        let height = height as usize;
        if width % 2 != 0 || height % 2 != 0 {
            return Err(anyhow!("NV12 requires even dimensions: {width}x{height}"));
        }

        let _lock = lock_pixel_buffer(self.ptr)?;
        let y_plane = pixel_buffer_plane(self.ptr, 0)?;
        let uv_plane = pixel_buffer_plane(self.ptr, 1)?;
        validate_plane_layout(&y_plane, 0, width, height, width)?;
        validate_plane_layout(&uv_plane, 1, width / 2, height / 2, width)?;

        fill_y_plane(&y_plane, width, height, frame_index, right_eye_shift_x_px);
        fill_uv_plane(&uv_plane, width, height, frame_index);
        Ok(())
    }

    pub fn fill_world_locked_diagnostic(
        &self,
        width: u32,
        height: u32,
        _frame_index: u64,
        hmd_pose: Pose,
        anchor_pose: Pose,
        view_params: [ViewParams; 2],
        forward_z_sign: f32,
    ) -> Result<()> {
        let width = width as usize;
        let height = height as usize;
        if width % 2 != 0 || height % 2 != 0 {
            return Err(anyhow!("NV12 requires even dimensions: {width}x{height}"));
        }

        let _lock = lock_pixel_buffer(self.ptr)?;
        let y_plane = pixel_buffer_plane(self.ptr, 0)?;
        let uv_plane = pixel_buffer_plane(self.ptr, 1)?;
        validate_plane_layout(&y_plane, 0, width, height, width)?;
        validate_plane_layout(&uv_plane, 1, width / 2, height / 2, width)?;

        clear_y_plane(&y_plane, width, height, 28);
        clear_uv_plane(&uv_plane, width, height, 128);
        draw_world_locked_scene(
            &y_plane,
            width,
            height,
            hmd_pose,
            anchor_pose,
            view_params,
            forward_z_sign,
        );
        Ok(())
    }
}

impl Clone for Nv12PixelBuffer {
    fn clone(&self) -> Self {
        let ptr = unsafe { CFRetain(self.ptr.cast_const()) }.cast_mut();
        Self { ptr }
    }
}

impl Drop for Nv12PixelBuffer {
    fn drop(&mut self) {
        unsafe { CFRelease(self.ptr.cast_const()) }
    }
}

struct PixelBufferLockGuard {
    pixel_buffer: CVPixelBufferRef,
}

struct CfObject {
    ptr: *const c_void,
}

impl CfObject {
    fn dictionary(
        keys: &[*const c_void],
        values: &[*const c_void],
        description: &str,
    ) -> Result<Self> {
        anyhow::ensure!(
            keys.len() == values.len(),
            "CFDictionaryCreate key/value length mismatch"
        );
        let ptr = unsafe {
            CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as isize,
                ptr::addr_of!(kCFTypeDictionaryKeyCallBacks),
                ptr::addr_of!(kCFTypeDictionaryValueCallBacks),
            )
        };
        if ptr.is_null() {
            return Err(anyhow!(
                "CFDictionaryCreate returned null for {description}"
            ));
        }

        Ok(Self { ptr })
    }

    fn as_ptr(&self) -> *const c_void {
        self.ptr
    }
}

impl Drop for CfObject {
    fn drop(&mut self) {
        unsafe { CFRelease(self.ptr) }
    }
}

impl Drop for PixelBufferLockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CVPixelBufferUnlockBaseAddress(self.pixel_buffer, 0);
        }
    }
}

pub struct Nv12Frame {
    width: usize,
    height: usize,
    conversion: VImageArgbToYpCbCr,
}

impl Nv12Frame {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let width = width as usize;
        let height = height as usize;
        if width % 2 != 0 || height % 2 != 0 {
            return Err(anyhow!("NV12 requires even dimensions: {width}x{height}"));
        }
        let mut frame = Self {
            width,
            height,
            conversion: VImageArgbToYpCbCr { opaque: [0; 128] },
        };

        let status = unsafe {
            vImageConvert_ARGBToYpCbCr_GenerateConversion(
                kvImage_ARGBToYpCbCrMatrix_ITU_R_601_4,
                &VIDEO_RANGE_8_BIT,
                &mut frame.conversion,
                KV_IMAGE_ARGB_8888,
                KV_IMAGE_420_YP8_CBCR8,
                KV_IMAGE_NO_FLAGS,
            )
        };
        if status != 0 {
            return Err(anyhow!(
                "vImageConvert_ARGBToYpCbCr_GenerateConversion failed: {status}"
            ));
        }

        Ok(frame)
    }

    pub fn pixel_buffer_from_bgra(
        &self,
        bgra: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Nv12PixelBuffer> {
        let width = width as usize;
        let height = height as usize;
        let stride = stride as usize;

        if width != self.width || height != self.height {
            return Err(anyhow!(
                "frame shape changed: {width}x{height} != {}x{}",
                self.width,
                self.height
            ));
        }
        if stride < width.saturating_mul(4) {
            return Err(anyhow!(
                "BGRA stride too small: {stride} < expected {}",
                width * 4
            ));
        }

        let needed = (height.saturating_sub(1))
            .saturating_mul(stride)
            .saturating_add(width.saturating_mul(4));
        if bgra.len() < needed {
            return Err(anyhow!(
                "BGRA buffer too small: {} < expected {needed}",
                bgra.len()
            ));
        }

        let pixel_buffer = create_nv12_pixel_buffer(width, height)?;
        let lock = lock_pixel_buffer(pixel_buffer.ptr)?;
        let y_plane = pixel_buffer_plane(pixel_buffer.ptr, 0)?;
        let uv_plane = pixel_buffer_plane(pixel_buffer.ptr, 1)?;
        validate_plane_layout(&y_plane, 0, width, height, width)?;
        validate_plane_layout(&uv_plane, 1, width / 2, height / 2, width)?;

        let src = VImageBuffer {
            data: bgra.as_ptr() as *mut c_void,
            height,
            width,
            row_bytes: stride,
        };
        let dest_y = VImageBuffer {
            data: y_plane.data,
            height,
            width,
            row_bytes: y_plane.row_bytes,
        };
        let dest_uv = VImageBuffer {
            data: uv_plane.data,
            height: height / 2,
            width: width / 2,
            row_bytes: uv_plane.row_bytes,
        };

        let status = unsafe {
            vImageConvert_ARGB8888To420Yp8_CbCr8(
                &src,
                &dest_y,
                &dest_uv,
                &self.conversion,
                BGRA_PERMUTE_MAP.as_ptr(),
                KV_IMAGE_NO_FLAGS,
            )
        };
        if status != 0 {
            return Err(anyhow!(
                "vImageConvert_ARGB8888To420Yp8_CbCr8 failed: {status}"
            ));
        }

        drop(lock);
        Ok(pixel_buffer)
    }

    pub fn pixel_buffer_from_nv12_planes(&self, y: &[u8], uv: &[u8]) -> Result<Nv12PixelBuffer> {
        let expected_y = self.width * self.height;
        let expected_uv = self.width * self.height / 2;
        if y.len() < expected_y {
            return Err(anyhow!(
                "Y plane too small: {} < expected {expected_y}",
                y.len()
            ));
        }
        if uv.len() < expected_uv {
            return Err(anyhow!(
                "UV plane too small: {} < expected {expected_uv}",
                uv.len()
            ));
        }

        let pixel_buffer = create_nv12_pixel_buffer(self.width, self.height)?;
        let lock = lock_pixel_buffer(pixel_buffer.ptr)?;
        copy_plane_to_pixel_buffer(pixel_buffer.ptr, 0, y, self.width, self.width, self.height)?;
        copy_plane_to_pixel_buffer(
            pixel_buffer.ptr,
            1,
            uv,
            self.width,
            self.width,
            self.height / 2,
        )?;
        drop(lock);
        Ok(pixel_buffer)
    }
}

struct PixelBufferPlane {
    data: *mut c_void,
    width: usize,
    height: usize,
    row_bytes: usize,
}

fn create_nv12_pixel_buffer(width: usize, height: usize) -> Result<Nv12PixelBuffer> {
    let mut pixel_buffer = ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            ptr::null(),
            width,
            height,
            K_CV_PIXEL_FORMAT_TYPE_420V,
            ptr::null(),
            &mut pixel_buffer,
        )
    };
    cv_check(status, "CVPixelBufferCreate")?;
    if pixel_buffer.is_null() {
        return Err(anyhow!("CVPixelBufferCreate returned a null pixel buffer"));
    }

    Ok(Nv12PixelBuffer { ptr: pixel_buffer })
}

pub fn create_iosurface_nv12_pixel_buffer(width: u32, height: u32) -> Result<Nv12PixelBuffer> {
    let width = width as usize;
    let height = height as usize;
    if width % 2 != 0 || height % 2 != 0 {
        return Err(anyhow!("NV12 requires even dimensions: {width}x{height}"));
    }

    let empty_iosurface_properties = CfObject::dictionary(&[], &[], "IOSurface properties")?;
    let attributes = {
        let keys = unsafe {
            [
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ]
        };
        let values = unsafe { [empty_iosurface_properties.as_ptr(), kCFBooleanTrue] };
        CfObject::dictionary(&keys, &values, "CVPixelBuffer attributes")?
    };

    let mut pixel_buffer = ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            ptr::null(),
            width,
            height,
            K_CV_PIXEL_FORMAT_TYPE_420V,
            attributes.as_ptr(),
            &mut pixel_buffer,
        )
    };

    cv_check(status, "CVPixelBufferCreate(IOSurface NV12)")?;
    if pixel_buffer.is_null() {
        return Err(anyhow!(
            "CVPixelBufferCreate(IOSurface NV12) returned a null pixel buffer"
        ));
    }

    Ok(Nv12PixelBuffer { ptr: pixel_buffer })
}

fn lock_pixel_buffer(pixel_buffer: CVPixelBufferRef) -> Result<PixelBufferLockGuard> {
    let status = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
    cv_check(status, "CVPixelBufferLockBaseAddress")?;
    Ok(PixelBufferLockGuard { pixel_buffer })
}

fn pixel_buffer_plane(
    pixel_buffer: CVPixelBufferRef,
    plane_index: usize,
) -> Result<PixelBufferPlane> {
    let data = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, plane_index) };
    if data.is_null() {
        return Err(anyhow!(
            "CVPixelBufferGetBaseAddressOfPlane({plane_index}) returned null"
        ));
    }

    Ok(PixelBufferPlane {
        data,
        width: unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, plane_index) },
        height: unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, plane_index) },
        row_bytes: unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, plane_index) },
    })
}

fn copy_plane_to_pixel_buffer(
    pixel_buffer: CVPixelBufferRef,
    plane_index: usize,
    src: &[u8],
    src_row_bytes: usize,
    copy_bytes_per_row: usize,
    src_height: usize,
) -> Result<()> {
    let plane = pixel_buffer_plane(pixel_buffer, plane_index)?;
    if plane.height != src_height || plane.row_bytes < copy_bytes_per_row {
        return Err(anyhow!(
            "unexpected CVPixelBuffer plane {plane_index}: {}x{} row_bytes={} for source row_bytes={src_row_bytes} copy_bytes_per_row={copy_bytes_per_row} height={src_height}",
            plane.width,
            plane.height,
            plane.row_bytes
        ));
    }

    let required = src_row_bytes
        .checked_mul(src_height)
        .context("source plane size overflow")?;
    if src.len() < required {
        return Err(anyhow!(
            "source plane {plane_index} too small: {} < expected {required}",
            src.len()
        ));
    }

    for row in 0..src_height {
        let src_start = row * src_row_bytes;
        let dst_start = row * plane.row_bytes;
        unsafe {
            ptr::copy_nonoverlapping(
                src.as_ptr().add(src_start),
                plane.data.cast::<u8>().add(dst_start),
                copy_bytes_per_row,
            );
        }
    }

    Ok(())
}

fn validate_plane_layout(
    plane: &PixelBufferPlane,
    plane_index: usize,
    expected_width: usize,
    expected_height: usize,
    min_row_bytes: usize,
) -> Result<()> {
    if plane.width != expected_width
        || plane.height != expected_height
        || plane.row_bytes < min_row_bytes
    {
        return Err(anyhow!(
            "unexpected CVPixelBuffer plane {plane_index}: {}x{} row_bytes={} expected {}x{} row_bytes>={min_row_bytes}",
            plane.width,
            plane.height,
            plane.row_bytes,
            expected_width,
            expected_height
        ));
    }

    Ok(())
}

fn fill_y_plane(
    plane: &PixelBufferPlane,
    width: usize,
    height: usize,
    _frame_index: u64,
    right_eye_shift_x_px: i32,
) {
    let eye_width = width / 2;
    let center_x = eye_width / 2;
    let center_y = height / 2;
    let marker_top = 64.min(height.saturating_sub(1));
    let marker_left = 64.min(eye_width.saturating_sub(1));

    for y in 0..height {
        let row = unsafe { plane.data.cast::<u8>().add(y * plane.row_bytes) };
        unsafe { ptr::write_bytes(row, 56, width) };

        if y == 0 || y + 1 == height {
            unsafe { ptr::write_bytes(row, 235, width) };
            continue;
        }

        for eye in 0..2 {
            let eye_start = eye * eye_width;
            let content_shift = if eye == 1 { right_eye_shift_x_px } else { 0 };
            let shifted_center_x = shifted_coord(center_x, content_shift, eye_width);

            unsafe {
                *row.add(eye_start) = 235;
                *row.add((eye_start + eye_width).saturating_sub(1)) = 235;
            }
            write_eye_row_segment(
                row,
                eye_start,
                eye_width,
                shifted_center_x.saturating_sub(2),
                4,
                28,
            );

            if y % 160 < 2 {
                unsafe { ptr::write_bytes(row.add(eye_start), 200, eye_width) };
            }

            for grid_x in (0..eye_width).step_by(160) {
                let x = eye_start + grid_x;
                unsafe {
                    *row.add(x) = 200;
                    if grid_x + 1 < eye_width {
                        *row.add(x + 1) = 200;
                    }
                }
            }

            if y.abs_diff(center_y) < 3 {
                unsafe { ptr::write_bytes(row.add(eye_start), 235, eye_width) };
            }

            draw_center_boxes(row, eye_start, eye_width, shifted_center_x, center_y, y);
            draw_count_ticks(row, eye_start, eye_width, shifted_center_x, center_y, y);

            let shifted_marker_left = shifted_coord(marker_left, content_shift, eye_width);
            draw_eye_marker(
                row,
                eye_start,
                eye_width,
                shifted_marker_left,
                marker_top,
                y,
                eye == 0,
            );
        }
    }
}

fn shifted_coord(coord: usize, shift: i32, limit: usize) -> usize {
    if limit == 0 {
        return 0;
    }
    let shifted = coord as isize + shift as isize;
    shifted.clamp(0, limit.saturating_sub(1) as isize) as usize
}

fn draw_center_boxes(
    row: *mut u8,
    eye_start: usize,
    eye_width: usize,
    center_x: usize,
    center_y: usize,
    y: usize,
) {
    let box_size = 72.min(eye_width / 8).max(24);
    let gap = 28;
    let total = box_size * 5 + gap * 4;
    let base_x = center_x.saturating_sub(total / 2);
    let base_y = center_y.saturating_sub(box_size / 2);
    let Some(local_y) = y.checked_sub(base_y) else {
        return;
    };
    if local_y >= box_size {
        return;
    }

    for index in 0..5 {
        let local_x = base_x + index * (box_size + gap);
        let border = local_y < 5 || local_y + 5 >= box_size;
        let value = if index == 2 { 235 } else { 205 };
        if border {
            write_eye_row_segment(row, eye_start, eye_width, local_x, box_size, value);
        } else {
            write_eye_row_segment(row, eye_start, eye_width, local_x, 5, value);
            write_eye_row_segment(
                row,
                eye_start,
                eye_width,
                local_x + box_size.saturating_sub(5),
                5,
                value,
            );
        }
    }
}

fn draw_count_ticks(
    row: *mut u8,
    eye_start: usize,
    eye_width: usize,
    center_x: usize,
    center_y: usize,
    y: usize,
) {
    if y.abs_diff(center_y + 180) >= 8 {
        return;
    }

    let tick_width = 18.min(eye_width / 20).max(8);
    let gap = 52;
    let total = tick_width * 9 + gap * 8;
    let start = center_x.saturating_sub(total / 2);
    for index in 0..9 {
        let height_band = 2 + index % 5;
        if y % 16 < height_band {
            write_eye_row_segment(
                row,
                eye_start,
                eye_width,
                start + index * (tick_width + gap),
                tick_width,
                235,
            );
        }
    }
}

fn draw_eye_marker(
    row: *mut u8,
    eye_start: usize,
    eye_width: usize,
    start_x: usize,
    top_y: usize,
    y: usize,
    left_eye: bool,
) {
    let Some(local_y) = y.checked_sub(top_y) else {
        return;
    };
    if local_y >= 96 {
        return;
    }

    let draw_segment = |x0: usize, x1: usize| {
        write_eye_row_segment(
            row,
            eye_start,
            eye_width,
            start_x + x0,
            x1.saturating_sub(x0),
            235,
        );
    };

    if left_eye {
        if (82..96).contains(&local_y) {
            draw_segment(0, 62);
        }
        if local_y < 96 {
            draw_segment(0, 14);
        }
    } else {
        if (0..14).contains(&local_y) || (40..54).contains(&local_y) {
            draw_segment(0, 58);
        }
        if local_y < 96 {
            draw_segment(0, 14);
        }
        if local_y < 54 {
            draw_segment(44, 58);
        }
        if local_y >= 54 {
            let diag_x = 14 + (local_y - 54).min(41);
            draw_segment(diag_x, (diag_x + 16).min(64));
        }
    }
}

fn write_eye_row_segment(
    row: *mut u8,
    eye_start: usize,
    eye_width: usize,
    local_x: usize,
    len: usize,
    value: u8,
) {
    if local_x >= eye_width || len == 0 {
        return;
    }
    let clamped_len = len.min(eye_width - local_x);
    unsafe { ptr::write_bytes(row.add(eye_start + local_x), value, clamped_len) };
}

fn fill_uv_plane(plane: &PixelBufferPlane, width: usize, height: usize, _frame_index: u64) {
    let uv_height = height / 2;
    let center_y = uv_height / 2;

    for uv_y in 0..uv_height {
        let row = unsafe { plane.data.cast::<u8>().add(uv_y * plane.row_bytes) };
        unsafe { ptr::write_bytes(row, 128, width) };

        if uv_y.abs_diff(center_y) < 4 || uv_y % 80 == 0 {
            unsafe {
                ptr::write_bytes(row, 140, width);
            }
        }
    }
}

fn clear_y_plane(plane: &PixelBufferPlane, width: usize, height: usize, value: u8) {
    for y in 0..height {
        let row = unsafe { plane.data.cast::<u8>().add(y * plane.row_bytes) };
        unsafe { ptr::write_bytes(row, value, width) };
    }
}

fn clear_uv_plane(plane: &PixelBufferPlane, width: usize, height: usize, value: u8) {
    for uv_y in 0..height / 2 {
        let row = unsafe { plane.data.cast::<u8>().add(uv_y * plane.row_bytes) };
        unsafe { ptr::write_bytes(row, value, width) };
    }
}

fn draw_world_locked_scene(
    plane: &PixelBufferPlane,
    width: usize,
    height: usize,
    hmd_pose: Pose,
    anchor_pose: Pose,
    view_params: [ViewParams; 2],
    forward_z_sign: f32,
) {
    let up = Vec3::Y;
    let local_forward = if forward_z_sign >= 0.0 {
        Vec3::Z
    } else {
        Vec3::NEG_Z
    };
    let raw_forward = anchor_pose.orientation * local_forward;
    let forward = (raw_forward - up * raw_forward.dot(up))
        .try_normalize()
        .unwrap_or(Vec3::NEG_Z);
    let right = forward.cross(up).try_normalize().unwrap_or(Vec3::X);
    let origin = anchor_pose.position;
    let eye_height = origin - up * 1.35;

    let world = |x: f32, y: f32, z: f32| eye_height + right * x + up * y + forward * z;

    for eye in 0..2 {
        let eye_pose = hmd_pose * view_params[eye].pose;
        let eye_start_x = eye * (width / 2);
        let viewport = Viewport {
            start_x: eye_start_x,
            width: width / 2,
            height,
        };

        for x_step in -6..=6 {
            let x = x_step as f32 * 0.5;
            draw_projected_line(
                plane,
                viewport,
                eye_pose,
                view_params[eye],
                world(x, 0.0, 0.8),
                world(x, 0.0, 6.0),
                150,
            );
        }
        for z_step in 1..=12 {
            let z = z_step as f32 * 0.5;
            draw_projected_line(
                plane,
                viewport,
                eye_pose,
                view_params[eye],
                world(-3.0, 0.0, z),
                world(3.0, 0.0, z),
                if z_step == 4 { 220 } else { 150 },
            );
        }

        draw_wire_cube(
            plane,
            viewport,
            eye_pose,
            view_params[eye],
            |x, y, z| world(x, y + 1.35, z),
            Vec3::new(0.0, 0.0, 2.6),
            0.55,
            235,
        );
        draw_wire_cube(
            plane,
            viewport,
            eye_pose,
            view_params[eye],
            |x, y, z| world(x, y + 1.1, z),
            Vec3::new(-1.0, 0.0, 3.4),
            0.35,
            190,
        );
        draw_wire_cube(
            plane,
            viewport,
            eye_pose,
            view_params[eye],
            |x, y, z| world(x, y + 1.1, z),
            Vec3::new(1.0, 0.0, 3.4),
            0.35,
            190,
        );

        draw_projected_line(
            plane,
            viewport,
            eye_pose,
            view_params[eye],
            origin,
            origin + up * 0.8,
            235,
        );
        draw_projected_line(
            plane,
            viewport,
            eye_pose,
            view_params[eye],
            origin,
            origin + forward * 1.0,
            210,
        );
    }
}

#[derive(Clone, Copy)]
struct Viewport {
    start_x: usize,
    width: usize,
    height: usize,
}

fn draw_wire_cube(
    plane: &PixelBufferPlane,
    viewport: Viewport,
    eye_pose: Pose,
    view_params: ViewParams,
    to_world: impl Fn(f32, f32, f32) -> Vec3,
    center: Vec3,
    size: f32,
    value: u8,
) {
    let s = size * 0.5;
    let corners = [
        Vec3::new(-s, -s, -s),
        Vec3::new(s, -s, -s),
        Vec3::new(s, s, -s),
        Vec3::new(-s, s, -s),
        Vec3::new(-s, -s, s),
        Vec3::new(s, -s, s),
        Vec3::new(s, s, s),
        Vec3::new(-s, s, s),
    ];
    let edges = [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (4, 5),
        (5, 6),
        (6, 7),
        (7, 4),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ];

    for (a, b) in edges {
        let pa = center + corners[a];
        let pb = center + corners[b];
        draw_projected_line(
            plane,
            viewport,
            eye_pose,
            view_params,
            to_world(pa.x, pa.y, pa.z),
            to_world(pb.x, pb.y, pb.z),
            value,
        );
    }
}

fn draw_projected_line(
    plane: &PixelBufferPlane,
    viewport: Viewport,
    eye_pose: Pose,
    view_params: ViewParams,
    a_world: Vec3,
    b_world: Vec3,
    value: u8,
) {
    let view_mat = (Mat4::from_translation(eye_pose.position)
        * Mat4::from_quat(eye_pose.orientation))
    .inverse();
    let a_eye = view_mat.transform_point3(a_world);
    let b_eye = view_mat.transform_point3(b_world);
    let Some((a_eye, b_eye)) = clip_against_near(a_eye, b_eye, 0.1) else {
        return;
    };
    let projection = view_params.fov.to_wgpu_projection_matrix();
    let Some(a_px) = project_eye_point(a_eye, projection, viewport) else {
        return;
    };
    let Some(b_px) = project_eye_point(b_eye, projection, viewport) else {
        return;
    };

    draw_luma_line(plane, viewport, a_px, b_px, value);
}

fn clip_against_near(mut a: Vec3, mut b: Vec3, near: f32) -> Option<(Vec3, Vec3)> {
    let a_forward = -a.z;
    let b_forward = -b.z;
    if a_forward < near && b_forward < near {
        return None;
    }
    if a_forward < near || b_forward < near {
        let denominator = b_forward - a_forward;
        if denominator.abs() <= f32::EPSILON {
            return None;
        }
        let t = (near - a_forward) / denominator;
        let clipped = a + (b - a) * t.clamp(0.0, 1.0);
        if a_forward < near {
            a = clipped;
        } else {
            b = clipped;
        }
    }

    Some((a, b))
}

fn project_eye_point(point: Vec3, projection: Mat4, viewport: Viewport) -> Option<Vec2> {
    let clip = projection * Vec4::new(point.x, point.y, point.z, 1.0);
    if clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    if !ndc.x.is_finite() || !ndc.y.is_finite() {
        return None;
    }

    let max_x = (viewport.width.saturating_sub(1)) as f32;
    let max_y = (viewport.height.saturating_sub(1)) as f32;
    let x = viewport.start_x as f32 + (((ndc.x + 1.0) * 0.5) * max_x).clamp(-max_x, max_x * 2.0);
    let y = (((ndc.y + 1.0) * 0.5) * max_y).clamp(-max_y, max_y * 2.0);
    Some(Vec2::new(x, y))
}

fn draw_luma_line(plane: &PixelBufferPlane, viewport: Viewport, start: Vec2, end: Vec2, value: u8) {
    let mut x0 = start.x.round() as i32;
    let mut y0 = start.y.round() as i32;
    let x1 = end.x.round() as i32;
    let y1 = end.y.round() as i32;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let min_x = viewport.start_x as i32;
    let max_x = (viewport.start_x + viewport.width).saturating_sub(1) as i32;
    let max_y = viewport.height.saturating_sub(1) as i32;

    loop {
        if x0 >= min_x && x0 <= max_x && y0 >= 0 && y0 <= max_y {
            unsafe {
                *plane
                    .data
                    .cast::<u8>()
                    .add(y0 as usize * plane.row_bytes + x0 as usize) = value;
            }
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn cv_check(status: CVReturn, operation: &str) -> Result<()> {
    if status == K_CV_RETURN_SUCCESS {
        Ok(())
    } else {
        Err(anyhow!("{operation} failed: {status}"))
    }
}

pub fn fill_bgra_test_pattern(buffer: &mut [u8], width: u32, height: u32, frame_index: u64) {
    let width = width as usize;
    let height = height as usize;
    let eye_width = width / 2;
    let drift = (frame_index as usize * 4) % eye_width.max(1);

    for y in 0..height {
        for x in 0..width {
            let eye = usize::from(x >= eye_width);
            let local_x = if eye == 0 { x } else { x - eye_width };
            let idx = (y * width + x) * 4;

            let grid = local_x % 160 < 3 || y % 160 < 3;
            let center = local_x.abs_diff(eye_width / 2) < 4 || y.abs_diff(height / 2) < 4;
            let moving = local_x.abs_diff((drift + eye_width / 4) % eye_width) < 28
                && y.abs_diff(height / 2) < 70;
            let marker = if eye == 0 {
                local_x < 70 && y < 100
            } else {
                local_x + 70 > eye_width && y < 100
            };

            let (r, g, b) = if marker {
                (240, 240, 240)
            } else if moving {
                (240, 220, 80)
            } else if center {
                (40, 40, 40)
            } else if grid {
                (170, 170, 170)
            } else {
                let gradient = ((local_x + y + drift) % 180) as u8;
                (40 + gradient / 2, 80 + gradient / 3, 110 + gradient / 4)
            };

            buffer[idx] = b;
            buffer[idx + 1] = g;
            buffer[idx + 2] = r;
            buffer[idx + 3] = 255;
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn bgra_solid(width: usize, height: usize, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut pixels = vec![0; width * height * 4];
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[0] = b;
            pixel[1] = g;
            pixel[2] = r;
            pixel[3] = 255;
        }
        pixels
    }

    fn copy_plane(pixel_buffer: &Nv12PixelBuffer, plane_index: usize) -> Result<Vec<u8>> {
        let _lock = lock_pixel_buffer(pixel_buffer.ptr)?;
        let plane = pixel_buffer_plane(pixel_buffer.ptr, plane_index)?;
        let mut copy = vec![0; plane.width * plane.height];
        for row in 0..plane.height {
            unsafe {
                ptr::copy_nonoverlapping(
                    plane.data.cast::<u8>().add(row * plane.row_bytes),
                    copy.as_mut_ptr().add(row * plane.width),
                    plane.width,
                );
            }
        }
        Ok(copy)
    }

    fn y_mean_for_solid(b: u8, g: u8, r: u8) -> Result<f64> {
        let width = 8;
        let height = 8;
        let converter = Nv12Frame::new(width, height)?;
        let bgra = bgra_solid(width as usize, height as usize, b, g, r);
        let pixel_buffer = converter.pixel_buffer_from_bgra(&bgra, width, height, width * 4)?;
        let y = copy_plane(&pixel_buffer, 0)?;
        Ok(y.iter().map(|value| f64::from(*value)).sum::<f64>() / y.len() as f64)
    }

    #[test]
    fn bgra_to_nv12_luma_tracks_known_colors() -> Result<()> {
        let black = y_mean_for_solid(0, 0, 0)?;
        let white = y_mean_for_solid(255, 255, 255)?;
        let red = y_mean_for_solid(0, 0, 255)?;
        let green = y_mean_for_solid(0, 255, 0)?;
        let blue = y_mean_for_solid(255, 0, 0)?;

        assert!(
            black < 40.0,
            "black Y mean should be near video black, got {black}"
        );
        assert!(white > 210.0, "white Y mean should be bright, got {white}");
        assert!(
            white - black > 170.0,
            "white and black luma should be far apart, got black={black} white={white}"
        );
        assert!(
            blue > black + 10.0,
            "blue should not collapse to black, got black={black} blue={blue}"
        );
        assert!(
            red > blue && green > red,
            "BT.601 luma ordering should be green > red > blue, got green={green} red={red} blue={blue}"
        );

        Ok(())
    }
}
