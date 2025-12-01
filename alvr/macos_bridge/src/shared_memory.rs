//! Shared memory interface for receiving frames from Wine
//!
//! This module creates and manages a memory-mapped file that Wine's
//! VideoEncoderSharedMem writes frames to.

use anyhow::{Context, Result};
use memmap2::MmapMut;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

/// Shared memory file path - must match C++ ALVR_SHM_PATH
pub const SHM_PATH: &str = "/tmp/alvr_frame_buffer.shm";
pub const SHM_MAGIC: u32 = 0x414C5652; // "ALVR"
pub const SHM_VERSION: u32 = 1;

/// Maximum frame dimensions
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 2048;
pub const BYTES_PER_PIXEL: u32 = 4; // BGRA
pub const MAX_FRAME_SIZE: usize = (MAX_WIDTH * MAX_HEIGHT * BYTES_PER_PIXEL) as usize;
pub const NUM_BUFFERS: usize = 3;

/// Frame states for lock-free synchronization
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    Empty = 0,
    Writing = 1,
    Ready = 2,
    Encoding = 3,
}

/// Per-frame metadata in shared memory - must match C++ AlvrFrameHeader
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
}

/// Copyable frame header for returning to callers
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp_ns: u64,
    pub frame_number: u64,
    pub is_idr: u8,
    pub pose: [[f32; 4]; 3],
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
        }
    }
}

/// Shared memory header - must match C++ AlvrSharedMemory
#[repr(C)]
pub struct SharedMemoryHeader {
    pub magic: u32,
    pub version: u32,
    pub initialized: u32,
    pub shutdown: u32,
    pub config_width: u32,
    pub config_height: u32,
    pub config_format: u32,
    pub config_set: u32,
    pub write_sequence: u64,
    pub read_sequence: u64,
    pub frames_written: u64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub reserved: [u8; 64],
    pub frame_headers: [FrameHeaderRaw; NUM_BUFFERS],
}

/// Calculate offset to frame pixel data (aligned to 4K page)
fn frame_offset(buffer_index: usize) -> usize {
    let header_size = std::mem::size_of::<SharedMemoryHeader>();
    let aligned_header = (header_size + 4095) & !4095;
    aligned_header + buffer_index * MAX_FRAME_SIZE
}

/// Total shared memory size
fn total_size() -> usize {
    frame_offset(NUM_BUFFERS)
}

/// Shared memory manager
pub struct SharedMemory {
    _file: File,
    mmap: MmapMut,
}

impl SharedMemory {
    /// Create and initialize shared memory
    pub fn create() -> Result<Self> {
        let path = Path::new(SHM_PATH);
        let size = total_size();

        log::info!("Creating shared memory at {} ({} bytes)", SHM_PATH, size);

        // Create or truncate file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .context("Failed to create shared memory file")?;

        // Set file size
        file.set_len(size as u64)
            .context("Failed to set shared memory size")?;

        // Memory map
        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // Initialize header
        let header = unsafe { &mut *(mmap.as_mut_ptr() as *mut SharedMemoryHeader) };
        header.magic = SHM_MAGIC;
        header.version = SHM_VERSION;
        header.initialized = 0;
        header.shutdown = 0;
        header.config_width = 0;
        header.config_height = 0;
        header.config_format = 0;
        header.config_set = 0;
        header.write_sequence = 0;
        header.read_sequence = 0;
        header.frames_written = 0;
        header.frames_encoded = 0;
        header.frames_dropped = 0;

        // Initialize frame headers
        for i in 0..NUM_BUFFERS {
            header.frame_headers[i].state = AtomicU32::new(FrameState::Empty as u32);
        }

        // Sync to disk
        mmap.flush()?;

        // Mark as initialized
        header.initialized = 1;
        mmap.flush()?;

        log::info!("Shared memory initialized, waiting for Wine connection...");

        Ok(Self { _file: file, mmap })
    }

    /// Get header reference
    pub fn header(&self) -> &SharedMemoryHeader {
        unsafe { &*(self.mmap.as_ptr() as *const SharedMemoryHeader) }
    }

    /// Get mutable header reference
    pub fn header_mut(&mut self) -> &mut SharedMemoryHeader {
        unsafe { &mut *(self.mmap.as_mut_ptr() as *mut SharedMemoryHeader) }
    }

    /// Check if Wine has connected and set configuration
    pub fn is_configured(&self) -> bool {
        self.header().config_set != 0
    }

    /// Get configuration (width, height, format)
    pub fn get_config(&self) -> Option<(u32, u32, u32)> {
        let h = self.header();
        if h.config_set != 0 {
            Some((h.config_width, h.config_height, h.config_format))
        } else {
            None
        }
    }

    /// Try to acquire a frame for encoding
    /// Returns (buffer_index, frame_header, pixel_data) if a frame is ready
    pub fn try_acquire_frame(&mut self) -> Option<(usize, FrameHeader, &[u8])> {
        let header = self.header();

        // Find a buffer in READY state
        for i in 0..NUM_BUFFERS {
            let frame_header = &header.frame_headers[i];

            // Atomic compare-exchange: READY -> ENCODING
            let expected = FrameState::Ready as u32;
            let new = FrameState::Encoding as u32;
            if frame_header
                .state
                .compare_exchange(expected, new, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // Successfully acquired this buffer - copy fields to return struct
                let frame_copy = FrameHeader::from_raw(frame_header);

                // Get pixel data
                let offset = frame_offset(i);
                let frame_size = (frame_copy.height * frame_copy.stride) as usize;
                let pixel_data = &self.mmap[offset..offset + frame_size];

                return Some((i, frame_copy, pixel_data));
            }
        }

        None
    }

    /// Release a frame after encoding
    pub fn release_frame(&mut self, buffer_index: usize) {
        let header = self.header();
        header.frame_headers[buffer_index]
            .state
            .store(FrameState::Empty as u32, Ordering::Release);

        // Update statistics (non-atomic OK for statistics)
        let header_mut = self.header_mut();
        header_mut.frames_encoded = header_mut.frames_encoded.wrapping_add(1);
        header_mut.read_sequence = header_mut.read_sequence.wrapping_add(1);
    }

    /// Signal shutdown
    pub fn shutdown(&mut self) {
        self.header_mut().shutdown = 1;
        let _ = self.mmap.flush();
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        self.shutdown();
        log::info!("Shared memory cleaned up");
    }
}
