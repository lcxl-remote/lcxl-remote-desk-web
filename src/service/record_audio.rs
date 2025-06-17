use std::ptr::null_mut;

use crate::{desk_error::DeskError, model::common::ErrorCode};
use windows::Win32::{
    Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
    Media::{
        Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            DEVICE_STATE_ACTIVE, EDataFlow, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
            IMMEndpoint, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
            WAVEFORMATEXTENSIBLE_0, eCapture, eConsole, eRender,
        },
        KernelStreaming::WAVE_FORMAT_EXTENSIBLE,
        Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    },
    System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    },
};
use windows_core::GUID;
use windows_core::Interface;

/// REFERENCE_TIME time units per second and per millisecond
const REFTIMES_PER_SEC: u64 = 10000000;
pub const REFTIMES_PER_MILLISEC: u64 = REFTIMES_PER_SEC / 1000;

pub fn init_thread() -> Result<(), DeskError> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    Ok(())
}

pub fn destroy_thread() -> Result<(), DeskError> {
    log::info!("dropping thread, uninitializing COM");
    unsafe { CoUninitialize() };
    Ok(())
}

pub struct AudioCapture {
    pub format: WAVEFORMATEXTENSIBLE,
    pub audio_client: IAudioClient,
    pub audio_capture_client: IAudioCaptureClient,
    pub hns_actual_duration: u64,
}

#[derive(Debug)]
pub struct AudioBuffer<T> {
    pub buffer: Vec<T>,    // Raw audio data
    pub num_frames: usize, // Number of frames
}

fn log_wave_format(format: &WAVEFORMATEXTENSIBLE) {
    let mut log_str = format!(
        "Audio format: cbSize={}, nAvgBytesPerSec={}, nBlockAlign={}, nChannels={}, nSamplesPerSec={}, wBitsPerSample={}, wFormatTag={}",
        format.Format.cbSize as u16,
        format.Format.nAvgBytesPerSec as u32,
        format.Format.nBlockAlign as u16,
        format.Format.nChannels as u16,
        format.Format.nSamplesPerSec as u32,
        format.Format.wBitsPerSample as u16,
        format.Format.wFormatTag as u16
    );
    if format.Format.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
        let dw_channel_mask = format.dwChannelMask;
        let sub_format = format.SubFormat;
        let valid_bits_pre_sample = unsafe { format.Samples.wValidBitsPerSample };
        let sample_pre_block = unsafe { format.Samples.wSamplesPerBlock };
        log_str+= format!(
                "\nAudio extensible format: SubFormat={:?}, dwChannelMask={}, wValidBitsPerSample={}, wSamplesPerBlock={}",
                sub_format,
                dw_channel_mask,
                valid_bits_pre_sample,
                sample_pre_block
            ).as_str();
        if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            log_str += "\nAudio format is IEEE_FLOAT";
        }
    }
    log::info!("{}", log_str);
}

#[derive(Debug, Clone)]
pub enum AudioDataFlow {
    Render,
    Capture,
}

#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub id: String,
    pub firendly_name: String,
    pub data_flow: AudioDataFlow,
}

impl AudioCapture {
    pub fn enum_devices(dataflow: EDataFlow) -> Result<Vec<AudioDevice>, DeskError> {
        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let collection =
            unsafe { device_enumerator.EnumAudioEndpoints(dataflow, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        let mut devices = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = unsafe { collection.Item(i) }?;

            let device_id_ptr = unsafe { device.GetId() }?;
            let device_id = unsafe { device_id_ptr.to_string() }?;
            unsafe { CoTaskMemFree(Some(device_id_ptr.as_ptr() as *const _)) };
            let prop_store = unsafe { device.OpenPropertyStore(STGM_READ) }?;

            let prop_var = unsafe { prop_store.GetValue(&PKEY_Device_FriendlyName) }?;
            let firendly_name = if !prop_var.is_empty() {
                let firendly_name_ptr = unsafe { prop_var.Anonymous.Anonymous.Anonymous.pwszVal };
                unsafe { firendly_name_ptr.to_string()? }
            } else {
                "".to_string()
            };
            let endpoint = device.cast::<IMMEndpoint>()?;
            let data_flow = unsafe { endpoint.GetDataFlow() }?;
            println!(
                "index: {}, device_id: {}, firendly_name: {}, data flow: {:?}",
                i, device_id, firendly_name, data_flow
            );
            let audio_data_flow = if data_flow == eCapture {
                AudioDataFlow::Capture
            } else if data_flow == eRender {
                AudioDataFlow::Render
            } else {
                panic!("Should not be happend")
            };
            devices.push(AudioDevice {
                id: device_id,
                firendly_name: firendly_name,
                data_flow: audio_data_flow,
            });
        }
        Ok(devices)
    }

    /// Create a new instance of AudioRecord. Initializes COM
    /// see https://learn.microsoft.com/zh-cn/windows/win32/coreaudio/capturing-a-stream
    pub fn new() -> Result<Self, DeskError> {
        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let device = unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;

        let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
        let p_mix_format = unsafe { audio_client.GetMixFormat()? };
        let pformat = p_mix_format;
        let format = AudioCapture::get_wave_format_tensible(p_mix_format);
        log_wave_format(&format);

        let pcm_format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut closet_format_match: *mut WAVEFORMATEX = null_mut();
        let result = unsafe {
            audio_client.IsFormatSupported(
                AUDCLNT_SHAREMODE_SHARED,
                &pcm_format,
                Some(&mut closet_format_match),
            )
        };
        if result.is_ok() {
            log::info!("Support pcm!");
            //pformat = &mut pcm_format;
        } else {
            log::info!("not support pcm");
        }
        if let Some(closet_format) = unsafe { closet_format_match.as_mut() } {
            let format = AudioCapture::get_wave_format_tensible(closet_format);
            log_wave_format(&format);
        }

        unsafe { CoTaskMemFree(Some(closet_format_match as *mut _)) };
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
        unsafe { CoTaskMemFree(Some(p_mix_format as *mut _)) };
        let buffer_frame_count = unsafe { audio_client.GetBufferSize() }?;

        let hns_actual_duration = REFTIMES_PER_SEC as u64 * buffer_frame_count as u64
            / format.Format.nSamplesPerSec as u64;

        let audio_capture_client: IAudioCaptureClient = unsafe { audio_client.GetService() }?;
        Ok(AudioCapture {
            format,
            audio_client,
            audio_capture_client,
            hns_actual_duration,
        })
    }

    fn get_wave_format_tensible(format: *mut WAVEFORMATEX) -> WAVEFORMATEXTENSIBLE {
        let tmp_format = unsafe { *format };
        if tmp_format.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
            let p_wave_format_extensible = format as *mut WAVEFORMATEXTENSIBLE;
            return unsafe { *p_wave_format_extensible };
        }
        WAVEFORMATEXTENSIBLE {
            Format: tmp_format,
            Samples: WAVEFORMATEXTENSIBLE_0 { wReserved: 0 },
            dwChannelMask: 0,
            SubFormat: GUID::default(),
        }
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

    pub fn get_one_buffer<T>(&self) -> Result<AudioBuffer<T>, DeskError>
    where
        T: std::clone::Clone + Default,
    {
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

        let p_data_with_type = pdata as *mut T; // Cast the pointer to T type
        let pdata_len: usize =
            numframestoread as usize * self.format.Format.nBlockAlign as usize / size_of::<T>(); // Calculate the length of the buffer in bytes

        let buffer_vec = if p_data_with_type.is_null() || numframestoread <= 0 {
            vec![]
        } else {
            if dwflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                vec![T::default(); pdata_len]
            } else {
                let buffer = unsafe { std::slice::from_raw_parts(p_data_with_type, pdata_len) };
                buffer.to_vec()
            }
        };

        unsafe {
            self.audio_capture_client.ReleaseBuffer(numframestoread)?;
        }
        Ok(AudioBuffer {
            buffer: buffer_vec,
            num_frames: numframestoread as usize,
        })
    }

    pub fn get_buffer<T>(&self) -> Result<AudioBuffer<T>, DeskError>
    where
        T: std::clone::Clone + Default,
    {
        if size_of::<T>() != self.format.Format.wBitsPerSample as usize / 8 {
            panic!("Data type size mismatch with audio format");
        }
        let mut buffer = vec![];
        let mut num_frames: usize = 0;
        loop {
            let one_buffer = self.get_one_buffer::<T>()?;
            if one_buffer.buffer.is_empty() {
                break;
            }
            let one_buffer_len = one_buffer.buffer.len();

            buffer.extend(one_buffer.buffer);
            num_frames += one_buffer.num_frames;

            log::debug!(
                "add {} frames, buffer size: {} bytes, total {} frames, total buffer size: {} bytes",
                one_buffer.num_frames,
                one_buffer_len,
                num_frames,
                buffer.len(),
            );
        }
        Ok(AudioBuffer { buffer, num_frames })
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
        const SIZE_20MS: usize = 48000 * 2 / 1000 * 20;
        let frame_20ms_byte_len = SIZE_20MS * self.record.format.Format.wBitsPerSample as usize / 8;
        // the internal buffer is not enough to hold a 20ms frame, read more data from the audio device
        if self.buffer.len() < frame_20ms_byte_len {
            if self.record.format.Format.wBitsPerSample == 32 {
                let float_buffer = self.record.get_buffer::<f32>()?;
                let buffer = unsafe {
                    core::slice::from_raw_parts(
                        float_buffer.buffer.as_ptr() as *const u8,
                        float_buffer.buffer.len() * 4,
                    )
                };
                if !buffer.is_empty() {
                    log::debug!(
                        "extend buffer (size={}) to opus audio buffer (size={})",
                        buffer.len(),
                        self.buffer.len()
                    );
                    self.buffer.extend(buffer);
                }
            } else if self.record.format.Format.wBitsPerSample == 16 {
                let i16_buffer = self.record.get_buffer::<i16>()?;
                let buffer = unsafe {
                    core::slice::from_raw_parts(
                        i16_buffer.buffer.as_ptr() as *const u8,
                        i16_buffer.buffer.len() * 2,
                    )
                };
                if !buffer.is_empty() {
                    log::debug!(
                        "extend buffer (size={}) to opus audio buffer (size={})",
                        buffer.len(),
                        self.buffer.len()
                    );
                    self.buffer.extend(buffer);
                }
            } else {
                return DeskError::custom_error(
                    ErrorCode::SYSTEM_ERROR,
                    format!(
                        "Unsupport bit per sample: {}",
                        self.record.format.Format.wBitsPerSample as u16
                    ),
                );
            };
        }

        /*
        const SIZE_20MS: usize = opusic_c::frame_bytes_size(
            opusic_c::SampleRate::Hz48000,
            opusic_c::Channels::Stereo,
            20,
        );
         */

        let origin_num_frames = 0; // origin_num_frames

        let encoded_buffer = Vec::<u8>::new();

        if self.buffer.len() < frame_20ms_byte_len {
            return Ok(OpusAudioBuffer {
                data: encoded_buffer,
                origin_num_frames,
            });
        }
        // u32 = u8*4

        let encoded_buffer = if self.record.format.Format.wBitsPerSample == 32 {
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
        } else if self.record.format.Format.wBitsPerSample == 16 {
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
                    self.record.format.Format.wBitsPerSample as u16
                ),
            );
        };

        let removed: Vec<u8> = self.buffer.drain(0..frame_20ms_byte_len).collect();
        // let current_buffer_len = self.buffer.len();
        log::debug!("removed {} bytes from buffer", removed.len());
        let origin_num_frames = removed.len() / self.record.format.Format.nBlockAlign as usize;

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
    use windows::Win32::Media::Audio::{eAll, eCapture};

    use super::*;

    static INIT: Once = Once::new();
    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            env_logger::builder()
                .format_timestamp_micros()
                .filter_level(LevelFilter::Debug)
                .init();
            init_thread().unwrap();
        });
    }

    #[test]
    fn test_device_info() -> Result<(), DeskError> {
        initialize();

        let devices = AudioCapture::enum_devices(eAll)?;
        log::debug!("all devices: {:?}", devices);

        let devices = AudioCapture::enum_devices(eCapture)?;
        log::debug!("capture devices: {:?}", devices);
        let devices = AudioCapture::enum_devices(eRender)?;
        log::debug!("rennder devices: {:?}", devices);
        Ok(())
    }

    #[test]
    fn test_write_wav() -> Result<(), DeskError> {
        initialize();

        let audio_record = AudioCapture::new()?;
        audio_record.start()?;

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create("sample/sine.wav", spec).unwrap();
        let dur = time::Duration::from_millis(
            (audio_record.hns_actual_duration / REFTIMES_PER_MILLISEC / 2) as u64,
        );
        log::info!("sleep for {:?} every time", dur);
        for i in 0..30 {
            sleep(dur);
            let buffer = audio_record.get_buffer::<f32>()?;
            log::info!("buffer{} len: {}", i, buffer.buffer.len());
            for sample in buffer.buffer {
                writer.write_sample(sample).unwrap();
            }
        }
        audio_record.stop()?;
        Ok(())
    }

    #[test]
    fn test_opus_decode() -> Result<(), DeskError> {
        initialize();
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
            // decode again
            let mut decoder = opus::Decoder::new(48000, opus::Channels::Stereo)?;
            let mut decoded_buffer = vec![0f32; buffer.origin_num_frames * 2];

            let sample_size =
                decoder.decode_float(&buffer.data, decoded_buffer.as_mut_slice(), true)?;
            log::info!("decoded sample size: {}", sample_size,);
        }

        // stop recording and write the final packet to the file
        opus_audio_record.stop()?;

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
