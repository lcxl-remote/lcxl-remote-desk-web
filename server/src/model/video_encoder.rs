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
    H264,
    VP8,
    VP9,
}

impl Default for VideoEncoderType {
    fn default() -> Self {
        return VideoEncoderType::H264;
    }
}
pub trait VideoEncoderTypeHelper {
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, DeskError>;
}
