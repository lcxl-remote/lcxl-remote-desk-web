use std::{collections::BTreeMap, str::FromStr};

use desk_signal_facade::model::{audio_capture::AudioDevice, desk_settings::DeskSettings};
use strum::IntoEnumIterator;

#[cfg(target_os = "linux")]
use crate::service::audio_capture::pipewire_capture::{
    PipewireAudioCapture, PipewireAudioDeviceEnumerator,
};
use crate::{
    error::DeskError,
    model::audio_capture::{
        AudioCapture, AudioCaptureType, AudioCaptureTypeHelper, AudioDeviceEnumerator,
    },
};

#[cfg(target_os = "windows")]
use crate::service::audio_capture::wasapi_capture::{
    WasapiAudioCapture, WasapiAudioDeviceEnumerator,
};

impl AudioCaptureTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_audio_capture_type(&self) -> Result<AudioCaptureType, DeskError> {
        if let Some(ref audio_capture) = self.audio_capture {
            let result = AudioCaptureType::from_str(audio_capture);
            if result.is_ok() {
                return Ok(result.unwrap());
            } else {
                log::error!(
                    "Failed to parse audio capture type: {}, use default setting, error: {}",
                    audio_capture,
                    result.err().unwrap()
                );
            }
        }

        Ok(AudioCaptureType::default())
    }
}

/// Create a video encoder based on the settings.
pub fn create_audio_capture(
    desk_settings: &DeskSettings,
) -> Result<Box<dyn AudioCapture>, DeskError> {
    let capture: Box<dyn AudioCapture> = match desk_settings.get_audio_capture_type()? {
        #[cfg(target_os = "windows")]
        AudioCaptureType::WASAPI => Box::new(WasapiAudioCapture::new(desk_settings)?),
        #[cfg(target_os = "linux")]
        AudioCaptureType::PIPEWIRE => Box::new(PipewireAudioCapture::new(desk_settings)?),
    };
    Ok(capture)
}

pub fn list_audio_capture() -> BTreeMap<String, Vec<AudioDevice>> {
    AudioCaptureType::iter()
        .map(|x| {
            (
                Into::<&'static str>::into(x).to_string(),
                list_audio_device(x).unwrap(),
            )
        })
        .collect()
}

pub fn list_audio_device(
    audio_capture_type: AudioCaptureType,
) -> Result<Vec<AudioDevice>, DeskError> {
    let capture: Box<dyn AudioDeviceEnumerator + Send> = match audio_capture_type {
        #[cfg(target_os = "windows")]
        AudioCaptureType::WASAPI => Box::new(WasapiAudioDeviceEnumerator::new()?),
        #[cfg(target_os = "linux")]
        AudioCaptureType::PIPEWIRE => Box::new(PipewireAudioDeviceEnumerator::new()),
    };
    let output_list = capture.get_device_list()?;

    Ok(output_list)
}
