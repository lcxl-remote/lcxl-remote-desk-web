use std::{collections::BTreeMap, str::FromStr};

use strum::IntoEnumIterator;

#[cfg(target_os = "linux")]
use crate::service::audio_capture::alsa_capture::{AlsaAudioCapture, AlsaAudioDeviceEnumerator};
use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{
            AudioCapture, AudioCaptureType, AudioCaptureTypeHelper, AudioDevice,
            AudioDeviceEnumerator,
        },
        common::ErrorCode,
        settings::DeskSettings,
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
) -> Result<Box<dyn AudioCapture + Send>, DeskError> {
    let capture: Box<dyn AudioCapture + Send> = match desk_settings.get_audio_capture_type()? {
        #[cfg(target_os = "windows")]
        AudioCaptureType::WASAPI => Box::new(WasapiAudioCapture::new(desk_settings)?),
        #[cfg(target_os = "linux")]
        AudioCaptureType::ALSA => Box::new(AlsaAudioCapture::new(desk_settings)?),
    };
    Ok(capture)
}

pub fn audio_capture_list() -> BTreeMap<String, Vec<AudioDevice>> {
    AudioCaptureType::iter()
        .map(|x| {
            (
                Into::<&'static str>::into(x).to_string(),
                get_audio_device_list(x).unwrap(),
            )
        })
        .collect()
}

pub fn get_audio_device_list(
    audio_capture_type: AudioCaptureType,
) -> Result<Vec<AudioDevice>, DeskError> {
    let capture: Box<dyn AudioDeviceEnumerator + Send> = match audio_capture_type {
        #[cfg(target_os = "windows")]
        AudioCaptureType::WASAPI => Box::new(WasapiAudioDeviceEnumerator::new()),
        #[cfg(target_os = "linux")]
        AudioCaptureType::ALSA => Box::new(AlsaAudioDeviceEnumerator::new()),
        _ => {
            return DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!("Unsupported capture type:{:?}", audio_capture_type),
            );
        }
    };
    let output_list = capture.get_device_list()?;

    Ok(output_list)
}
