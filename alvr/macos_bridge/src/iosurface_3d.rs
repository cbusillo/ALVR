use crate::bgra::{Nv12PixelBuffer, create_iosurface_nv12_pixel_buffer};
use alvr_common::{Pose, ViewParams};
use anyhow::Result;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const PIXEL_BUFFER_POOL_SIZE: usize = 6;

pub struct Iosurface3dFrameSource {
    width: u32,
    height: u32,
    anchor_pose: Option<Pose>,
    available_buffers: VecDeque<Nv12PixelBuffer>,
}

pub struct Iosurface3dFrame {
    pub pixel_buffer: Nv12PixelBuffer,
    pub recycle_buffer: Nv12PixelBuffer,
    pub fill_elapsed: Duration,
}

impl Iosurface3dFrameSource {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let mut available_buffers = VecDeque::with_capacity(PIXEL_BUFFER_POOL_SIZE);
        for _ in 0..PIXEL_BUFFER_POOL_SIZE {
            available_buffers.push_back(create_iosurface_nv12_pixel_buffer(width, height)?);
        }

        Ok(Self {
            width,
            height,
            anchor_pose: None,
            available_buffers,
        })
    }

    pub fn frame(
        &mut self,
        frame_index: u64,
        hmd_pose: Pose,
        view_params: [ViewParams; 2],
        forward_z_sign: f32,
        allow_anchor_update: bool,
    ) -> Result<Option<Iosurface3dFrame>> {
        let Some(buffer) = self.available_buffers.pop_front() else {
            return Ok(None);
        };

        if allow_anchor_update && self.anchor_pose.is_none() {
            self.anchor_pose = Some(hmd_pose);
        }
        let anchor_pose = self.anchor_pose.unwrap_or(hmd_pose);
        let fill_start = Instant::now();
        buffer.fill_world_locked_diagnostic(
            self.width,
            self.height,
            frame_index,
            hmd_pose,
            anchor_pose,
            view_params,
            forward_z_sign,
        )?;
        let pixel_buffer = buffer.clone();

        Ok(Some(Iosurface3dFrame {
            pixel_buffer,
            recycle_buffer: buffer,
            fill_elapsed: fill_start.elapsed(),
        }))
    }

    pub fn recycle(&mut self, buffer: Nv12PixelBuffer) {
        self.available_buffers.push_back(buffer);
    }

    pub fn width(&self) -> u32 {
        self.width
    }
}
