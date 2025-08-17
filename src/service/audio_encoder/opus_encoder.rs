use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::AudioBuffer,
        audio_encoder::{AudioEncoder, EncodedAudioBuffer},
    },
};

pub struct OpusAudioEncoder {
    //pub encoder: opusic_c::Encoder,
    pub encoder: opus::Encoder,

    pub buffer: Vec<u8>,
}

/// Workaround for Arc not being Send + Sync
/// This is only works in single thread, so it is safe to use in this case.
unsafe impl Send for OpusAudioEncoder {}

unsafe impl Sync for OpusAudioEncoder {}

impl OpusAudioEncoder {
    pub fn new() -> Result<Self, DeskError> {
        let encoder = opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Audio)?;
        Ok(Self {
            encoder,
            buffer: vec![],
        })
    }
}

impl AudioEncoder for OpusAudioEncoder {
    fn encode(&mut self, audio_buffer: &dyn AudioBuffer) -> Result<EncodedAudioBuffer, DeskError> {
        todo!()
    }
}
