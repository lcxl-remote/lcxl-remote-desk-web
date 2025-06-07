use futures::SinkExt;
use windows::Win32::{
    Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
        eConsole, eRender,
    },
    System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    },
};

use crate::desk_error::DeskError;

/// REFERENCE_TIME time units per second and per millisecond
const REFTIMES_PER_SEC: u64 = 10000000;
const REFTIMES_PER_MILLISEC: u64 = REFTIMES_PER_SEC / 1000;

pub struct AudioRecord {
    pub format: WAVEFORMATEX,
    pub audio_client: IAudioClient,
    pub audio_capture_client: IAudioCaptureClient,
    pub hns_actual_duration: u64,
}

impl AudioRecord {
    /// Create a new instance of AudioRecord. Initializes COM
    /// see https://learn.microsoft.com/zh-cn/windows/win32/coreaudio/capturing-a-stream
    pub fn new() -> Result<Self, DeskError> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };

        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let device = unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;

        let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
        let pformat = unsafe { audio_client.GetMixFormat()? };
        let format = unsafe { *pformat };

        log::info!(
            "Audio format: cbSize={}, nAvgBytesPerSec={}, nBlockAlign={}, nChannels={}, nSamplesPerSec={}, wBitsPerSample={}, wFormatTag={}",
            format.cbSize as u16,
            format.nAvgBytesPerSec as u32,
            format.nBlockAlign as u16,
            format.nChannels as u16,
            format.nSamplesPerSec as u32,
            format.wBitsPerSample as u16,
            format.wFormatTag as u16
        );

        unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                REFTIMES_PER_SEC as i64,
                0,
                pformat,
                None,
            )?
        };
        unsafe { CoTaskMemFree(Some(pformat as *mut _)) };
        let buffer_frame_count = unsafe { audio_client.GetBufferSize() }?;
        let hns_actual_duration =
            REFTIMES_PER_SEC as u64 * buffer_frame_count as u64 / format.nSamplesPerSec as u64;

        let audio_capture_client: IAudioCaptureClient = unsafe { audio_client.GetService() }?;
        Ok(AudioRecord {
            format,
            audio_client,
            audio_capture_client,
            hns_actual_duration,
        })
    }

    pub fn start(&self) -> Result<(), DeskError> {
        log::info!("Start to record audio...");
        unsafe { self.audio_client.Start() }?;
        log::info!("Audio recording started.");
        Ok(())
    }

    pub fn get_buffer_size(&self) -> Result<u32, DeskError> {
        let packet_size = unsafe { self.audio_capture_client.GetNextPacketSize() }?;
        Ok(packet_size)
    }

    pub fn get_buffer(&self) -> Result<Vec<u8>, DeskError> {
        let mut pdata: *mut u8 = std::ptr::null_mut();
        let mut numframestoread: u32 = 0;
        let mut dwflags: u32 = 0;

        unsafe {
            self.audio_capture_client.GetBuffer(
                &mut pdata,
                &mut numframestoread,
                &mut dwflags,
                None,
                None,
            )?
        };
        log::debug!(
            "dwflags: {}, buffer pointer: {:#x}, size: {}",
            dwflags,
            pdata as usize,
            numframestoread
        );
        let buffer_vec = if dwflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0
            || pdata.is_null()
            || numframestoread <= 0
        {
            Vec::<u8>::new()
        } else {
            let buffer = unsafe { std::slice::from_raw_parts(pdata, numframestoread as usize) };
            buffer.to_vec()
        };

        unsafe {
            self.audio_capture_client.ReleaseBuffer(numframestoread)?;
        }
        Ok(buffer_vec)
    }

    pub fn stop(&self) -> Result<(), DeskError> {
        log::info!("stopping audio capture client");
        unsafe {
            self.audio_client.Stop()?;
        }
        log::info!("audio capture client stopped");

        Ok(())
    }
}

impl Drop for AudioRecord {
    fn drop(&mut self) {
        unsafe {
            log::info!("dropping AudioRecord, uninitializing COM");
            CoUninitialize();
        }
    }
}

pub struct OpusAudioRecord {
    pub record: AudioRecord,
    pub encoder: opusic_c::Encoder,
    pub buffer: Vec<u8>,
}

impl OpusAudioRecord {
    pub fn new() -> Result<Self, DeskError> {
        let record = AudioRecord::new()?;
        let opus_audio_record = OpusAudioRecord {
            record,
            encoder: opusic_c::Encoder::new(
                opusic_c::Channels::Stereo,
                opusic_c::SampleRate::Hz48000,
                opusic_c::Application::Audio,
            )?,
            buffer: Vec::new(),
        };
        Ok(opus_audio_record)
    }

    pub fn start(&self) -> Result<(), DeskError> {
        self.record.start()
    }

    pub fn stop(&self) -> Result<(), DeskError> {
        self.record.stop()
    }

    pub fn get_buffer(&mut self) -> Result<Vec<u8>, DeskError> {
        let buffer = self.record.get_buffer()?;
        if !buffer.is_empty() {
            log::debug!(
                "extend buffer (size={}) to opus audio buffer (size={})",
                buffer.len(),
                self.buffer.len()
            );
            self.buffer.extend(buffer);
        }

        const SIZE_20MS: usize = opusic_c::frame_bytes_size(
            opusic_c::SampleRate::Hz48000,
            opusic_c::Channels::Stereo,
            20,
        );

        // u16 = u8*2
        if self.buffer.len() * 2 < SIZE_20MS {
            return Ok(Vec::new());
        }
        let input_buffer = Vec::<u16>::with_capacity(SIZE_20MS);

        let mut output = Vec::new();
        let len = self
            .encoder
            .encode_to_vec(input_buffer.as_slice(), &mut output)?;
        Ok(output.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Once, thread::sleep, time};

    use log::LevelFilter;

    use super::*;

    static INIT: Once = Once::new();
    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            env_logger::builder()
                .format_timestamp_micros()
                .filter_level(LevelFilter::Debug)
                .init();
        });
    }

    #[test]
    fn test_audio() -> Result<(), DeskError> {
        initialize();

        let audio_record = AudioRecord::new()?;
        audio_record.start()?;
        let dur = time::Duration::from_millis(
            (audio_record.hns_actual_duration / REFTIMES_PER_MILLISEC / 2) as u64,
        );
        log::info!("sleep for {:?}", dur);
        sleep(dur);
        let buffer = audio_record.get_buffer()?;
        log::info!("buffer len: {}", buffer.len());
        audio_record.stop()?;
        Ok(())
    }
}
