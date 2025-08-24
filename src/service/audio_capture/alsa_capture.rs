use std::ffi::CString;

use alsa::{
    Direction, PCM, ValueOr,
    device_name::HintIter,
    pcm::{Access, Format, HwParams},
};

use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{AudioBuffer, AudioCapture, AudioDataFlow, AudioDevice, WaveFormat},
        settings::DeskSettings,
    },
};

///https://stackoverflow.com/questions/75576834/how-do-i-capture-currently-playing-audio-from-another-application-in-rust
pub struct AlsaAudioCapture {
    pub pcm: PCM,
}

impl AudioCapture for AlsaAudioCapture {
    fn start(&mut self) -> Result<WaveFormat, DeskError> {
        {
            // For this example, we assume 44100Hz, one channel, 16 bit audio.
            let hwp = HwParams::any(&self.pcm)?;
            hwp.set_channels(1)?;
            hwp.set_rate(44100, ValueOr::Nearest)?;
            hwp.set_format(Format::s16())?;
            hwp.set_access(Access::RWInterleaved)?;
            self.pcm.hw_params(&hwp)?;
        }
        self.pcm.start()?;
        let wave_format = WaveFormat::default();
        Ok(wave_format)
    }

    fn get_devices_list(&self) -> Result<Vec<AudioDevice>, DeskError> {
         let mut audio_device_list = vec![];
        let hint_iter = HintIter::new(None, &*CString::new("pcm").unwrap()).unwrap();
        for hint in hint_iter {
            log::info!("{:?}", hint);
            audio_device_list.push( AudioDevice{
                id: hint.name.unwrap_or_default(),
                firendly_name: hint.desc.unwrap_or_default(),
                data_flow: AudioDataFlow::Capture,
                default: false,
            });
        }
       
        Ok(audio_device_list)
    }

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError> {
        todo!()
    }

    fn stop(&mut self) -> Result<(), DeskError> {
        self.pcm.drop()?;
        Ok(())
    }
}

impl AlsaAudioCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        let pcm = PCM::new("default", Direction::Capture, false)?;
        Ok(Self { pcm })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use log::LevelFilter;

    use super::*;
    use crate::utils::logs::init_logs;
    static INIT: Once = Once::new();
    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            init_logs(LevelFilter::Debug).unwrap();
        });
    }

    #[test]
    fn test_device_info() -> Result<(), DeskError> {
        initialize();
        let desk_settings = DeskSettings::default();
        let capture = AlsaAudioCapture::new(&desk_settings)?;
        let devices = capture.get_devices_list()?;
        log::debug!("all devices: {:?}", devices);
        Ok(())
    }
}
