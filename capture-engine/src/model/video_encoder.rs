use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use desk_signal_facade::model::media_capability::VideoEncoderId;

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

    /// Applies (`Some(kbps)`) or clears (`None` — restore the encoder's
    /// initial ceiling) a runtime bitrate cap, without rebuilding the
    /// encoder. Quality-driven rate control is unaffected below the
    /// cap; the cap only limits bitrate spikes under heavy motion.
    ///
    /// Returns `false` when the codec implementation does not support
    /// runtime rate changes (or the change failed) so callers can skip
    /// further cap updates for this encoder.
    fn set_bitrate_cap(&mut self, _cap_kbps: Option<u32>) -> bool {
        false
    }
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

impl From<VideoEncoderType> for VideoEncoderId {
    fn from(value: VideoEncoderType) -> Self {
        match value {
            VideoEncoderType::X264 => Self::X264,
            VideoEncoderType::H264 => Self::OpenH264,
            VideoEncoderType::VP8 => Self::Vp8,
            VideoEncoderType::VP9 => Self::Vp9,
            VideoEncoderType::AV1 => Self::Av1,
        }
    }
}

impl From<VideoEncoderId> for VideoEncoderType {
    fn from(value: VideoEncoderId) -> Self {
        match value {
            VideoEncoderId::X264 => Self::X264,
            VideoEncoderId::OpenH264 => Self::H264,
            VideoEncoderId::Vp8 => Self::VP8,
            VideoEncoderId::Vp9 => Self::VP9,
            VideoEncoderId::Av1 => Self::AV1,
        }
    }
}

pub trait VideoEncoderTypeHelper {
    fn get_video_encoder_type(&self) -> Result<VideoEncoderType, CaptureError>;
}
