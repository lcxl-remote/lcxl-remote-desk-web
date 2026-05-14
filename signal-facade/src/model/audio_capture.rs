use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Selected Audio Device Model
#[derive(
    Debug,
    Clone,
    PartialEq,
    Serialize,
    Deserialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
pub struct SelectedAudioDevice {
    /// Audio data flow (render or capture)
    pub audio_data_flow: AudioDataFlow,
    /// Audio device id, None for default audio device
    pub audio_device_id: Option<String>,
}

impl Default for SelectedAudioDevice {
    fn default() -> Self {
        SelectedAudioDevice {
            audio_data_flow: AudioDataFlow::Render,
            audio_device_id: None,
        }
    }
}

/// Audio Data Flow Enum
#[derive(
    Debug,
    Clone,
    PartialEq,
    Serialize,
    Deserialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
pub enum AudioDataFlow {
    /// Render audio to speakers or headphones
    Render,
    /// Capture audio from microphone or other input devices
    Capture,
}

/// Audio Device Model
#[derive(
    Debug, Clone, Serialize, Deserialize, ToSchema, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct AudioDevice {
    /// Device id
    pub id: String,
    /// Audio device friendly name, e.g. "Speakers (Definition Audio)"
    pub firendly_name: String,
    /// Data flow of the device (render or capture)
    pub data_flow: AudioDataFlow,
    /// Is default device for this data flow?
    pub default: bool,
}

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn audio_device_round_trips_wincode() {
        // `AudioDevice` embeds `AudioDataFlow` — exercising both
        // variants here covers the nested-enum wincode derive at the
        // same time.
        let config = unbounded_config();
        for data_flow in [AudioDataFlow::Render, AudioDataFlow::Capture] {
            let device = AudioDevice {
                id: "mic-1".to_string(),
                firendly_name: "Microphone (Realtek)".to_string(),
                data_flow,
                default: true,
            };
            let bytes = wincode::config::serialize(&device, config).expect("encode");
            let back: AudioDevice = wincode::config::deserialize(&bytes, config).expect("decode");
            assert_eq!(back.id, device.id);
            assert_eq!(back.firendly_name, device.firendly_name);
            assert_eq!(back.default, device.default);
            // `AudioDataFlow` derives `PartialEq`; compare via match
            // since the type carries no `PartialEq` impl in production.
            match (back.data_flow, device.data_flow) {
                (AudioDataFlow::Render, AudioDataFlow::Render)
                | (AudioDataFlow::Capture, AudioDataFlow::Capture) => {}
                _ => panic!("data_flow round-trip mismatch"),
            }
        }
    }

    #[test]
    fn selected_audio_device_round_trips_wincode() {
        let config = unbounded_config();
        let original = SelectedAudioDevice {
            audio_data_flow: AudioDataFlow::Render,
            audio_device_id: Some("default-spk".to_string()),
        };
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: SelectedAudioDevice =
            wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.audio_device_id, original.audio_device_id);
    }
}
