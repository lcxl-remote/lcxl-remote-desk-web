use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::model::image_capture::Resolution;

/// Concrete video encoder implementation.
///
/// This is intentionally distinct from the RTP codec: X264 and OpenH264
/// both produce H.264, but the worker must preserve which implementation
/// the controller selected.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    ToSchema,
    SchemaWrite,
    SchemaRead,
)]
pub enum VideoEncoderId {
    #[default]
    #[serde(rename = "X264")]
    X264,
    #[serde(rename = "OpenH264")]
    OpenH264,
    #[serde(rename = "VP8")]
    Vp8,
    #[serde(rename = "VP9")]
    Vp9,
    #[serde(rename = "AV1")]
    Av1,
}

impl VideoEncoderId {
    /// Capture-engine setting string consumed by `VideoEncoderType::from_str`.
    pub const fn setting_name(self) -> &'static str {
        match self {
            Self::X264 => "X264",
            Self::OpenH264 => "H264",
            Self::Vp8 => "VP8",
            Self::Vp9 => "VP9",
            Self::Av1 => "AV1",
        }
    }

    /// Parse the existing capture-engine setting spelling.
    pub fn from_setting_name(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "X264" => Some(Self::X264),
            "H264" | "OPENH264" => Some(Self::OpenH264),
            "VP8" => Some(Self::Vp8),
            "VP9" => Some(Self::Vp9),
            "AV1" => Some(Self::Av1),
            _ => None,
        }
    }
}

pub const AUTO_ENCODER_ORDER: &[VideoEncoderId] = &[
    VideoEncoderId::X264,
    VideoEncoderId::Vp8,
    VideoEncoderId::Vp9,
    VideoEncoderId::OpenH264,
    VideoEncoderId::Av1,
];

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead,
)]
pub struct EncoderInputLimits {
    pub max_landscape: Option<Resolution>,
    pub max_portrait: Option<Resolution>,
    pub width_alignment: u32,
    pub height_alignment: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead,
)]
pub enum EncoderInputSupport {
    Known(EncoderInputLimits),
    RuntimeProbeRequired,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead,
)]
pub struct VideoEncoderCapability {
    pub id: VideoEncoderId,
    pub input_support: EncoderInputSupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderCompatibility {
    Compatible,
    RuntimeProbeRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderCompatibilityError {
    EmptyDimensions,
    WidthAlignment { required: u32 },
    HeightAlignment { required: u32 },
    DimensionsExceeded { max_width: u32, max_height: u32 },
}

impl VideoEncoderCapability {
    /// Evidence-backed limits for the encoder implementation.
    pub fn for_id(id: VideoEncoderId) -> Self {
        let input_support = match id {
            // The x264 wrapper passes signed 32-bit dimensions directly to
            // x264 and I420 requires even dimensions. It publishes no smaller
            // product cap, so display-sized inputs are bounded by alignment.
            VideoEncoderId::X264 => EncoderInputSupport::Known(EncoderInputLimits {
                max_landscape: None,
                max_portrait: None,
                width_alignment: 2,
                height_alignment: 2,
            }),
            // OpenH264 reports these orientation-specific maxima at encode
            // time. Preflight mirrors that deterministic rejection.
            VideoEncoderId::OpenH264 => EncoderInputSupport::Known(EncoderInputLimits {
                max_landscape: Some(Resolution::new(3840, 2160)),
                max_portrait: Some(Resolution::new(2160, 3840)),
                width_alignment: 2,
                height_alignment: 2,
            }),
            // The currently locked wrappers expose creation failure but no
            // stable product-level maximum that can be advertised honestly.
            VideoEncoderId::Vp8 | VideoEncoderId::Vp9 | VideoEncoderId::Av1 => {
                EncoderInputSupport::RuntimeProbeRequired
            }
        };
        Self { id, input_support }
    }
}

pub fn capabilities_for_encoder_names(
    names: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<VideoEncoderCapability> {
    names
        .into_iter()
        .filter_map(|name| VideoEncoderId::from_setting_name(name.as_ref()))
        .map(VideoEncoderCapability::for_id)
        .collect()
}

pub fn check_encoder_input(
    source: Resolution,
    support: &EncoderInputSupport,
) -> Result<EncoderCompatibility, EncoderCompatibilityError> {
    let EncoderInputSupport::Known(limits) = support else {
        return Ok(EncoderCompatibility::RuntimeProbeRequired);
    };
    if source.width == 0 || source.height == 0 {
        return Err(EncoderCompatibilityError::EmptyDimensions);
    }
    if limits.width_alignment > 1 && !source.width.is_multiple_of(limits.width_alignment) {
        return Err(EncoderCompatibilityError::WidthAlignment {
            required: limits.width_alignment,
        });
    }
    if limits.height_alignment > 1 && !source.height.is_multiple_of(limits.height_alignment) {
        return Err(EncoderCompatibilityError::HeightAlignment {
            required: limits.height_alignment,
        });
    }

    let maximum = if source.width >= source.height {
        limits.max_landscape.as_ref()
    } else {
        limits.max_portrait.as_ref()
    };
    if let Some(maximum) = maximum
        && (source.width > maximum.width || source.height > maximum.height)
    {
        return Err(EncoderCompatibilityError::DimensionsExceeded {
            max_width: maximum.width,
            max_height: maximum.height,
        });
    }
    Ok(EncoderCompatibility::Compatible)
}

pub fn compatible_encoders(
    source: Resolution,
    capabilities: &[VideoEncoderCapability],
) -> Vec<VideoEncoderId> {
    AUTO_ENCODER_ORDER
        .iter()
        .copied()
        .filter(|id| {
            capabilities.iter().any(|capability| {
                capability.id == *id
                    && check_encoder_input(source, &capability.input_support)
                        == Ok(EncoderCompatibility::Compatible)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_names_round_trip_without_collapsing_h264_implementations() {
        let cases = [
            (VideoEncoderId::X264, "X264"),
            (VideoEncoderId::OpenH264, "H264"),
            (VideoEncoderId::Vp8, "VP8"),
            (VideoEncoderId::Vp9, "VP9"),
            (VideoEncoderId::Av1, "AV1"),
        ];

        for (id, setting) in cases {
            assert_eq!(VideoEncoderId::from_setting_name(setting), Some(id));
            assert_eq!(id.setting_name(), setting);
        }
        assert_ne!(VideoEncoderId::X264, VideoEncoderId::OpenH264);
    }

    #[test]
    fn serde_uses_stable_browser_facing_names() {
        assert_eq!(
            serde_json::to_string(&VideoEncoderId::OpenH264).unwrap(),
            r#""OpenH264""#
        );
        assert_eq!(
            serde_json::from_str::<VideoEncoderId>(r#""VP9""#).unwrap(),
            VideoEncoderId::Vp9
        );
    }

    #[test]
    fn auto_order_is_explicit_and_default_first() {
        assert_eq!(AUTO_ENCODER_ORDER[0], VideoEncoderId::default());
        assert_eq!(
            AUTO_ENCODER_ORDER,
            &[
                VideoEncoderId::X264,
                VideoEncoderId::Vp8,
                VideoEncoderId::Vp9,
                VideoEncoderId::OpenH264,
                VideoEncoderId::Av1,
            ]
        );
    }

    #[test]
    fn openh264_rejects_dci_4k_before_encode() {
        let capability = VideoEncoderCapability::for_id(VideoEncoderId::OpenH264);
        assert_eq!(
            check_encoder_input(Resolution::new(3840, 2160), &capability.input_support),
            Ok(EncoderCompatibility::Compatible)
        );
        assert_eq!(
            check_encoder_input(Resolution::new(4096, 2160), &capability.input_support),
            Err(EncoderCompatibilityError::DimensionsExceeded {
                max_width: 3840,
                max_height: 2160,
            })
        );
    }

    #[test]
    fn auto_skips_runtime_probe_and_keeps_x264_first() {
        let capabilities = [
            VideoEncoderCapability::for_id(VideoEncoderId::Vp8),
            VideoEncoderCapability::for_id(VideoEncoderId::OpenH264),
            VideoEncoderCapability::for_id(VideoEncoderId::X264),
        ];
        assert_eq!(
            compatible_encoders(Resolution::new(4096, 2160), &capabilities),
            vec![VideoEncoderId::X264]
        );
    }
}
