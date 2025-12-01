//! VideoToolbox HEVC encoder wrapper
//!
//! Converts BGRA frames to I420 and encodes with VideoToolbox.

use anyhow::{Context, Result};
use shiguredo_video_toolbox::{Encoder, EncoderConfig, EncodedFrame, ProfileLevel};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

/// Annex-B NAL start code
const NAL_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// HEVC encoder output
pub struct EncodedOutput {
    /// NAL units in Annex-B format
    pub nal_data: Vec<u8>,
    /// Whether this is a keyframe (IDR)
    pub is_keyframe: bool,
    /// VPS/SPS/PPS config NALs for keyframes (Annex-B format)
    pub config_nals: Option<Vec<u8>>,
}

/// HEVC encoder using VideoToolbox
pub struct HevcEncoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    /// Pre-allocated buffers for color conversion
    y_plane: Vec<u8>,
    u_plane: Vec<u8>,
    v_plane: Vec<u8>,
    /// Statistics
    frames_encoded: u64,
    last_log: Instant,
    /// Whether we've sent config NALs
    config_sent: bool,
}

impl HevcEncoder {
    /// Create a new HEVC encoder
    pub fn new(width: u32, height: u32, bitrate_bps: u32, fps: u32) -> Result<Self> {
        log::info!(
            "Creating HEVC encoder: {}x{} @ {}fps, {} Mbps",
            width,
            height,
            fps,
            bitrate_bps / 1_000_000
        );

        let config = EncoderConfig {
            width: width as usize,
            height: height as usize,
            target_bitrate: bitrate_bps as usize,
            fps_numerator: fps as usize,
            fps_denominator: 1,
            // Optimize for real-time streaming
            prioritize_speed_over_quality: true,
            real_time: true,
            maximize_power_efficiency: false,
            use_parallelization: true,
            // Disable B-frames for lower latency
            allow_frame_reordering: false,
            allow_open_gop: false,
            allow_temporal_compression: true,
            // Keyframe every 2 seconds
            max_key_frame_interval: None,
            max_key_frame_interval_duration: Some(Duration::from_secs(2)),
            // HEVC Main profile for wide compatibility
            profile_level: ProfileLevel::H265Main,
            h264_entropy_mode: shiguredo_video_toolbox::H264EntropyMode::Cabac,
            // Minimize frame delay for lower latency
            max_frame_delay_count: NonZeroUsize::new(1),
        };

        let encoder = Encoder::new_h265(&config).context("Failed to create HEVC encoder")?;

        // Pre-allocate conversion buffers
        let y_size = (width * height) as usize;
        let uv_size = y_size / 4;

        Ok(Self {
            encoder,
            width,
            height,
            y_plane: vec![0u8; y_size],
            u_plane: vec![0u8; uv_size],
            v_plane: vec![0u8; uv_size],
            frames_encoded: 0,
            last_log: Instant::now(),
            config_sent: false,
        })
    }

    /// Encode a BGRA frame
    /// Returns encoded NAL data if available
    pub fn encode_frame(
        &mut self,
        bgra_data: &[u8],
        force_idr: bool,
    ) -> Result<Option<EncodedOutput>> {
        // Convert BGRA to I420
        self.bgra_to_i420(bgra_data);

        // Encode
        self.encoder
            .encode(&self.y_plane, &self.u_plane, &self.v_plane)
            .context("Failed to encode frame")?;

        // Get encoded output
        let result = if let Some(frame) = self.encoder.next_frame() {
            Some(self.process_encoded_frame(frame)?)
        } else {
            None
        };

        self.frames_encoded += 1;

        // Log periodically
        if self.last_log.elapsed() > Duration::from_secs(1) {
            log::info!("Encoded {} frames", self.frames_encoded);
            self.last_log = Instant::now();
        }

        Ok(result)
    }

    /// Flush any remaining frames
    pub fn flush(&mut self) -> Result<Vec<EncodedOutput>> {
        self.encoder.finish().context("Failed to finish encoding")?;

        let mut outputs = Vec::new();
        while let Some(frame) = self.encoder.next_frame() {
            outputs.push(self.process_encoded_frame(frame)?);
        }

        Ok(outputs)
    }

    /// Check if config NALs have been sent
    pub fn config_sent(&self) -> bool {
        self.config_sent
    }

    /// Mark config as sent
    pub fn mark_config_sent(&mut self) {
        self.config_sent = true;
    }

    /// Process an encoded frame into our output format
    fn process_encoded_frame(&self, frame: EncodedFrame) -> Result<EncodedOutput> {
        // Convert AVCC data to Annex-B NAL units
        let nal_data = avcc_to_annexb(&frame.data);

        // Build config NALs for keyframes
        let config_nals = if frame.keyframe {
            let mut config = Vec::new();
            // VPS
            for vps in &frame.vps_list {
                config.extend_from_slice(&NAL_START_CODE);
                config.extend_from_slice(vps);
            }
            // SPS
            for sps in &frame.sps_list {
                config.extend_from_slice(&NAL_START_CODE);
                config.extend_from_slice(sps);
            }
            // PPS
            for pps in &frame.pps_list {
                config.extend_from_slice(&NAL_START_CODE);
                config.extend_from_slice(pps);
            }
            Some(config)
        } else {
            None
        };

        Ok(EncodedOutput {
            nal_data,
            is_keyframe: frame.keyframe,
            config_nals,
        })
    }

    /// Convert BGRA to I420 (YUV420P)
    fn bgra_to_i420(&mut self, bgra: &[u8]) {
        let w = self.width as usize;
        let h = self.height as usize;

        // Process 2x2 blocks for chroma subsampling
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                // Alpha ignored

                // BT.601 conversion
                let y_val = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                self.y_plane[y * w + x] = y_val.clamp(0, 255) as u8;

                // Chroma at half resolution (2x2 subsampling)
                if y % 2 == 0 && x % 2 == 0 {
                    let u_val = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                    let v_val = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;

                    let uv_idx = (y / 2) * (w / 2) + (x / 2);
                    self.u_plane[uv_idx] = u_val.clamp(0, 255) as u8;
                    self.v_plane[uv_idx] = v_val.clamp(0, 255) as u8;
                }
            }
        }
    }
}

/// Convert AVCC format to Annex-B format
/// AVCC: [length_prefix (4 bytes)][nal_data]...
/// Annex-B: [00 00 00 01][nal_data]...
fn avcc_to_annexb(avcc_data: &[u8]) -> Vec<u8> {
    let mut annexb = Vec::with_capacity(avcc_data.len() + 64);
    let mut offset = 0;

    while offset + 4 <= avcc_data.len() {
        // Read 4-byte length prefix (big-endian)
        let nal_len = u32::from_be_bytes([
            avcc_data[offset],
            avcc_data[offset + 1],
            avcc_data[offset + 2],
            avcc_data[offset + 3],
        ]) as usize;

        offset += 4;

        if offset + nal_len > avcc_data.len() {
            log::warn!("Invalid AVCC NAL length: {} at offset {}", nal_len, offset);
            break;
        }

        // Write Annex-B start code and NAL data
        annexb.extend_from_slice(&NAL_START_CODE);
        annexb.extend_from_slice(&avcc_data[offset..offset + nal_len]);

        offset += nal_len;
    }

    annexb
}
