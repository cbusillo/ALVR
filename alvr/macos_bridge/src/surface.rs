use crate::SurfaceLeaseId;
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::VecDeque,
    ffi::c_void,
    ptr::{self, NonNull},
    sync::{Arc, Mutex, MutexGuard},
};

type CVPixelBufferRef = *mut c_void;
type CVReturn = i32;
type IOSurfaceRef = *mut c_void;

const K_CV_PIXEL_FORMAT_TYPE_420V: u32 = u32::from_be_bytes(*b"420v");
const K_CV_RETURN_SUCCESS: CVReturn = 0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFBooleanTrue: *const c_void;

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
    fn CVPixelBufferGetIOSurface(pixel_buffer: CVPixelBufferRef) -> IOSurfaceRef;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: CVPixelBufferRef) -> u32;
    fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetPlaneCount(pixel_buffer: CVPixelBufferRef) -> usize;
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

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetID(surface: IOSurfaceRef) -> u32;
}

struct CfObject {
    ptr: *const c_void,
}

impl CfObject {
    fn dictionary(keys: &[*const c_void], values: &[*const c_void]) -> Result<Self> {
        ensure!(
            keys.len() == values.len(),
            "CFDictionaryCreate key/value length mismatch"
        );
        let keys_ptr = if keys.is_empty() {
            ptr::null()
        } else {
            keys.as_ptr()
        };
        let values_ptr = if values.is_empty() {
            ptr::null()
        } else {
            values.as_ptr()
        };
        let ptr = unsafe {
            CFDictionaryCreate(
                ptr::null(),
                keys_ptr,
                values_ptr,
                keys.len() as isize,
                ptr::addr_of!(kCFTypeDictionaryKeyCallBacks),
                ptr::addr_of!(kCFTypeDictionaryValueCallBacks),
            )
        };
        ensure!(!ptr.is_null(), "CFDictionaryCreate returned null");

        Ok(Self { ptr })
    }
}

impl Drop for CfObject {
    fn drop(&mut self) {
        unsafe { CFRelease(self.ptr) }
    }
}

struct PixelBufferLock {
    pixel_buffer: CVPixelBufferRef,
}

impl Drop for PixelBufferLock {
    fn drop(&mut self) {
        unsafe {
            let _ = CVPixelBufferUnlockBaseAddress(self.pixel_buffer, 0);
        }
    }
}

struct PixelPlane {
    data: *mut u8,
    width: usize,
    height: usize,
    row_bytes: usize,
}

struct NativeSurface {
    pixel_buffer: CVPixelBufferRef,
    iosurface: IOSurfaceRef,
    surface_id: u32,
    width: u32,
    height: u32,
}

// SAFETY: The wrapper is uniquely owned and moves only between mutex-protected pool state and one lease.
unsafe impl Send for NativeSurface {}

impl NativeSurface {
    fn new(width: u32, height: u32) -> Result<Self> {
        ensure!(
            width > 0 && width.is_multiple_of(2),
            "NV12 width must be even"
        );
        ensure!(
            height > 0 && height.is_multiple_of(2),
            "NV12 height must be even"
        );

        let iosurface_properties = CfObject::dictionary(&[], &[])?;
        let keys = unsafe {
            [
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ]
        };
        let values = unsafe { [iosurface_properties.ptr, kCFBooleanTrue] };
        let attributes = CfObject::dictionary(&keys, &values)?;

        let mut pixel_buffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                ptr::null(),
                width as usize,
                height as usize,
                K_CV_PIXEL_FORMAT_TYPE_420V,
                attributes.ptr,
                &mut pixel_buffer,
            )
        };
        cv_check(status, "CVPixelBufferCreate")?;
        ensure!(!pixel_buffer.is_null(), "CVPixelBufferCreate returned null");

        let iosurface = unsafe { CVPixelBufferGetIOSurface(pixel_buffer) };
        if iosurface.is_null() {
            unsafe { CFRelease(pixel_buffer.cast_const()) };
            return Err(anyhow!(
                "IOSurface-backed CVPixelBuffer did not expose an IOSurface"
            ));
        }

        let surface = Self {
            pixel_buffer,
            iosurface,
            surface_id: unsafe { IOSurfaceGetID(iosurface) },
            width,
            height,
        };
        surface.validate_layout()?;
        surface.initialize_neutral()?;

        Ok(surface)
    }

    fn validate_layout(&self) -> Result<()> {
        ensure!(
            unsafe { CVPixelBufferGetPixelFormatType(self.pixel_buffer) }
                == K_CV_PIXEL_FORMAT_TYPE_420V,
            "CVPixelBuffer is not video-range NV12"
        );
        ensure!(
            unsafe { CVPixelBufferGetWidth(self.pixel_buffer) } == self.width as usize
                && unsafe { CVPixelBufferGetHeight(self.pixel_buffer) } == self.height as usize,
            "CVPixelBuffer dimensions do not match the requested surface"
        );
        ensure!(
            unsafe { CVPixelBufferGetPlaneCount(self.pixel_buffer) } == 2,
            "NV12 CVPixelBuffer must expose two planes"
        );
        Ok(())
    }

    fn initialize_neutral(&self) -> Result<()> {
        let _lock = lock_pixel_buffer(self.pixel_buffer)?;
        let luma = pixel_plane(self.pixel_buffer, 0)?;
        let chroma = pixel_plane(self.pixel_buffer, 1)?;
        validate_plane(
            &luma,
            self.width as usize,
            self.height as usize,
            self.width as usize,
        )?;
        validate_plane(
            &chroma,
            self.width as usize / 2,
            self.height as usize / 2,
            self.width as usize,
        )?;

        for row in 0..luma.height {
            unsafe {
                ptr::write_bytes(luma.data.add(row * luma.row_bytes), 32, self.width as usize)
            };
        }
        for row in 0..chroma.height {
            unsafe {
                ptr::write_bytes(
                    chroma.data.add(row * chroma.row_bytes),
                    128,
                    self.width as usize,
                )
            };
        }

        Ok(())
    }

    fn write_probe_marker(&mut self, frame_id: u64) -> Result<()> {
        let _lock = lock_pixel_buffer(self.pixel_buffer)?;
        let luma = pixel_plane(self.pixel_buffer, 0)?;
        validate_plane(
            &luma,
            self.width as usize,
            self.height as usize,
            self.width as usize,
        )?;

        let marker_rows = 16.min(luma.height);
        let marker_width = 256.min(self.width as usize);
        let value = 48 + (frame_id % 160) as u8;
        for row in 0..marker_rows {
            unsafe { ptr::write_bytes(luma.data.add(row * luma.row_bytes), value, marker_width) };
        }
        unsafe {
            ptr::copy_nonoverlapping(
                frame_id.to_le_bytes().as_ptr(),
                luma.data,
                size_of::<u64>().min(marker_width),
            );
        }

        Ok(())
    }
}

impl Drop for NativeSurface {
    fn drop(&mut self) {
        unsafe { CFRelease(self.pixel_buffer.cast_const()) }
    }
}

struct PoolState {
    available: VecDeque<NativeSurface>,
    capacity: usize,
    next_generation: u64,
    acquired: u64,
    recycled: u64,
}

#[derive(Clone)]
pub struct SurfacePool {
    state: Arc<Mutex<PoolState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    pub capacity: usize,
    pub available: usize,
    pub acquired: u64,
    pub recycled: u64,
}

pub struct SurfaceLease {
    id: SurfaceLeaseId,
    surface: Option<NativeSurface>,
    pool: Arc<Mutex<PoolState>>,
}

impl SurfacePool {
    pub fn new(width: u32, height: u32, capacity: usize) -> Result<Self> {
        ensure!(
            capacity > 0,
            "surface pool capacity must be greater than zero"
        );
        let mut available = VecDeque::with_capacity(capacity);
        for _ in 0..capacity {
            available.push_back(NativeSurface::new(width, height)?);
        }

        Ok(Self {
            state: Arc::new(Mutex::new(PoolState {
                available,
                capacity,
                next_generation: 0,
                acquired: 0,
                recycled: 0,
            })),
        })
    }

    pub fn try_acquire(&self) -> Result<Option<SurfaceLease>> {
        let mut state = lock_state(&self.state);
        let Some(surface) = state.available.pop_front() else {
            return Ok(None);
        };
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .context("surface lease generation overflow")?;
        state.acquired += 1;
        let id = SurfaceLeaseId {
            surface_id: surface.surface_id,
            generation: state.next_generation,
        };

        Ok(Some(SurfaceLease {
            id,
            surface: Some(surface),
            pool: Arc::clone(&self.state),
        }))
    }

    pub fn stats(&self) -> PoolStats {
        let state = lock_state(&self.state);
        PoolStats {
            capacity: state.capacity,
            available: state.available.len(),
            acquired: state.acquired,
            recycled: state.recycled,
        }
    }
}

impl SurfaceLease {
    pub fn id(&self) -> SurfaceLeaseId {
        self.id
    }

    pub fn width(&self) -> u32 {
        self.surface().width
    }

    pub fn height(&self) -> u32 {
        self.surface().height
    }

    pub fn cv_pixel_buffer(&self) -> NonNull<c_void> {
        NonNull::new(self.surface().pixel_buffer).expect("CVPixelBuffer pointer must be non-null")
    }

    pub fn iosurface(&self) -> NonNull<c_void> {
        NonNull::new(self.surface().iosurface).expect("IOSurface pointer must be non-null")
    }

    pub fn write_probe_marker(&mut self, frame_id: u64) -> Result<()> {
        self.surface
            .as_mut()
            .expect("surface lease must own a surface")
            .write_probe_marker(frame_id)
    }

    fn surface(&self) -> &NativeSurface {
        self.surface
            .as_ref()
            .expect("surface lease must own a surface")
    }
}

impl Drop for SurfaceLease {
    fn drop(&mut self) {
        if let Some(surface) = self.surface.take() {
            let mut state = lock_state(&self.pool);
            state.available.push_back(surface);
            state.recycled += 1;
            debug_assert!(state.available.len() <= state.capacity);
        }
    }
}

fn lock_state(state: &Arc<Mutex<PoolState>>) -> MutexGuard<'_, PoolState> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

fn lock_pixel_buffer(pixel_buffer: CVPixelBufferRef) -> Result<PixelBufferLock> {
    cv_check(
        unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) },
        "CVPixelBufferLockBaseAddress",
    )?;
    Ok(PixelBufferLock { pixel_buffer })
}

fn pixel_plane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> Result<PixelPlane> {
    let data =
        unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, plane_index) }.cast::<u8>();
    ensure!(
        !data.is_null(),
        "CVPixelBuffer plane {plane_index} has a null base address"
    );

    Ok(PixelPlane {
        data,
        width: unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, plane_index) },
        height: unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, plane_index) },
        row_bytes: unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, plane_index) },
    })
}

fn validate_plane(
    plane: &PixelPlane,
    width: usize,
    height: usize,
    minimum_row_bytes: usize,
) -> Result<()> {
    ensure!(
        plane.width == width && plane.height == height && plane.row_bytes >= minimum_row_bytes,
        "unexpected CVPixelBuffer plane layout: {}x{} row_bytes={} expected {}x{} row_bytes>={minimum_row_bytes}",
        plane.width,
        plane.height,
        plane.row_bytes,
        width,
        height
    );
    Ok(())
}

fn cv_check(status: CVReturn, operation: &str) -> Result<()> {
    ensure!(
        status == K_CV_RETURN_SUCCESS,
        "{operation} failed with CVReturn {status}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_pool_recycles_the_same_surface_with_a_new_generation() -> Result<()> {
        let pool = SurfacePool::new(64, 64, 2)?;
        let first = pool.try_acquire()?.unwrap();
        let second = pool.try_acquire()?.unwrap();
        let first_id = first.id();

        assert!(pool.try_acquire()?.is_none());
        assert_ne!(first.id().surface_id, second.id().surface_id);
        drop(first);

        let recycled = pool.try_acquire()?.unwrap();
        assert_eq!(recycled.id().surface_id, first_id.surface_id);
        assert!(recycled.id().generation > first_id.generation);
        drop(second);
        drop(recycled);

        let stats = pool.stats();
        assert_eq!(stats.available, stats.capacity);
        assert_eq!(stats.acquired, stats.recycled);
        Ok(())
    }
}
