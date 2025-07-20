use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub enum AudioDataFlow {
    Render,
    Capture,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AudioDevice {
    /// device id
    pub id: String,
    /// audio device friendly name, e.g. "Speakers (Definition Audio)"
    pub firendly_name: String,
    /// data flow of the device (render or capture)
    pub data_flow: AudioDataFlow,
    /// is default device for this data flow?
    pub default: bool,
}

/// Selected Audio Device Model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SelectedAudioDevice {
    pub audio_data_flow: AudioDataFlow,
    /// audio device id, None for default audio device
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
