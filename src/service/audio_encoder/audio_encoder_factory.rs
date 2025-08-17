use std::str::FromStr;

use crate::{
    desk_error::DeskError,
    model::{
        audio_encoder::{AudioEncoderType, AudioEncoderTypeHelper},
        settings::DeskSettings,
    },
};

impl AudioEncoderTypeHelper for DeskSettings {
    fn get_audio_encoder_type(&self) -> Result<AudioEncoderType, DeskError> {
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
