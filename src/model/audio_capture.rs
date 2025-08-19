use serde::{Deserialize, Serialize};
use strum_macros::{EnumIter, EnumString, IntoStaticStr};
use utoipa::ToSchema;

use crate::desk_error::DeskError;

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

/// Audio Buffer Trait
pub trait AudioBuffer {
    /// Get raw buffer as byte slice
    fn get_buffer_slice(&self) -> &[u8];

    /// Get number of frames in the buffer
    fn get_num_frames(&self) -> usize;

    /// Get buffer as slice of specific type
    fn get_f32_buffer_slice(&self) -> &[f32] {
        align_slice_byte::<f32>(self.get_buffer_slice())
    }

    /// Get buffer as slice of specific type
    fn get_u16_buffer_slice(&self) -> &[u16] {
        align_slice_byte::<u16>(self.get_buffer_slice())
    }
}

pub fn align_slice_byte<T>(data: &[u8]) -> &[T] {
    let type_size = std::mem::size_of::<T>();
    if data.len() % type_size != 0 {
        panic!("Data length is not aligned with type size");
    }
    let num_t = data.len() / type_size;
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const T, num_t) }
}

pub trait AudioCapture {
    /// Start capturing audio
    fn start(&mut self) -> Result<WaveFormat, DeskError>;
    /// Enumerates audio devices based on the specified data flow.
    fn get_devices_list(&self) -> Result<Vec<AudioDevice>, DeskError>;

    /// Get audio buffer
    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError>;

    fn stop(&mut self) -> Result<(), DeskError>;
}

/// Image Capture Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString)]
pub enum AudioCaptureType {
    /// Capture audio from WASAPI device
    WASAPI,
}

impl Default for AudioCaptureType {
    fn default() -> Self {
        AudioCaptureType::WASAPI
    }
}

pub trait AudioCaptureTypeHelper {
    fn get_audio_capture_type(&self) -> Result<AudioCaptureType, DeskError>;
}

#[derive(Clone, Copy, Default)]
pub struct WaveFormat {
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
}
