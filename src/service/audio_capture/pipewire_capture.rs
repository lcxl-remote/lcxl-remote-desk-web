use std::ffi::CString;

use alsa::{
    Direction, PCM, ValueOr,
    device_name::HintIter,
    pcm::{Access, Format, HwParams},
};
use pipewire::{context::Context, main_loop::MainLoop};

use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{
            AudioBuffer, AudioCapture, AudioDataFlow, AudioDevice, AudioDeviceEnumerator,
            WaveFormat,
        },
        settings::DeskSettings,
    },
};

pub struct PipewireAudioDeviceEnumerator {}

impl PipewireAudioDeviceEnumerator {
    pub fn new() -> Self {
        Self {}
    }
}

impl AudioDeviceEnumerator for PipewireAudioDeviceEnumerator {
    fn get_device_list(&self) -> Result<Vec<AudioDevice>, DeskError> {
        let mut audio_device_list = vec![];
        let hint_iter = HintIter::new(None, &*CString::new("pcm").unwrap()).unwrap();
        for hint in hint_iter {
            log::info!("{:?}", hint);
            audio_device_list.push(AudioDevice {
                id: hint.name.unwrap_or_default(),
                firendly_name: hint.desc.unwrap_or_default(),
                data_flow: AudioDataFlow::Capture,
                default: false,
            });
        }

        Ok(audio_device_list)
    }
}

pub struct PipewireAudioCapture {
    pub pcm: PCM,
}

impl AudioCapture for PipewireAudioCapture {
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

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError> {
        todo!()
    }

    fn stop(&mut self) -> Result<(), DeskError> {
        self.pcm.drop()?;
        Ok(())
    }
}

impl PipewireAudioCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        // 1. 创建主循环（glib main loop）
        let main_loop = MainLoop::new(None)?;

        // 2. 创建 PipeWire 上下文并连接核心
        let context = Context::new(&main_loop)?;
        let core = context.connect(None)?; // `None` 表示使用默认配置

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

        Ok(())
    }
}
