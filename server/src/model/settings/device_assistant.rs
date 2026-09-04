//! Durable device-owned Device Assistant product switch.

pub use desk_agent_protocol::device_assistant::{
    DeviceAssistantSettings, DeviceAssistantSettingsUpdate,
};

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
