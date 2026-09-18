//! Durable device-owned Device Assistant product switch.

pub use desk_agent_protocol::device_assistant::{
    DeviceAssistantSettings, DeviceAssistantSettingsUpdate,
};

// Device initialization is separate from an unknown remote authority snapshot.
// Missing wire fields and unknown frontend projections must still fail closed.
pub(super) fn default_device_assistant_settings() -> DeviceAssistantSettings {
    DeviceAssistantSettings {
        revision: 0,
        enabled: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAssistantSettingsUpdateError {
    RevisionConflict(DeviceAssistantSettings),
    RevisionExhausted,
}

pub fn apply_device_assistant_settings_update(
    current: DeviceAssistantSettings,
    update: DeviceAssistantSettingsUpdate,
) -> Result<DeviceAssistantSettings, DeviceAssistantSettingsUpdateError> {
    if current.revision != update.expected_revision {
        return Err(DeviceAssistantSettingsUpdateError::RevisionConflict(
            current,
        ));
    }
    Ok(DeviceAssistantSettings {
        revision: current
            .revision
            .checked_add(1)
            .ok_or(DeviceAssistantSettingsUpdateError::RevisionExhausted)?,
        enabled: update.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_configuration_enables_assistant_by_default_and_preserves_explicit_off() {
        use crate::model::settings::Settings;
        assert!(Settings::default().device_assistant.enabled);
        let absent: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.device_assistant, default_device_assistant_settings());
        let disabled: Settings =
            serde_json::from_str(r#"{"device_assistant":{"revision":7,"enabled":false}}"#).unwrap();
        assert!(!disabled.device_assistant.enabled);
        assert_eq!(disabled.device_assistant.revision, 7);
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&disabled).unwrap()).unwrap();
        assert_eq!(restored.device_assistant, disabled.device_assistant);
    }

    #[test]
    fn default_is_revision_zero_and_disabled() {
        let settings = DeviceAssistantSettings::default();
        assert_eq!(settings.revision, 0);
        assert!(!settings.enabled);
        assert_eq!(
            serde_json::from_str::<DeviceAssistantSettings>("{}")
                .unwrap_err()
                .classify(),
            serde_json::error::Category::Data,
            "the public shared contract rejects incomplete snapshots"
        );
    }

    #[test]
    fn update_is_compare_and_set_and_revision_is_device_owned() {
        let current = DeviceAssistantSettings {
            revision: 7,
            enabled: false,
        };
        assert_eq!(
            apply_device_assistant_settings_update(
                current,
                DeviceAssistantSettingsUpdate {
                    expected_revision: 6,
                    enabled: true,
                },
            ),
            Err(DeviceAssistantSettingsUpdateError::RevisionConflict(
                current
            ))
        );
        assert_eq!(
            apply_device_assistant_settings_update(
                current,
                DeviceAssistantSettingsUpdate {
                    expected_revision: 7,
                    enabled: true,
                },
            )
            .unwrap(),
            DeviceAssistantSettings {
                revision: 8,
                enabled: true,
            }
        );
    }
}
