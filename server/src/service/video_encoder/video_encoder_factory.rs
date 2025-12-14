use std::str::FromStr;

use strum::IntoEnumIterator;

use crate::{
    desk_error::DeskError,
    model::{
        image_capture::DisplayInfo,
        settings::DeskSettings,
        video_encoder::{VideoEncoder, VideoEncoderType, VideoEncoderTypeHelper},
    },
    service::video_encoder::{h264_encoder::H264Encoder, vpx_encoder::VpxEncoder},
};

impl VideoEncoderTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, DeskError> {
        if let Some(ref video_encoder) = self.video_encoder {
            let result = VideoEncoderType::from_str(video_encoder);
            if let Ok(video_encoder_type) = result {
                return Ok(video_encoder_type);
            } else {
                log::error!(
                    "Failed to parse video encode type: {}, use default setting, error: {}",
                    video_encoder,
                    result.err().unwrap()
                );
            }
        }

        Ok(VideoEncoderType::default())
    }
}

/// Create a video encoder based on the settings.
pub fn create_video_encoder(
    desk_setting: &DeskSettings,
    display_info: &DisplayInfo,
) -> Result<Box<dyn VideoEncoder>, DeskError> {
    let encoder: Box<dyn VideoEncoder> = match desk_setting.get_video_encoder_type()? {
        VideoEncoderType::H264 => Box::new(H264Encoder::new(
            desk_setting.h264_encoder.clone().unwrap_or_default(),
        )),
        VideoEncoderType::VP8 => Box::new(VpxEncoder::new(
            vpx_encode::VideoCodecId::VP8,
            desk_setting.vp8_encoder.clone().unwrap_or_default(),
            display_info,
        )),
        VideoEncoderType::VP9 => Box::new(VpxEncoder::new(
            vpx_encode::VideoCodecId::VP9,
            desk_setting.vp9_encoder.clone().unwrap_or_default(),
            display_info,
        )),
    };
    Ok(encoder)
}

pub fn list_video_encoder() -> Vec<String> {
    VideoEncoderType::iter()
        .map(|x| Into::<&'static str>::into(x).to_string())
        .collect()
}