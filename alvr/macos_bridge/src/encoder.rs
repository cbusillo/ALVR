use crate::{FrameMetadata, SurfaceLease, SurfaceLeaseId, contract::FrameOrderValidator};
use anyhow::{Context, Result, anyhow, ensure};
use shiguredo_video_toolbox::{
    CodecConfig, EncodeOptions, EncodedFrame as VideoToolboxFrame, Encoder, EncoderConfig,
    Error as VideoToolboxError, FnEncodeHandler, HevcEncoderConfig, HevcProfile, PixelFormat,
    VideoCodecType, supported_codecs,
};
use std::{
    num::NonZeroU32,
    sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    time::Duration,
};

const NAL_START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareEncoderSupport {
    pub hevc_supported: bool,
    pub hardware_accelerated: bool,
    pub supports_frame_reordering: bool,
}

pub fn hevc_hardware_support() -> Result<HardwareEncoderSupport> {
    let info = supported_codecs()
        .into_iter()
        .find(|info| info.codec == VideoCodecType::Hevc)
        .context("VideoToolbox did not report HEVC encoder information")?;
    let support = HardwareEncoderSupport {
        hevc_supported: info.encoding.supported,
        hardware_accelerated: info.encoding.hardware_accelerated,
        supports_frame_reordering: info.encoding.supports_frame_reordering,
    };
    ensure!(
        support.hevc_supported,
        "VideoToolbox HEVC encode is unavailable"
    );
    ensure!(
        support.hardware_accelerated,
        "VideoToolbox did not report a hardware-accelerated HEVC encoder"
    );
    Ok(support)
}

#[derive(Debug, Clone, Copy)]
pub struct NativeHevcEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
}

pub struct EncodedFrame {
    pub lease_id: SurfaceLeaseId,
    pub metadata: FrameMetadata,
    pub nal_data: Vec<u8>,
    pub is_keyframe: bool,
    pub decoder_config_nals: Option<Vec<u8>>,
}

struct PendingFrame {
    lease_id: SurfaceLeaseId,
    metadata: FrameMetadata,
    lease: SurfaceLease,
}

type VideoToolboxResult = std::result::Result<VideoToolboxFrame<PendingFrame>, VideoToolboxError>;
type VideoToolboxEncoder = Encoder<FnEncodeHandler<PendingFrame>>;

pub struct NativeHevcEncoder {
    encoder: VideoToolboxEncoder,
    output_rx: Receiver<VideoToolboxResult>,
    width: u32,
    height: u32,
    order: FrameOrderValidator,
    pending_count: usize,
}

impl NativeHevcEncoder {
    pub fn new(config: NativeHevcEncoderConfig) -> Result<(Self, HardwareEncoderSupport)> {
        ensure!(
            config.width > 0 && config.width.is_multiple_of(2),
            "HEVC width must be even"
        );
        ensure!(
            config.height > 0 && config.height.is_multiple_of(2),
            "HEVC height must be even"
        );
        ensure!(config.fps > 0, "HEVC FPS must be greater than zero");
        ensure!(
            config.bitrate_bps > 0,
            "HEVC bitrate must be greater than zero"
        );

        let support = hevc_hardware_support()?;
        let keyframe_interval = config
            .fps
            .checked_mul(2)
            .and_then(NonZeroU32::new)
            .context("HEVC keyframe interval overflow")?;
        let (output_tx, output_rx) = mpsc::channel();
        let handler = FnEncodeHandler::new(move |result: VideoToolboxResult| {
            let _ = output_tx.send(result);
        });
        let encoder = Encoder::new(
            EncoderConfig {
                width: config.width,
                height: config.height,
                codec: CodecConfig::Hevc(HevcEncoderConfig {
                    profile: HevcProfile::Main,
                    allow_open_gop: false,
                }),
                pixel_format: PixelFormat::Nv12,
                average_bitrate: Some(config.bitrate_bps),
                fps_numerator: config.fps,
                fps_denominator: 1,
                prioritize_encoding_speed_over_quality: true,
                real_time: true,
                maximize_power_efficiency: false,
                allow_frame_reordering: false,
                allow_temporal_compression: true,
                max_key_frame_interval: Some(keyframe_interval),
                max_key_frame_interval_duration: Some(Duration::from_secs(2)),
                max_frame_delay_count: NonZeroU32::new(1),
            },
            handler,
        )
        .context("failed to create VideoToolbox HEVC encoder")?;

        Ok((
            Self {
                encoder,
                output_rx,
                width: config.width,
                height: config.height,
                order: FrameOrderValidator::default(),
                pending_count: 0,
            },
            support,
        ))
    }

    pub fn submit(
        &mut self,
        lease: SurfaceLease,
        metadata: FrameMetadata,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>> {
        ensure!(
            lease.width() == self.width && lease.height() == self.height,
            "surface dimensions {}x{} do not match encoder dimensions {}x{}",
            lease.width(),
            lease.height(),
            self.width,
            self.height
        );
        self.order.validate(&metadata)?;
        let pixel_buffer = lease.cv_pixel_buffer().as_ptr();
        let pending = PendingFrame {
            lease_id: lease.id(),
            metadata,
            lease,
        };

        unsafe {
            self.encoder.encode_pixel_buffer(
                pixel_buffer,
                &EncodeOptions {
                    force_key_frame: force_keyframe,
                },
                pending,
            )
        }
        .context("failed to submit IOSurface-backed CVPixelBuffer to VideoToolbox")?;

        self.order.record_validated(metadata);
        self.pending_count += 1;
        self.drain_ready()
    }

    pub fn drain_ready(&mut self) -> Result<Vec<EncodedFrame>> {
        let mut outputs = Vec::new();
        let mut first_error = None;
        loop {
            let result = match self.output_rx.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow!("VideoToolbox callback channel disconnected"));
                }
            };
            self.pending_count = self
                .pending_count
                .checked_sub(1)
                .context("VideoToolbox emitted a callback without a pending frame")?;
            match result {
                Ok(frame) => match complete_frame(frame) {
                    Ok(frame) => outputs.push(frame),
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                },
                Err(error) if first_error.is_none() => {
                    first_error = Some(
                        anyhow!(error).context("VideoToolbox failed to encode a submitted frame"),
                    );
                }
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(outputs)
        }
    }

    pub fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        self.encoder
            .finish()
            .context("failed to flush VideoToolbox")?;
        let outputs = self.drain_ready()?;
        ensure!(
            self.pending_count == 0,
            "VideoToolbox flush left {} frame leases pending",
            self.pending_count
        );
        Ok(outputs)
    }

    pub fn pending_count(&self) -> usize {
        self.pending_count
    }
}

impl Drop for NativeHevcEncoder {
    fn drop(&mut self) {
        if self.pending_count == 0 {
            return;
        }
        let _ = self.encoder.finish();
        while self.pending_count != 0 {
            match self.output_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(result) => {
                    self.pending_count -= 1;
                    drop(result);
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
    }
}

fn complete_frame(frame: VideoToolboxFrame<PendingFrame>) -> Result<EncodedFrame> {
    let PendingFrame {
        lease_id,
        metadata,
        lease,
    } = frame.user_data;
    let nal_data = avcc_to_annexb(&frame.data)?;
    let decoder_config_nals = if frame.keyframe {
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
    drop(lease);

    Ok(EncodedFrame {
        lease_id,
        metadata,
        nal_data,
        is_keyframe: frame.keyframe,
        decoder_config_nals,
    })
}

fn avcc_to_annexb(data: &[u8]) -> Result<Vec<u8>> {
    let mut annexb = Vec::with_capacity(data.len() + 64);
    let mut offset = 0;

    while offset < data.len() {
        ensure!(
            data.len() - offset >= 4,
            "AVCC frame has {} trailing bytes without a NAL length",
            data.len() - offset
        );
        let nal_length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        ensure!(nal_length > 0, "AVCC frame contains an empty NAL unit");
        ensure!(
            nal_length <= data.len() - offset,
            "AVCC NAL length {nal_length} exceeds the remaining {} bytes",
            data.len() - offset
        );
        annexb.extend_from_slice(&NAL_START_CODE);
        annexb.extend_from_slice(&data[offset..offset + nal_length]);
        offset += nal_length;
    }

    Ok(annexb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_multiple_avcc_nals_to_annex_b() {
        let avcc = [0, 0, 0, 2, 0xaa, 0xbb, 0, 0, 0, 1, 0xcc];
        assert_eq!(
            avcc_to_annexb(&avcc).unwrap(),
            [0, 0, 0, 1, 0xaa, 0xbb, 0, 0, 0, 1, 0xcc]
        );
    }

    #[test]
    fn rejects_truncated_avcc_nals() {
        assert!(avcc_to_annexb(&[0, 0, 0, 4, 1, 2]).is_err());
    }
}
