//! Durable device-owned AI Assistant product switch.

pub use desk_agent_protocol::ai_assistant::{AiAssistantSettings, AiAssistantSettingsUpdate};

// Device initialization is separate from an unknown remote authority snapshot.
// Missing wire fields and unknown frontend projections must still fail closed.
pub(super) fn default_ai_assistant_settings() -> AiAssistantSettings {
    AiAssistantSettings {
        revision: 0,
        enabled: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiAssistantSettingsUpdateError {
    RevisionConflict(AiAssistantSettings),
    RevisionExhausted,
}

pub fn apply_ai_assistant_settings_update(
    current: AiAssistantSettings,
    update: AiAssistantSettingsUpdate,
) -> Result<AiAssistantSettings, AiAssistantSettingsUpdateError> {
    if current.revision != update.expected_revision {
        return Err(AiAssistantSettingsUpdateError::RevisionConflict(current));
    }
    Ok(AiAssistantSettings {
        revision: current
            .revision
            .checked_add(1)
            .ok_or(AiAssistantSettingsUpdateError::RevisionExhausted)?,
        enabled: update.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_configuration_enables_assistant_by_default_and_preserves_explicit_off() {
        use crate::model::settings::Settings;
        assert!(Settings::default().ai_assistant.enabled);
        let absent: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.ai_assistant, default_ai_assistant_settings());
        let disabled: Settings =
            serde_json::from_str(r#"{"ai_assistant":{"revision":7,"enabled":false}}"#).unwrap();
        assert!(!disabled.ai_assistant.enabled);
        assert_eq!(disabled.ai_assistant.revision, 7);
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&disabled).unwrap()).unwrap();
        assert_eq!(restored.ai_assistant, disabled.ai_assistant);
    }

    #[test]
    fn default_is_revision_zero_and_disabled() {
        let settings = AiAssistantSettings::default();
        assert_eq!(settings.revision, 0);
        assert!(!settings.enabled);
        assert_eq!(
            serde_json::from_str::<AiAssistantSettings>("{}")
                .unwrap_err()
                .classify(),
            serde_json::error::Category::Data,
            "the public shared contract rejects incomplete snapshots"
        );
    }

    #[test]
    fn update_is_compare_and_set_and_revision_is_device_owned() {
        let current = AiAssistantSettings {
            revision: 7,
            enabled: false,
        };
        assert_eq!(
            apply_ai_assistant_settings_update(
                current,
                AiAssistantSettingsUpdate {
                    expected_revision: 6,
                    enabled: true,
                },
            ),
            Err(AiAssistantSettingsUpdateError::RevisionConflict(current))
        );
        assert_eq!(
            apply_ai_assistant_settings_update(
                current,
                AiAssistantSettingsUpdate {
                    expected_revision: 7,
                    enabled: true,
                },
            )
            .unwrap(),
            AiAssistantSettings {
                revision: 8,
                enabled: true,
            }
        );
    }
}
