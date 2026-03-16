use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::{error::DeskError, model::image_capture::ImageInfo};

pub struct NalInfo {
    pub nal_bytes: bytes::Bytes,
}

pub trait VideoEncoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<Vec<NalInfo>, DeskError>;
}

#[derive(EnumIter, IntoStaticStr, EnumString, Debug, Clone, Copy)]
pub enum VideoEncoderType {
    X264,
    VP8,
    VP9,
    H264,
}

impl Default for VideoEncoderType {
    fn default() -> Self {
        return VideoEncoderType::X264;
    }
}
pub trait VideoEncoderTypeHelper {
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, DeskError>;
}
