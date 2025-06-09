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

use crate::{desk_error::DeskError, model::common::ErrorCode};

/// REFERENCE_TIME time units per second and per millisecond
const REFTIMES_PER_SEC: u64 = 10000000;
pub const REFTIMES_PER_MILLISEC: u64 = REFTIMES_PER_SEC / 1000;

pub struct AudioCapture {
    pub format: WAVEFORMATEX,
    pub audio_client: IAudioClient,
    pub audio_capture_client: IAudioCaptureClient,
    pub hns_actual_duration: u64,
}

impl AudioCapture {
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
        Ok(AudioCapture {
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
            "dwflags: {}, buffer pointer: {:#x}, frame number: {}",
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
            let pdata_len: usize = numframestoread as usize
                * self.format.wBitsPerSample as usize
                * self.format.nChannels as usize
                / 8; // Calculate the length of the buffer in bytes

            let buffer = unsafe { std::slice::from_raw_parts(pdata, pdata_len) };
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

impl Drop for AudioCapture {
    fn drop(&mut self) {
        unsafe {
            log::info!("dropping AudioRecord, uninitializing COM");
            CoUninitialize();
        }
    }
}

pub struct OpusAudioCapture {
    pub record: AudioCapture,
    pub encoder: opusic_c::Encoder,
    pub buffer: Vec<u8>,
}
/// Workaround for Arc not being Send + Sync
/// This is only works in single thread, so it is safe to use in this case.
unsafe impl Send for OpusAudioCapture {}

unsafe impl Sync for OpusAudioCapture {}

impl OpusAudioCapture {
    pub fn new() -> Result<Self, DeskError> {
        let record = AudioCapture::new()?;
        let opus_audio_record = OpusAudioCapture {
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
        loop {
            let buffer = self.record.get_buffer()?;
            if buffer.is_empty() {
                break;
            }
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

        let mut encoded_buffer = Vec::<u8>::new();
        // u32 = u8*4
        loop {
            let frame_20ms_byte_len = SIZE_20MS * self.record.format.wBitsPerSample as usize / 8;
            if self.buffer.len() < frame_20ms_byte_len {
                break;
            }
            let mut result = if self.record.format.wBitsPerSample == 32 {
                let input_buffer = unsafe {
                    core::slice::from_raw_parts(self.buffer.as_ptr() as *const f32, SIZE_20MS)
                };

                let mut output = Vec::with_capacity(SIZE_20MS * 4);
                let len = self
                    .encoder
                    .encode_float_to_vec(input_buffer, &mut output)?;
                log::debug!("encode_float_to_vec len={}", len);
                output.to_vec()
            } else if self.record.format.wBitsPerSample == 16 {
                let input_buffer = unsafe {
                    core::slice::from_raw_parts(self.buffer.as_ptr() as *const u16, SIZE_20MS)
                };

                let mut output = Vec::new();
                let len = self.encoder.encode_to_vec(input_buffer, &mut output)?;
                log::debug!("encode_to_vec len={}", len);

                output.to_vec()
            } else {
                return DeskError::custom_error(
                    ErrorCode::SYSTEM_ERROR,
                    format!(
                        "Unsupport bit per sample: {}",
                        self.record.format.wBitsPerSample as u16
                    ),
                );
            };
            encoded_buffer.append(&mut result);
            let removed: Vec<u8> = self.buffer.drain(0..frame_20ms_byte_len).collect();
            // let current_buffer_len = self.buffer.len();
            log::debug!("removed {} bytes from buffer", removed.len());
        }
        Ok(encoded_buffer)
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

        let audio_record = AudioCapture::new()?;
        audio_record.start()?;
        let dur = time::Duration::from_millis(
            (audio_record.hns_actual_duration / REFTIMES_PER_MILLISEC / 2) as u64,
        );
        log::info!("sleep for {:?}", dur);
        sleep(dur);
        let buffer = audio_record.get_buffer()?;
        log::info!("buffer1 len: {}", buffer.len());

        sleep(dur);
        let buffer = audio_record.get_buffer()?;
        log::info!("buffer2 len: {}", buffer.len());

        sleep(dur);
        let buffer = audio_record.get_buffer()?;
        log::info!("buffer3 len: {}", buffer.len());
        audio_record.stop()?;
        Ok(())
    }

    #[test]
    fn test_opus_audio() -> Result<(), DeskError> {
        initialize();

        let mut opus_audio_record = OpusAudioCapture::new()?;
        opus_audio_record.start()?;
        let dur = time::Duration::from_millis(
            (opus_audio_record.record.hns_actual_duration / REFTIMES_PER_MILLISEC / 2) as u64,
        );
        sleep(dur);
        let buffer = opus_audio_record.get_buffer()?;
        log::info!("buffer1 len: {}", buffer.len());

        sleep(dur);
        let buffer = opus_audio_record.get_buffer()?;
        log::info!("buffer2 len: {}", buffer.len());
        opus_audio_record.stop()?;
        Ok(())
    }
}
