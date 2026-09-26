//! Whole-output Wayland input authority, separate from application authority.
use super::*;

/// Issued alongside an observed output. The worker retains the complete native
/// stream, geometry and frame identity behind the opaque output reference.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct OutputFrameBinding {
    pub stream_generation: u64,
    pub observation_id: String,
    pub received_at_unix_ms: u64,
    pub freshness: crate::ScreenFrameFreshness,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WaylandOutputInputAction {
    pub screen: RawInputScreenContext,
    pub frame: OutputFrameBinding,
    pub step: RawInputStep,
}

impl WaylandOutputInputAction {
    /// This validates shape only. The original worker must compare every field
    /// against its retained observation and recheck all runtime input gates.
    pub fn validate(&self) -> Result<(), ComputerUseValidationError> {
        RawInputAction {
            screen: self.screen.clone(),
            step: self.step.clone(),
        }
        .validate()?;
        if self.frame.stream_generation == 0
            || self.frame.received_at_unix_ms == 0
            || self.frame.observation_id.is_empty()
            || self.frame.observation_id.len() > 128
            || !matches!(
                self.frame.freshness,
                crate::ScreenFrameFreshness::Fresh
                    | crate::ScreenFrameFreshness::UnchangedVerified
                    | crate::ScreenFrameFreshness::LatestObserved
            )
        {
            return Err(ComputerUseValidationError::InvalidContextReference(
                "output input requires an original frame observation",
            ));
        }
        if matches!(&self.step, RawInputStep::TypeText { text } if text.chars().count() > 64) {
            return Err(ComputerUseValidationError::InvalidContextReference(
                "output text exceeds the bounded input batch",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_observed_shape_is_allowed_but_geometry_is_still_bounded() {
        let mut action = WaylandOutputInputAction {
            screen: RawInputScreenContext {
                display: "output-1".into(),
                width: 1920,
                height: 1080,
                dpi_x: 96,
                dpi_y: 96,
            },
            frame: OutputFrameBinding {
                stream_generation: 1,
                observation_id: "frame-1".into(),
                received_at_unix_ms: 1,
                freshness: crate::ScreenFrameFreshness::LatestObserved,
            },
            step: RawInputStep::Click {
                x: 100,
                y: 100,
                button: RawInputMouseButton::Primary,
            },
        };
        assert!(action.validate().is_ok());
        action.frame.freshness = crate::ScreenFrameFreshness::Fresh;
        assert!(action.validate().is_ok());
        action.step = RawInputStep::Click {
            x: 1920,
            y: 100,
            button: RawInputMouseButton::Primary,
        };
        assert!(action.validate().is_err());
    }
}
