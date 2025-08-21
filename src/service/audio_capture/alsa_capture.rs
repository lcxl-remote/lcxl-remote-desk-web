use crate::{
    desk_error::DeskError,
    model::{audio_capture::AudioCapture, settings::DeskSettings},
};

pub struct AlsaAudioCapture {}

impl AudioCapture for AlsaAudioCapture {
    fn start(
        &mut self,
    ) -> Result<crate::model::audio_capture::WaveFormat, crate::desk_error::DeskError> {
        todo!()
    }

    fn get_devices_list(
        &self,
    ) -> Result<Vec<crate::model::audio_capture::AudioDevice>, crate::desk_error::DeskError> {
        todo!()
    }

    fn get_buffer(
        &self,
    ) -> Result<
        Box<dyn crate::model::audio_capture::AudioBuffer + Send + Sync>,
        crate::desk_error::DeskError,
    > {
        todo!()
    }

    fn stop(&mut self) -> Result<(), crate::desk_error::DeskError> {
        todo!()
    }
}

impl AlsaAudioCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        Ok(Self {})
    }
}
