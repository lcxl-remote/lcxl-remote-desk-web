use desk_utils::error::DeskErrorCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::model::image_capture::Resolution;
use crate::model::media_capability::VideoEncoderId;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead,
)]
#[serde(rename_all = "snake_case")]
pub enum MediaPipelinePhase {
    Streaming,
    Blocked,
    Failed,
}

/// Browser-visible state for one host-side media pipeline.
///
/// A blocked pipeline is deliberately not represented as an empty video: the
/// controller receives this typed state even when the SDP answer contains no
/// active video track or the worker exits before its first encoded frame.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead,
)]
pub struct MediaPipelineStateData {
    pub phase: MediaPipelinePhase,
    pub encoder: Option<VideoEncoderId>,
    pub source_resolution: Option<Resolution>,
    #[serde(default)]
    pub compatible_encoders: Vec<VideoEncoderId>,
    pub reason_code: Option<DeskErrorCode>,
    pub message: Option<String>,
}

impl MediaPipelineStateData {
    pub fn blocked_dimensions(
        encoder: Option<VideoEncoderId>,
        source_resolution: Resolution,
        compatible_encoders: Vec<VideoEncoderId>,
        message: String,
    ) -> Self {
        Self {
            phase: MediaPipelinePhase::Blocked,
            encoder,
            source_resolution: Some(source_resolution),
            compatible_encoders,
            reason_code: Some(DeskErrorCode::VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED),
            message: Some(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_code_serializes_as_shared_business_code() {
        let state = MediaPipelineStateData::blocked_dimensions(
            Some(VideoEncoderId::OpenH264),
            Resolution::new(4096, 2160),
            vec![VideoEncoderId::X264],
            "unsupported".to_string(),
        );
        let value = serde_json::to_value(state).unwrap();
        assert_eq!(value["reason_code"], 89);
        assert_eq!(value["phase"], "blocked");
    }
}
