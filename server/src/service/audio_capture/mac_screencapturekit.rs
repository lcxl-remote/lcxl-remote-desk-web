use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::{
    error::DeskError,
    model::audio_capture::{AudioBuffer, AudioCapture, AudioDeviceEnumerator, WaveFormat},
};
use desk_signal_facade::model::{
    audio_capture::{AudioDataFlow, AudioDevice},
    desk_settings::DeskSettings,
};
use desk_utils::error::DeskErrorCode;
use screencapturekit::{
    cm_sample_buffer::CMSampleBuffer,
    sc_content_filter::{InitParams, SCContentFilter},
    sc_output_handler::{SCStreamOutputType, StreamOutput},
    sc_shareable_content::SCShareableContent,
    sc_stream::SCStream,
    sc_stream_configuration::SCStreamConfiguration,
};

pub struct MacScreencaptureKitAudioCapture {
    stream: Option<SCStream>,
    buffer: Arc<Mutex<VecDeque<u8>>>,
    started: bool,
    format: WaveFormat,
}

pub struct MacScreencaptureKitAudioBuffer {
    buffer: Vec<u8>,
    num_frames: usize,
}

impl AudioBuffer for MacScreencaptureKitAudioBuffer {
    fn get_buffer_slice(&self) -> &[u8] {
        &self.buffer
    }

    fn get_num_frames(&self) -> usize {
        self.num_frames
    }
}

struct AudioReceiver {
    buffer: Arc<Mutex<VecDeque<u8>>>,
}

impl StreamOutput for AudioReceiver {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if let SCStreamOutputType::Audio = of_type {
            let buffers = sample.sys_ref.get_av_audio_buffer_list();
            let mut guard = self.buffer.lock().unwrap();
            for buf in buffers {
                guard.extend(buf.data);
            }
        }
    }
}

impl MacScreencaptureKitAudioCapture {
    pub fn new(_settings: &DeskSettings) -> Result<Self, DeskError> {
        // Hardcoded format for now or derive from system default logic later
        // ScreenCaptureKit typically captures at system rate (44.1k or 48k) in Float32
        let format = WaveFormat {
            format_tag: 3, // IEEE_FLOAT
            channels: 2,
            samples_per_sec: 48000,
            avg_bytes_per_sec: 48000 * 4 * 2,
            block_align: 8,
            bits_per_sample: 32,
        };

        Ok(Self {
            stream: None,
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            started: false,
            format,
        })
    }
}

impl AudioCapture for MacScreencaptureKitAudioCapture {
    fn start(&mut self) -> Result<WaveFormat, DeskError> {
        if self.started {
            return Ok(self.format);
        }

        // Initialize SCStream with audio capture enabled
        let content = SCShareableContent::try_current().map_err(|e| {
            DeskError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, e.as_str())
        })?;
        let display = content.displays.first().ok_or(DeskError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "No display found",
        ))?;

        let filter = SCContentFilter::new(InitParams::Display(display.clone()));

        let config = SCStreamConfiguration {
            width: 100,
            height: 100,
            captures_audio: true,
            excludes_current_process_audio: false,
            ..Default::default()
        };

        let receiver = AudioReceiver {
            buffer: self.buffer.clone(),
        };

        let mut stream = SCStream::new(filter, config, ErrorHandler);
        stream.add_output(receiver, SCStreamOutputType::Audio);

        stream
            .start_capture()
            .map_err(|e| DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, e.as_str()))?;

        self.stream = Some(stream);
        self.started = true;

        Ok(self.format)
    }

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError> {
        let mut queue = self.buffer.lock().unwrap();
        let len = queue.len();
        let mut vec = Vec::with_capacity(len);

        // Drain everything available
        vec.extend(queue.drain(..));

        let num_frames = vec.len()
            / (self.format.channels as usize * (self.format.bits_per_sample as usize / 8));

        Ok(Box::new(MacScreencaptureKitAudioBuffer {
            buffer: vec,
            num_frames,
        }))
    }

    fn stop(&mut self) -> Result<(), DeskError> {
        if let Some(stream) = &self.stream {
            stream.stop_capture().map_err(|e| {
                DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, e.as_str())
            })?;
        }
        self.stream = None;
        self.started = false;
        Ok(())
    }
}

struct ErrorHandler;
impl screencapturekit::sc_error_handler::StreamErrorHandler for ErrorHandler {
    fn on_error(&self) {
        log::error!("Audio SCStream error");
    }
}

pub struct MacScreencaptureKitAudioDeviceEnumerator;

impl MacScreencaptureKitAudioDeviceEnumerator {
    pub fn new() -> Self {
        Self
    }
}

impl AudioDeviceEnumerator for MacScreencaptureKitAudioDeviceEnumerator {
    fn get_device_list(&self) -> Result<Vec<AudioDevice>, DeskError> {
        // ScreenCaptureKit captures "System Audio", it doesn't really enumerate input devices like WASAPI.
        // We can just return a single "System Audio" device.
        Ok(vec![AudioDevice {
            id: "system_audio".to_string(),
            firendly_name: "System Audio".to_string(),
            data_flow: AudioDataFlow::Capture,
            default: true,
        }])
    }
}
