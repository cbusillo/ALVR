use crate::{SurfaceLease, native_source::NativeSourceFrame};
use anyhow::{Result, anyhow};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    ptr::NonNull,
    time::{Duration, Instant},
};

const ERROR_CAPACITY: usize = 512;
const METAL_LIBRARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bgra_to_nv12.metallib"));

unsafe extern "C" {
    fn alvr_metal_converter_create(
        library_bytes: *const u8,
        library_size: usize,
        error_buffer: *mut c_char,
        error_capacity: usize,
    ) -> *mut c_void;
    fn alvr_metal_converter_destroy(converter: *mut c_void);
    fn alvr_metal_converter_convert(
        converter: *mut c_void,
        source_surface: *mut c_void,
        destination_buffer: *mut c_void,
        source_width: u32,
        source_height: u32,
        gpu_duration_ns: *mut u64,
        error_buffer: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
}

pub struct MetalConverter {
    converter: NonNull<c_void>,
}

#[derive(Debug, Clone, Copy)]
pub struct ConversionTiming {
    pub wall: Duration,
    pub gpu: Duration,
}

impl MetalConverter {
    pub fn new() -> Result<Self> {
        let mut error = [0 as c_char; ERROR_CAPACITY];
        let converter = unsafe {
            alvr_metal_converter_create(
                METAL_LIBRARY.as_ptr(),
                METAL_LIBRARY.len(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        NonNull::new(converter)
            .map(|converter| {
                eprintln!("metal_converter resampler=bilinear eye_boundary=clamped");
                Self { converter }
            })
            .ok_or_else(|| anyhow!(error_message(&error)))
    }

    pub fn convert(
        &self,
        source_frame: &NativeSourceFrame<'_>,
        destination: &SurfaceLease,
        source_width: u32,
        source_height: u32,
    ) -> Result<ConversionTiming> {
        self.convert_raw(
            source_frame.surface()?,
            destination.cv_pixel_buffer(),
            source_width,
            source_height,
        )
    }

    fn convert_raw(
        &self,
        source_surface: NonNull<c_void>,
        destination_buffer: NonNull<c_void>,
        source_width: u32,
        source_height: u32,
    ) -> Result<ConversionTiming> {
        let mut error = [0 as c_char; ERROR_CAPACITY];
        let mut gpu_duration_ns = 0;
        let start = Instant::now();
        let status = unsafe {
            alvr_metal_converter_convert(
                self.converter.as_ptr(),
                source_surface.as_ptr(),
                destination_buffer.as_ptr(),
                source_width,
                source_height,
                &mut gpu_duration_ns,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let elapsed = start.elapsed();
        if status == 0 {
            Ok(ConversionTiming {
                wall: elapsed,
                gpu: Duration::from_nanos(gpu_duration_ns),
            })
        } else {
            Err(anyhow!(
                "Metal conversion failed ({status}): {}",
                error_message(&error)
            ))
        }
    }
}

impl Drop for MetalConverter {
    fn drop(&mut self) {
        unsafe { alvr_metal_converter_destroy(self.converter.as_ptr()) }
    }
}

fn error_message(buffer: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SurfacePool, native_source::NativeSource};
    use std::{
        ptr,
        time::{SystemTime, UNIX_EPOCH},
    };

    unsafe extern "C" {
        fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
        fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
        fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferGetBaseAddressOfPlane(buffer: *mut c_void, plane: usize) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRowOfPlane(buffer: *mut c_void, plane: usize) -> usize;
    }

    #[test]
    fn converts_packed_bgra_eyes_to_video_range_nv12() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let service = format!("com.alvr.metal-test.{}.{}", std::process::id(), nonce);
        let source = NativeSource::new(&service, nonce, 8, 6).unwrap();
        let source_surface = source.surface(0).unwrap();

        unsafe {
            assert_eq!(
                IOSurfaceLock(source_surface.as_ptr(), 0, ptr::null_mut()),
                0
            );
            let base = IOSurfaceGetBaseAddress(source_surface.as_ptr()).cast::<u8>();
            let row_bytes = IOSurfaceGetBytesPerRow(source_surface.as_ptr());
            assert!(!base.is_null());
            for y in 0..6 {
                for x in 0..8 {
                    let pixel = base.add(y * row_bytes + x * 4);
                    if x < 4 {
                        ptr::copy_nonoverlapping([0u8, 0, 255, 255].as_ptr(), pixel, 4);
                    } else {
                        ptr::copy_nonoverlapping([255u8, 0, 0, 255].as_ptr(), pixel, 4);
                    }
                }
            }
            assert_eq!(
                IOSurfaceUnlock(source_surface.as_ptr(), 0, ptr::null_mut()),
                0
            );
        }

        let pool = SurfacePool::new(4, 4, 1).unwrap();
        let lease = pool.try_acquire().unwrap().unwrap();
        let converter = MetalConverter::new().unwrap();
        converter
            .convert_raw(source_surface, lease.cv_pixel_buffer(), 8, 6)
            .unwrap();

        unsafe {
            let buffer = lease.cv_pixel_buffer().as_ptr();
            assert_eq!(CVPixelBufferLockBaseAddress(buffer, 0), 0);
            let y_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0).cast::<u8>();
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
            let uv_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 1).cast::<u8>();
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
            assert!(!y_base.is_null() && !uv_base.is_null());

            let red_y = *y_base;
            let blue_y = *y_base.add(3);
            assert!((60..=66).contains(&red_y), "unexpected red luma {red_y}");
            assert!((29..=35).contains(&blue_y), "unexpected blue luma {blue_y}");

            let red_cb = *uv_base;
            let red_cr = *uv_base.add(1);
            let blue_cb = *uv_base.add(2);
            let blue_cr = *uv_base.add(3);
            assert!((99..=105).contains(&red_cb), "unexpected red Cb {red_cb}");
            assert!((237..=243).contains(&red_cr), "unexpected red Cr {red_cr}");
            assert!(
                (237..=243).contains(&blue_cb),
                "unexpected blue Cb {blue_cb}"
            );
            assert!(
                (115..=121).contains(&blue_cr),
                "unexpected blue Cr {blue_cr}"
            );

            assert!(y_stride >= 4 && uv_stride >= 4);
            assert_eq!(CVPixelBufferUnlockBaseAddress(buffer, 0), 0);
        }
    }

    #[test]
    fn bilinear_downscale_smooths_each_eye_without_cross_eye_bleed() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let service = format!(
            "com.alvr.metal-filter-test.{}.{}",
            std::process::id(),
            nonce
        );
        let source = NativeSource::new(&service, nonce, 8, 2).unwrap();
        let source_surface = source.surface(0).unwrap();

        unsafe {
            assert_eq!(
                IOSurfaceLock(source_surface.as_ptr(), 0, ptr::null_mut()),
                0
            );
            let base = IOSurfaceGetBaseAddress(source_surface.as_ptr()).cast::<u8>();
            let row_bytes = IOSurfaceGetBytesPerRow(source_surface.as_ptr());
            assert!(!base.is_null());
            for y in 0..2 {
                for x in 0..8 {
                    let pixel = base.add(y * row_bytes + x * 4);
                    let bgra = if x < 4 {
                        if x % 2 == 0 {
                            [0u8, 0, 0, 255]
                        } else {
                            [255u8, 255, 255, 255]
                        }
                    } else {
                        [255u8, 0, 0, 255]
                    };
                    ptr::copy_nonoverlapping(bgra.as_ptr(), pixel, 4);
                }
            }
            assert_eq!(
                IOSurfaceUnlock(source_surface.as_ptr(), 0, ptr::null_mut()),
                0
            );
        }

        let pool = SurfacePool::new(4, 2, 1).unwrap();
        let lease = pool.try_acquire().unwrap().unwrap();
        let converter = MetalConverter::new().unwrap();
        converter
            .convert_raw(source_surface, lease.cv_pixel_buffer(), 8, 2)
            .unwrap();

        unsafe {
            let buffer = lease.cv_pixel_buffer().as_ptr();
            assert_eq!(CVPixelBufferLockBaseAddress(buffer, 0), 0);
            let y_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0).cast::<u8>();
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
            let uv_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 1).cast::<u8>();
            assert!(!y_base.is_null() && !uv_base.is_null());

            for row in 0..2 {
                let y_row = y_base.add(row * y_stride);
                assert!((120..=132).contains(&*y_row), "unexpected filtered luma");
                assert!(
                    (120..=132).contains(&*y_row.add(1)),
                    "unexpected left-eye boundary luma"
                );
                assert!(
                    (29..=35).contains(&*y_row.add(2)),
                    "right eye was contaminated at the stereo boundary"
                );
                assert!((29..=35).contains(&*y_row.add(3)), "unexpected blue luma");
            }

            assert!((124..=132).contains(&*uv_base), "unexpected neutral Cb");
            assert!(
                (124..=132).contains(&*uv_base.add(1)),
                "unexpected neutral Cr"
            );
            assert!((237..=243).contains(&*uv_base.add(2)), "unexpected blue Cb");
            assert!((115..=121).contains(&*uv_base.add(3)), "unexpected blue Cr");
            assert_eq!(CVPixelBufferUnlockBaseAddress(buffer, 0), 0);
        }
    }
}
