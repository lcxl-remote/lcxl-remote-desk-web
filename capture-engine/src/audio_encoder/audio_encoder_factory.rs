use std::str::FromStr;

use desk_signal_facade::model::desk_settings::DeskSettings;
use strum::IntoEnumIterator;

use crate::{
    audio_encoder::opus_encoder::OpusAudioEncoder,
    error::CaptureError,
    model::{
        audio_capture::WaveFormat,
        audio_encoder::{AudioEncoder, AudioEncoderType, AudioEncoderTypeHelper},
    },
};

impl AudioEncoderTypeHelper for DeskSettings {
    fn get_audio_encoder_type(&self) -> Result<AudioEncoderType, CaptureError> {
        if let Some(ref audio_encoder) = self.audio_encoder {
            let result = AudioEncoderType::from_str(audio_encoder);
            if result.is_ok() {
                return Ok(result.unwrap());
            } else {
                log::error!(
                    "Failed to parse audio encoder type: {}, use default setting, error: {}",
                    audio_encoder,
                    result.err().unwrap()
                );
            }
        }

        Ok(AudioEncoderType::default())
    }
}

/// Create a video encoder based on the settings.
pub fn create_audio_encoder(
    desk_settings: &DeskSettings,
    wave_format: WaveFormat,
) -> Result<Box<dyn AudioEncoder>, CaptureError> {
    let capture: Box<dyn AudioEncoder> = match desk_settings.get_audio_encoder_type()? {
        AudioEncoderType::OPUS => Box::new(OpusAudioEncoder::new(desk_settings, wave_format)?),
    };
    Ok(capture)
}

pub fn list_audio_encoder() -> Vec<String> {
    AudioEncoderType::iter()
        .map(|x| Into::<&'static str>::into(x).to_string())
        .collect()
}
