use crate::{
    desk_error::DeskError,
    model::{
        common::ErrorCode,
        encoder::{VideoEncoder, VideoEncoderType, VideoEncoderTypeHelper},
        settings::DeskSettings,
    },
    service::encoder::h264_encoder::H264Encoder,
};

impl VideoEncoderTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, DeskError> {
        let video_encoder = {
            if let Some(ref video_encoder) = self.video_encoder {
                video_encoder.clone()
            } else {
                "h264".to_string()
            }
        };
        match video_encoder.as_str() {
            "h264" => Ok(VideoEncoderType::H264(
                self.h264_encoder.clone().unwrap_or_default(),
            )),
            _ => DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!("unknown video encoder: {}", video_encoder),
            ),
        }
    }
}

/// Create a video encoder based on the settings.
pub fn create_video_encoder(
    desk_setting: &DeskSettings,
) -> Result<Box<dyn VideoEncoder + Send + Sync>, DeskError> {
    let encoder = match desk_setting.get_video_encoder_type()? {
        VideoEncoderType::H264(h264_encoder_settings) => {
            Box::new(H264Encoder::new(h264_encoder_settings))
        }
    };
    Ok(encoder)
}
