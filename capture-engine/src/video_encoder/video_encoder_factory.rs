use std::str::FromStr;

use desk_signal_facade::model::{desk_settings::DeskSettings, image_capture::DisplayInfo};
use strum::IntoEnumIterator;

#[cfg(av1_supported)]
use crate::video_encoder::av1_encoder::Av1Encoder;
use crate::{
    error::CaptureError,
    model::video_encoder::{VideoEncoder, VideoEncoderType, VideoEncoderTypeHelper},
    video_encoder::{
        h264_encoder::H264Encoder, vpx_encoder::VpxEncoder, x264_encoder::X264Encoder,
    },
};

impl VideoEncoderTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, CaptureError> {
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
) -> Result<Box<dyn VideoEncoder>, CaptureError> {
    let encoder: Box<dyn VideoEncoder> = match desk_setting.get_video_encoder_type()? {
        VideoEncoderType::X264 => Box::new(X264Encoder::new(
            desk_setting.get_x264_encoder_settings(),
            display_info,
            desk_setting.video_fps,
        )?),
        VideoEncoderType::H264 => Box::new(H264Encoder::new(
            desk_setting.get_h264_encoder_settings(display_info),
        )),
        VideoEncoderType::VP8 => Box::new(VpxEncoder::new(
            vpx_encode::VideoCodecId::VP8,
            desk_setting.get_vp8_encoder_settings(),
            display_info,
        )?),
        VideoEncoderType::VP9 => Box::new(VpxEncoder::new(
            vpx_encode::VideoCodecId::VP9,
            desk_setting.get_vp9_encoder_settings(),
            display_info,
        )?),
        VideoEncoderType::AV1 => {
            #[cfg(av1_supported)]
            {
                Box::new(Av1Encoder::new(
                    desk_setting.get_av1_encoder_settings(display_info),
                    display_info,
                    desk_setting.video_fps,
                )?)
            }
            // SVT-AV1 has no prebuilt binary for this target, so the encoder is
            // compiled out. The variant still exists for codec negotiation, but
            // it cannot be instantiated here.
            #[cfg(not(av1_supported))]
            {
                return Err(CaptureError::AnyhowError(anyhow::anyhow!(
                    "AV1 encoding is not supported on this platform"
                )));
            }
        }
    };
    Ok(encoder)
}

pub fn list_video_encoder() -> Vec<String> {
    VideoEncoderType::iter()
        // AV1 is omitted where SVT-AV1 has no prebuilt binary (the encoder is
        // compiled out), so the browser is never offered an unusable codec.
        .filter(|x| cfg!(av1_supported) || !matches!(x, VideoEncoderType::AV1))
        .map(|x| Into::<&'static str>::into(x).to_string())
        .collect()
}
