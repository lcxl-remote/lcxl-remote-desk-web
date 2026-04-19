use desk_signal_facade::model::audio_capture::AudioDevice;
use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::error::CaptureError;

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
    fn start(&mut self) -> Result<WaveFormat, CaptureError>;

    /// Get audio buffer
    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, CaptureError>;

    fn stop(&mut self) -> Result<(), CaptureError>;
}

pub trait AudioDeviceEnumerator {
    /// Enumerates audio devices based on the specified data flow.
    fn get_device_list(&self) -> Result<Vec<AudioDevice>, CaptureError>;
}

/// Audio Capture Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString, Debug, Clone, Copy)]
pub enum AudioCaptureType {
    /// Capture audio from WASAPI device
    #[cfg(target_os = "windows")]
    WASAPI,
    #[cfg(target_os = "linux")]
    PIPEWIRE,
    #[cfg(target_os = "macos")]
    SCKIT,
}

impl Default for AudioCaptureType {
    fn default() -> Self {
        #[cfg(target_os = "windows")]
        return AudioCaptureType::WASAPI;
        #[cfg(target_os = "linux")]
        return AudioCaptureType::PIPEWIRE;
        #[cfg(target_os = "macos")]
        return AudioCaptureType::SCKIT;
    }
}

pub trait AudioCaptureTypeHelper {
    fn get_audio_capture_type(&self) -> Result<AudioCaptureType, CaptureError>;
}

#[derive(Clone, Copy, Default, Debug)]
pub struct WaveFormat {
    /// Waveform-audio format type.
    pub format_tag: u16,
    /// Number of channels in the waveform-audio data.
    pub channels: u16,
    /// Sample rate, in samples per second (hertz).
    pub samples_per_sec: u32,
    /// Required average data-transfer rate, in bytes per second.
    pub avg_bytes_per_sec: u32,
    /// Block alignment, in bytes.
    pub block_align: u16,
    /// Bits per sample for the wFormatTag format type.
    pub bits_per_sample: u16,
}
