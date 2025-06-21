use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioDataFlow {
    Render,
    Capture,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
