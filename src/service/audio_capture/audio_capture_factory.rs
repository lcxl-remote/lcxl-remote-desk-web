use std::str::FromStr;

use strum::IntoEnumIterator;

use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{AudioCapture, AudioCaptureType, AudioCaptureTypeHelper},
        settings::DeskSettings,
    },
    service::audio_capture::wasapi_capture::WasapiAudioCapture,
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
) -> Result<Box<dyn AudioCapture + Send + Sync>, DeskError> {
    let capture: Box<dyn AudioCapture + Send + Sync> =
        match desk_settings.get_audio_capture_type()? {
            AudioCaptureType::WASAPI => Box::new(WasapiAudioCapture::new(desk_settings)?),
        };
    Ok(capture)
}

pub fn audio_capture_list() -> Vec<String> {
    AudioCaptureType::iter()
        .map(|x| Into::<&'static str>::into(x).to_string())
        .collect()
}
