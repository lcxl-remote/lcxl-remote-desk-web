use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::{error::DeskError, model::image_capture::ImageInfo};

pub struct NalInfo {
    pub nal_bytes: bytes::Bytes,
}

pub trait VideoEncoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<Vec<NalInfo>, DeskError>;
    /// Request the encoder to produce a keyframe (IDR) on the next encode call.
    /// Default implementation is a no-op for encoders that don't support native keyframe forcing.
    fn request_keyframe(&mut self) {}
}

#[derive(EnumIter, IntoStaticStr, EnumString, Debug, Clone, Copy)]
pub enum VideoEncoderType {
    X264,
    VP8,
    VP9,
    H264,
    AV1,
}

impl Default for VideoEncoderType {
    fn default() -> Self {
        return VideoEncoderType::X264;
    }
}
pub trait VideoEncoderTypeHelper {
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, DeskError>;
}
