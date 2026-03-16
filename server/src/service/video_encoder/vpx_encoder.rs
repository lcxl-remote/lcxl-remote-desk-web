use std::{sync::LazyLock, time::Instant};

use desk_signal_facade::model::{desk_settings::VpxEncoderSettings, image_capture::DisplayInfo};
use prometheus::{HistogramVec, register_histogram_vec};
use vpx_encode::VideoCodecId;

use crate::{
    error::DeskError,
    model::{
        image_capture::ImageInfo,
        video_encoder::{NalInfo, VideoEncoder},
    },
    service::video_encoder::{encoder_utils::duration_to_seconds, yuv_utils::argb_to_i420},
};

pub static ENCODE_TO_VPX_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_vpx_histogram", "help", &["codec", "frame_type"]).unwrap()
});

pub struct VpxEncoder {
    pub codec: String,
    pub encoder: vpx_encode::Encoder,
    pub start_time: Instant,
}

impl VpxEncoder {
    pub fn new(
        codec: VideoCodecId,
        setting: VpxEncoderSettings,
        display_info: &DisplayInfo,
    ) -> Result<Self, DeskError> {
        let config = vpx_encode::Config {
            width: display_info.desktop_coordinates.width() as u32,
            height: display_info.desktop_coordinates.height() as u32,
            timebase: [1, 1000],
            bitrate: setting.bps,
            quality: Some(setting.quality),
            codec,
        };
        let encoder = vpx_encode::Encoder::new(config)?;
        let codec = match codec {
            VideoCodecId::VP8 => "VP8",
            VideoCodecId::VP9 => "VP9",
        };
        let vpx_encoder = VpxEncoder {
            codec: codec.to_string(),
            encoder,
            start_time: Instant::now(),
        };
        Ok(vpx_encoder)
    }
}

impl VideoEncoder for VpxEncoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<Vec<NalInfo>, DeskError> {
        let mut yuv_data = vec![];
        argb_to_i420(
            image_info.get_width() as usize,
            image_info.get_height() as usize,
            image_info.get_data(),
            &mut yuv_data,
        );
        let now = Instant::now();
        let time = now - self.start_time;

        let ms = time.as_secs() * 1000 + time.subsec_millis() as u64;

        let mut encoded = vec![];
        for packet in self.encoder.encode(ms as i64, yuv_data.as_slice())? {
            let encode_to_vpx_timer = Instant::now();
            let frame_type_str = match packet.key {
                true => "key",
                false => "non_key",
            };

            encoded.push(NalInfo {
                nal_bytes: bytes::Bytes::from(packet.data.to_vec()),
            });
            ENCODE_TO_VPX_HISTOGRAM
                .with_label_values(&[self.codec.as_str(), frame_type_str])
                .observe(duration_to_seconds(
                    Instant::now().saturating_duration_since(encode_to_vpx_timer),
                ));
        }

        Ok(encoded)
    }
}
