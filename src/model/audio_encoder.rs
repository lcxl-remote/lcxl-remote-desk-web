use crate::{desk_error::DeskError, model::audio_capture::AudioBuffer};

pub trait AudioEncoder {
    fn encode(&mut self, image_info: &dyn AudioBuffer) -> Result<(), DeskError>;
}
