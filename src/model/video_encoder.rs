use crate::{
    desk_error::DeskError,
    model::{image_capture::ImageInfo, settings::H264EncoderSettings},
};

pub struct NalInfo {
    pub nal_bytes: bytes::Bytes,
}

pub trait VideoEncoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<NalInfo, DeskError>;
}

pub enum VideoEncoderType {
    H264(H264EncoderSettings),
}

pub trait VideoEncoderTypeHelper {
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, DeskError>;
}
