use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::error::CaptureError;
use crate::model::image_capture::ImageInfo;

pub struct NalInfo {
    pub nal_bytes: bytes::Bytes,
    /// True if this NAL belongs to a keyframe access unit (IDR for H.264,
    /// key frame for VP8/VP9/AV1). Set by the encoder using its native
    /// frame-type signal (no NAL byte scanning at the call site). Lets the
    /// emit path label `MediaFrameKind::VideoI` accurately for both
    /// rebuild-driven IDRs and the encoder's internal periodic IDRs, so
    /// stats logged on the host match what the browser decoder counts.
    pub is_keyframe: bool,
}

pub trait VideoEncoder {
    /// Encode a freshly-captured frame. `enable_dirty_rect` gates whether
    /// the implementation may honour `ImageInfo::get_dirty_rects` to skip
    /// or partially update its internal YUV buffer. Pass `false` to force
    /// a full BGRA→YUV conversion on every call.
    fn encode(
        &mut self,
        image_info: &dyn ImageInfo,
        enable_dirty_rect: bool,
    ) -> Result<Vec<NalInfo>, CaptureError>;
    /// Re-encode using the cached YUV buffer without consuming new frame data.
    /// Used for heartbeat frames when the desktop is static.
    /// Returns an empty vec if no YUV buffer has been populated yet.
    fn encode_cached(&mut self) -> Result<Vec<NalInfo>, CaptureError> {
        Ok(vec![])
    }
    /// Request the encoder to produce a keyframe (IDR) on the next encode call.
    /// Default implementation is a no-op for encoders that don't support native keyframe forcing.
    fn request_keyframe(&mut self) {}
}

#[derive(EnumIter, IntoStaticStr, EnumString, Debug, Clone, Copy, Default)]
pub enum VideoEncoderType {
    #[default]
    X264,
    VP8,
    VP9,
    H264,
    AV1,
}

pub trait VideoEncoderTypeHelper {
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, CaptureError>;
}
