use std::{sync::LazyLock, time::Instant};

use desk_signal_facade::model::{desk_settings::VpxEncoderSettings, image_capture::DisplayInfo};
use prometheus::{HistogramVec, register_histogram_vec};
use vpx_encode::VideoCodecId;

use crate::{
    error::CaptureError,
    model::{
        image_capture::ImageInfo,
        video_encoder::{NalInfo, VideoEncoder},
    },
    video_encoder::{encoder_utils::duration_to_seconds, yuv_utils::PersistentYuvBuffer},
};

pub static ENCODE_TO_VPX_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_vpx_histogram", "help", &["codec", "frame_type"]).unwrap()
});

pub struct VpxEncoder {
    pub codec: String,
    pub encoder: vpx_encode::Encoder,
    pub start_time: Instant,
    yuv_buffer: Option<PersistentYuvBuffer>,
}

impl VpxEncoder {
    pub fn new(
        codec: VideoCodecId,
        setting: VpxEncoderSettings,
        display_info: &DisplayInfo,
    ) -> Result<Self, CaptureError> {
        let config = vpx_encode::Config {
            width: display_info.desktop_coordinates.width() as u32,
            height: display_info.desktop_coordinates.height() as u32,
            timebase: [1, 1000],
            // vpx `rc_target_bitrate` is in kilobits per second while
            // `VpxEncoderSettings.bps` is bits per second — divide or
            // the cap is inflated 1000x and never engages.
            bitrate: setting.bps / 1000,
            quality: Some(setting.quality),
            codec,
        };
        let encoder = vpx_encode::Encoder::new(config)?;
        let codec = match codec {
            VideoCodecId::VP8 => "VP8",
            VideoCodecId::VP9 => "VP9",
        };
        Ok(VpxEncoder {
            codec: codec.to_string(),
            encoder,
            start_time: Instant::now(),
            yuv_buffer: None,
        })
    }

    fn encode_with_encoder(
        encoder: &mut vpx_encode::Encoder,
        start_time: Instant,
        codec: &str,
        yuv: &PersistentYuvBuffer,
    ) -> Result<Vec<NalInfo>, CaptureError> {
        let ms = (Instant::now() - start_time).as_millis() as i64;
        let mut encoded = vec![];
        for packet in encoder.encode(ms, yuv.as_i420_slice())? {
            let encode_timer = Instant::now();
            let frame_type_str = if packet.key { "key" } else { "non_key" };
            encoded.push(NalInfo {
                nal_bytes: bytes::Bytes::from(packet.data.to_vec()),
                is_keyframe: packet.key,
            });
            ENCODE_TO_VPX_HISTOGRAM
                .with_label_values(&[codec, frame_type_str])
                .observe(duration_to_seconds(
                    Instant::now().saturating_duration_since(encode_timer),
                ));
        }
        Ok(encoded)
    }
}

impl VideoEncoder for VpxEncoder {
    fn encode(
        &mut self,
        image_info: &dyn ImageInfo,
        enable_dirty_rect: bool,
    ) -> Result<Vec<NalInfo>, CaptureError> {
        if self.yuv_buffer.is_none() {
            self.yuv_buffer = Some(PersistentYuvBuffer::new(
                image_info.get_width(),
                image_info.get_height(),
            ));
        }
        self.yuv_buffer
            .as_mut()
            .unwrap()
            .update(image_info, enable_dirty_rect)?;
        VpxEncoder::encode_with_encoder(
            &mut self.encoder,
            self.start_time,
            &self.codec,
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn encode_cached(&mut self) -> Result<Vec<NalInfo>, CaptureError> {
        let Some(_) = self.yuv_buffer.as_ref() else {
            return Ok(vec![]);
        };
        VpxEncoder::encode_with_encoder(
            &mut self.encoder,
            self.start_time,
            &self.codec,
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn set_bitrate_cap(&mut self, cap_kbps: Option<u32>) -> bool {
        let result = match cap_kbps {
            Some(kbps) => self.encoder.set_bitrate_cap_kbps(kbps.max(1)),
            None => self.encoder.reset_bitrate_cap(),
        };
        match result {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "{} bitrate cap update ({cap_kbps:?}) failed: {e}",
                    self.codec
                );
                false
            }
        }
    }
}
