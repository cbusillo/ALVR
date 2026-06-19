use anyhow::{Context, Result, anyhow};

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
    static kvImage_ARGBToYpCbCrMatrix_ITU_R_601_4: [f32; 9];

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
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
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
}

impl Drop for Nv12PixelBuffer {
    fn drop(&mut self) {
        unsafe { CFRelease(self.ptr.cast_const()) }
    }
}

struct PixelBufferLockGuard {
    pixel_buffer: CVPixelBufferRef,
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
                ptr::addr_of!(kvImage_ARGBToYpCbCrMatrix_ITU_R_601_4).cast(),
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
