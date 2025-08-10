use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Audio Data Flow Enum
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub enum AudioDataFlow {
    /// Render audio to speakers or headphones
    Render,
    /// Capture audio from microphone or other input devices
    Capture,
}

/// Audio Device Model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

/// Selected Audio Device Model
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
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
