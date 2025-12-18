use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::{error::DeskError, model::audio_capture::AudioBuffer};

pub struct EncodedAudioBuffer {
    pub data: Vec<u8>,            // Raw audio data
    pub origin_num_frames: usize, // Number of frames in the original
}

pub trait AudioEncoder {
    fn encode(&mut self, audio_buffer: &dyn AudioBuffer) -> Result<EncodedAudioBuffer, DeskError>;
}

/// Audio Encoder Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString)]
pub enum AudioEncoderType {
    /// Opus encoder
    OPUS,
}

impl Default for AudioEncoderType {
    fn default() -> Self {
        AudioEncoderType::OPUS
    }
}

pub trait AudioEncoderTypeHelper {
    fn get_audio_encoder_type(&self) -> Result<AudioEncoderType, DeskError>;
}
