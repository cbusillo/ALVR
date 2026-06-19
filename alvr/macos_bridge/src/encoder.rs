use crate::bgra::Nv12PixelBuffer;
use anyhow::{Context, Result};
use shiguredo_video_toolbox::{
    CodecConfig, EncodeOptions, EncodedFrame, Encoder, EncoderConfig, HevcEncoderConfig,
    HevcProfile, PixelFormat,
};
use std::{num::NonZeroU32, time::Duration};

const NAL_START_CODE: [u8; 4] = [0, 0, 0, 1];

pub struct EncodedOutput {
    pub nal_data: Vec<u8>,
    pub is_keyframe: bool,
    pub config_nals: Option<Vec<u8>>,
}

pub struct HevcEncoder {
    encoder: Encoder,
    config_sent: bool,
}

impl HevcEncoder {
    pub fn new(width: u32, height: u32, bitrate_bps: u64, fps: u32) -> Result<Self> {
        let config = EncoderConfig {
            width,
            height,
            codec: CodecConfig::Hevc(HevcEncoderConfig {
                profile: HevcProfile::Main,
                allow_open_gop: false,
            }),
            pixel_format: PixelFormat::Nv12,
            average_bitrate: Some(bitrate_bps),
            fps_numerator: fps,
            fps_denominator: 1,
            prioritize_encoding_speed_over_quality: true,
            real_time: true,
            maximize_power_efficiency: false,
            allow_frame_reordering: false,
            allow_temporal_compression: true,
            max_key_frame_interval: Some(NonZeroU32::new(fps * 2).unwrap()),
            max_key_frame_interval_duration: Some(Duration::from_secs(2)),
            max_frame_delay_count: Some(NonZeroU32::new(1).unwrap()),
        };

        Ok(Self {
            encoder: Encoder::new(config).context("failed to create VideoToolbox HEVC encoder")?,
            config_sent: false,
        })
    }

    pub fn encode_pixel_buffer(
        &mut self,
        pixel_buffer: &Nv12PixelBuffer,
        force_keyframe: bool,
    ) -> Result<Option<EncodedOutput>> {
        unsafe {
            self.encoder
                .encode_pixel_buffer(
                    pixel_buffer.as_ptr(),
                    &EncodeOptions {
                        force_key_frame: force_keyframe,
                    },
                )
                .context("failed to submit pixel buffer to VideoToolbox")?;
        }

        self.drain_one_frame()
    }

    pub fn finish(&mut self) -> Result<Vec<EncodedOutput>> {
        self.encoder
            .finish()
            .context("failed to flush VideoToolbox")?;

        let mut outputs = Vec::new();
        while let Some(output) = self.drain_one_frame()? {
            outputs.push(output);
        }

        Ok(outputs)
    }

    pub fn config_sent(&self) -> bool {
        self.config_sent
    }

    pub fn mark_config_sent(&mut self) {
        self.config_sent = true;
    }

    fn drain_one_frame(&mut self) -> Result<Option<EncodedOutput>> {
        self.encoder
            .next_frame()
            .context("failed to read VideoToolbox output")?
            .map(process_encoded_frame)
            .transpose()
    }
}

fn process_encoded_frame(frame: EncodedFrame) -> Result<EncodedOutput> {
    let nal_data = avcc_to_annexb(&frame.data);
    let config_nals = if frame.keyframe {
        let mut config = Vec::new();
        for nal in frame
            .vps_list
            .iter()
            .chain(frame.sps_list.iter())
            .chain(frame.pps_list.iter())
        {
            config.extend_from_slice(&NAL_START_CODE);
            config.extend_from_slice(nal);
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

fn avcc_to_annexb(avcc_data: &[u8]) -> Vec<u8> {
    let mut annexb = Vec::with_capacity(avcc_data.len() + 64);
    let mut offset = 0;

    while offset + 4 <= avcc_data.len() {
        let nal_len = u32::from_be_bytes([
            avcc_data[offset],
            avcc_data[offset + 1],
            avcc_data[offset + 2],
            avcc_data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + nal_len > avcc_data.len() {
            log::warn!("invalid AVCC NAL length {nal_len} at offset {offset}");
            break;
        }

        annexb.extend_from_slice(&NAL_START_CODE);
        annexb.extend_from_slice(&avcc_data[offset..offset + nal_len]);
        offset += nal_len;
    }

    annexb
}
