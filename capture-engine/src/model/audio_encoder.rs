use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::{error::CaptureError, model::audio_capture::AudioBuffer};

pub struct EncodedAudioBuffer {
    pub data: Vec<u8>,            // Raw audio data
    pub origin_num_frames: usize, // Number of frames in the original
}

pub trait AudioEncoder {
    fn encode(
        &mut self,
        audio_buffer: &dyn AudioBuffer,
    ) -> Result<EncodedAudioBuffer, CaptureError>;
}

/// Audio Encoder Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString, Default)]
pub enum AudioEncoderType {
    /// Opus encoder
    #[default]
    OPUS,
}

pub trait AudioEncoderTypeHelper {
    fn get_audio_encoder_type(&self) -> Result<AudioEncoderType, CaptureError>;
}
