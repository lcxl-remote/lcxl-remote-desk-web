use alsa::{Direction, PCM};

use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{AudioBuffer, AudioCapture, AudioDevice, WaveFormat},
        settings::DeskSettings,
    },
};

///https://stackoverflow.com/questions/75576834/how-do-i-capture-currently-playing-audio-from-another-application-in-rust
pub struct AlsaAudioCapture {
    pub pcm: PCM,
}

impl AudioCapture for AlsaAudioCapture {
    fn start(&mut self) -> Result<WaveFormat, DeskError> {
        self.pcm.start()?;
        let wave_format = WaveFormat::default();
        Ok(wave_format)
    }

    fn get_devices_list(&self) -> Result<Vec<AudioDevice>, DeskError> {
        todo!()
    }

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError> {
        todo!()
    }

    fn stop(&mut self) -> Result<(), DeskError> {
        self.pcm.drop();
        Ok(())
    }
}

impl AlsaAudioCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        let pcm = PCM::new("default", Direction::Capture, false)?;
        Ok(Self { pcm })
    }
}
