use std::ptr::null_mut;

use windows::Win32::{
    Media::{
        Audio::{
            eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM
        },
        KernelStreaming::WAVE_FORMAT_EXTENSIBLE,
        Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    },
    System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED
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
        if format.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
            let extensible_format = pformat as *mut WAVEFORMATEXTENSIBLE;
            let tmp_format = *unsafe { extensible_format.as_ref().unwrap() };
            let dw_channel_mask = tmp_format.dwChannelMask;
            let sub_format = tmp_format.SubFormat;
            let valid_bits_pre_sample = unsafe { tmp_format.Samples.wValidBitsPerSample };
            let sample_pre_block = unsafe { tmp_format.Samples.wSamplesPerBlock };
            log::info!(
                "Audio extensible format: SubFormat={:?}, dwChannelMask={}, wValidBitsPerSample={}, wSamplesPerBlock={}",
                sub_format,
                dw_channel_mask,
                valid_bits_pre_sample,
                sample_pre_block
            );
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                log::info!("Audio format is IEEE_FLOAT");
            }
        }

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

        let pcm_format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 44100,
            nAvgBytesPerSec: 44100*4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut closet_format_match: *mut WAVEFORMATEX = null_mut();
        let result = unsafe { audio_client.IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, &pcm_format, Some(&mut closet_format_match)) };
        if result.is_ok() {
            log::info!("Support pcm, closet_format_match={:?}", closet_format_match);
        } else {
            log::info!("not support pcm closet_format_match={:?}", closet_format_match);
        }
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
        let pdata_len: usize = numframestoread as usize * self.format.nBlockAlign as usize; // Calculate the length of the buffer in bytes

        let buffer_vec = if dwflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0
            || pdata.is_null()
            || numframestoread <= 0
        {
            vec![0; numframestoread as usize]
        } else {
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

#[derive(Debug)]
pub struct OpusAudioBuffer {
    pub data: Vec<u8>,            // Raw audio data
    pub origin_num_frames: usize, // Number of frames in the original
}

pub struct OpusAudioCapture {
    pub record: AudioCapture,
    //pub encoder: opusic_c::Encoder,
    pub encoder: opus::Encoder,
    pub buffer: Vec<u8>,
}
/// Workaround for Arc not being Send + Sync
/// This is only works in single thread, so it is safe to use in this case.
unsafe impl Send for OpusAudioCapture {}

unsafe impl Sync for OpusAudioCapture {}

impl OpusAudioCapture {
    pub fn new() -> Result<Self, DeskError> {
        let record = AudioCapture::new()?;
        /* 
        let encoder = opusic_c::Encoder::new(
                opusic_c::Channels::Stereo,
                opusic_c::SampleRate::Hz48000,
                opusic_c::Application::Audio,
            )?;
*/
        let encoder = opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Audio)?;
        let opus_audio_record = OpusAudioCapture {
            record,
            encoder,
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

    pub fn get_buffer(&mut self) -> Result<OpusAudioBuffer, DeskError> {
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

        /*
        const SIZE_20MS: usize = opusic_c::frame_bytes_size(
            opusic_c::SampleRate::Hz48000,
            opusic_c::Channels::Stereo,
            20,
        );
         */
        const SIZE_20MS: usize = 48000*2/1000*20;
        let mut origin_num_frames = 0; // origin_num_frames

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

                let mut output = [0; 4000];

                /*
                let len = self
                    .encoder
                    .encode_float_to_slice(input_buffer, &mut output)?;
                 */
                let len = self.encoder.encode_float(input_buffer, &mut output)?;
                log::debug!("encode_float_to_slice len={}", len);
                output[..len].to_vec()
            } else if self.record.format.wBitsPerSample == 16 {
                let input_buffer = unsafe {
                    core::slice::from_raw_parts(self.buffer.as_ptr() as *const i16, SIZE_20MS)
                };

                let mut output = [0; 4000];
                //let len = self.encoder.encode_to_vec(input_buffer, &mut output)?;
                let len = self.encoder.encode(input_buffer, &mut output)?;
                log::debug!("encode_to_vec len={}", len);

                output[..len].to_vec()
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
            origin_num_frames += removed.len() / self.record.format.nBlockAlign as usize;
        }
        Ok(OpusAudioBuffer {
            data: encoded_buffer,
            origin_num_frames,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs::File, sync::Once, thread::sleep, time};

    use bytes::Bytes;
    use log::LevelFilter;

    use webrtc::{media::io::ogg_writer::OggWriter, rtp};
    use webrtc_media::io::Writer;

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

        // initialize ogg writer
        let tmp_dir = env::temp_dir();
        let tmp_dir = tmp_dir.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(tmp_dir.as_path())?;
        let name = tmp_dir.join(format!("record.ogg"));

        let mut ogg_write = OggWriter::new(File::create(name.as_path())?, 48000, 2)?;
        let mut current_timesamp = 0usize;

        let mut opus_audio_record = OpusAudioCapture::new()?;
        opus_audio_record.start()?;
        let dur = time::Duration::from_millis(
            (opus_audio_record.record.hns_actual_duration / REFTIMES_PER_MILLISEC / 2) as u64,
        );
        for i in 0..10 {
            sleep(dur);
            let buffer = opus_audio_record.get_buffer()?;
            log::info!(
                "buffer {} len: {}, origin_num_frames: {}",
                i,
                buffer.data.len(),
                buffer.origin_num_frames
            );
            let buffer_bytes = Bytes::from(buffer.data);

            let mut pkt = rtp::packet::Packet {
                header: rtp::header::Header::default(),
                payload: buffer_bytes,
            };
            current_timesamp += buffer.origin_num_frames;
            pkt.header.timestamp = current_timesamp as u32;
            log::info!("current_timesamp: {}", pkt.header.timestamp);

            ogg_write.write_rtp(&pkt)?;
        }

        // stop recording and write the final packet to the file
        opus_audio_record.stop()?;
        ogg_write.close()?;
        log::info!("Written audio data to {}", name.to_string_lossy());
        Ok(())
    }
}
