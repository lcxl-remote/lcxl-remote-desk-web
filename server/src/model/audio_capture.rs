use serde::{Deserialize, Serialize};
use strum_macros::{EnumIter, EnumString, IntoStaticStr};
use utoipa::ToSchema;

use crate::error::DeskError;

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

    /// Get audio buffer
    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError>;

    fn stop(&mut self) -> Result<(), DeskError>;
}

pub trait AudioDeviceEnumerator {
    /// Enumerates audio devices based on the specified data flow.
    fn get_device_list(&self) -> Result<Vec<AudioDevice>, DeskError>;
}

/// Image Capture Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString, Debug, Clone, Copy)]
pub enum AudioCaptureType {
    /// Capture audio from WASAPI device
    #[cfg(target_os = "windows")]
    WASAPI,
    #[cfg(target_os = "linux")]
    PIPEWIRE,
}

impl Default for AudioCaptureType {
    fn default() -> Self {
        #[cfg(target_os = "windows")]
        return AudioCaptureType::WASAPI;
        #[cfg(target_os = "linux")]
        return AudioCaptureType::PIPEWIRE;
    }
}

pub trait AudioCaptureTypeHelper {
    fn get_audio_capture_type(&self) -> Result<AudioCaptureType, DeskError>;
}

#[derive(Clone, Copy, Default, Debug)]
pub struct WaveFormat {
    /// Waveform-audio format type. Format tags are registered with Microsoft Corporation for many compression algorithms. A complete list of format tags can be found in the Mmreg.h header file. For one- or two-channel PCM data, this value should be WAVE_FORMAT_PCM. When this structure is included in a WAVEFORMATEXTENSIBLE structure, this value must be WAVE_FORMAT_EXTENSIBLE.
    pub format_tag: u16,
    /// Number of channels in the waveform-audio data. Monaural data uses one channel and stereo data uses two channels.
    pub channels: u16,
    /// Sample rate, in samples per second (hertz). If wFormatTag is WAVE_FORMAT_PCM, then common values for nSamplesPerSec are 8.0 kHz, 11.025 kHz, 22.05 kHz, and 44.1 kHz. For non-PCM formats, this member must be computed according to the manufacturer's specification of the format tag.
    pub samples_per_sec: u32,
    /// Required average data-transfer rate, in bytes per second, for the format tag. If wFormatTag is WAVE_FORMAT_PCM, nAvgBytesPerSec should be equal to the product of nSamplesPerSec and nBlockAlign. For non-PCM formats, this member must be computed according to the manufacturer's specification of the format tag.
    pub avg_bytes_per_sec: u32,
    /// Block alignment, in bytes. The block alignment is the minimum atomic unit of data for the wFormatTag format type. If wFormatTag is WAVE_FORMAT_PCM or WAVE_FORMAT_EXTENSIBLE, nBlockAlign must be equal to the product of nChannels and wBitsPerSample divided by 8 (bits per byte). For non-PCM formats, this member must be computed according to the manufacturer's specification of the format tag.
    ///
    ///Software must process a multiple of nBlockAlign bytes of data at a time. Data written to and read from a device must always start at the beginning of a block. For example, it is illegal to start playback of PCM data in the middle of a sample (that is, on a non-block-aligned boundary).
    pub block_align: u16,
    /// Bits per sample for the wFormatTag format type. If wFormatTag is WAVE_FORMAT_PCM, then wBitsPerSample should be equal to 8 or 16. For non-PCM formats, this member must be set according to the manufacturer's specification of the format tag. If wFormatTag is WAVE_FORMAT_EXTENSIBLE, this value can be any integer multiple of 8 and represents the container size, not necessarily the sample size; for example, a 20-bit sample size is in a 24-bit container. Some compression schemes cannot define a value for wBitsPerSample, so this member can be 0.    
    pub bits_per_sample: u16,
}
