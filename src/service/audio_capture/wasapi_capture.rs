use std::ptr::null_mut;

use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{
            AudioBuffer, AudioCapture, AudioDataFlow, AudioDevice, SelectedAudioDevice, WaveFormat,
        },
        common::ErrorCode,
        settings::DeskSettings,
    },
};
use windows::Win32::{
    Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
    Media::{
        Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient,
            IMMDevice, IMMDeviceEnumerator, IMMEndpoint, MMDeviceEnumerator, WAVE_FORMAT_PCM,
            WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eAll, eCapture, eConsole,
            eRender,
        },
        KernelStreaming::WAVE_FORMAT_EXTENSIBLE,
        Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    },
    System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    },
};
use windows_core::{GUID, Interface};

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

pub struct WasapiAudioCapture {
    pub format: WAVEFORMATEXTENSIBLE,
    pub audio_client: IAudioClient,
    pub audio_capture_client: IAudioCaptureClient,
    pub hns_actual_duration: u64,
    /// Indicates if the audio capture has started
    /// This is used to prevent starting the capture multiple times
    pub started: bool,
}

/// FIXME Workaround for Box not being Send + Sync
/// This is only works in single thread, so it is safe to use in this case.
unsafe impl Send for WasapiAudioCapture {}

unsafe impl Sync for WasapiAudioCapture {}

#[derive(Debug)]
pub struct WasapiAudioBuffer {
    pub buffer: Vec<u8>,   // Raw audio data
    pub num_frames: usize, // Number of frames
}

impl AudioBuffer for WasapiAudioBuffer {
    fn get_buffer_slice(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    fn get_num_frames(&self) -> usize {
        self.num_frames
    }
}

impl From<WAVEFORMATEX> for WaveFormat {
    fn from(format: WAVEFORMATEX) -> Self {
        WaveFormat {
            format_tag: format.wFormatTag,
            channels: format.nChannels,
            samples_per_sec: format.nSamplesPerSec,
            avg_bytes_per_sec: format.nAvgBytesPerSec,
            block_align: format.nBlockAlign,
            bits_per_sample: format.wBitsPerSample,
        }
    }
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

impl AudioCapture for WasapiAudioCapture {
    fn get_devices_list(&self) -> Result<Vec<AudioDevice>, DeskError> {
        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

        // get default audio endpoint for render and capture
        let result = unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole) };
        let default_render_device_id = match result {
            Ok(device) => {
                let device_id = WasapiAudioCapture::get_device_id(&device)?;
                log::info!("Default render audio endpoint: {}", device_id);
                Some(device_id)
            }
            Err(error) => {
                log::warn!("Failed to get default render audio endpoint: {}", error);
                None
            }
        };

        let result = unsafe { device_enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) };
        let default_capture_device_id = match result {
            Ok(device) => {
                let device_id = WasapiAudioCapture::get_device_id(&device)?;
                log::info!("Default capture audio endpoint: {:?}", device_id);
                Some(device_id)
            }
            Err(error) => {
                log::warn!("Failed to get default capture audio endpoint: {}", error);
                None
            }
        };
        let collection =
            unsafe { device_enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        let mut devices = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = unsafe { collection.Item(i) }?;
            let device_id = Self::get_device_id(&device)?;
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
            log::info!(
                "index: {}, device_id: {}, firendly_name: {}, data flow: {:?}",
                i,
                device_id,
                firendly_name,
                data_flow
            );
            let audio_data_flow;
            let default_device;

            if data_flow == eCapture {
                audio_data_flow = AudioDataFlow::Capture;
                default_device = default_capture_device_id
                    .as_ref()
                    .is_some_and(|d| *d == device_id);
            } else if data_flow == eRender {
                audio_data_flow = AudioDataFlow::Render;
                default_device = default_render_device_id
                    .as_ref()
                    .is_some_and(|d| *d == device_id);
            } else {
                panic!("Should not be happend")
            }
            devices.push(AudioDevice {
                id: device_id,
                firendly_name: firendly_name,
                data_flow: audio_data_flow,
                default: default_device,
            });
        }
        Ok(devices)
    }

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError> {
        let mut buffer = vec![];
        let mut num_frames: usize = 0;
        loop {
            let result = self.get_one_buffer();
            if let Err(error) = result {
                if let DeskError::WindowsResultError(ref _backtrace, ref windows_error) = error {
                    if windows_error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                        log::warn!("audio device is invalidated");
                        return DeskError::custom_error(
                            ErrorCode::ACTION_NEED_RETRY,
                            "Audio device is invalidated, please retry".to_string(),
                        );
                    }
                }
                return Err(error);
            }
            let one_buffer = result?;
            if one_buffer.buffer.is_empty() {
                break;
            }
            let one_buffer_len = one_buffer.buffer.len();

            buffer.extend(one_buffer.buffer);
            num_frames += one_buffer.num_frames;

            log::trace!(
                "add {} frames, buffer size: {} bytes, total {} frames, total buffer size: {} bytes",
                one_buffer.num_frames,
                one_buffer_len,
                num_frames,
                buffer.len(),
            );
        }
        Ok(Box::new(WasapiAudioBuffer { buffer, num_frames }))
    }

    fn start(&mut self) -> Result<WaveFormat, DeskError> {
        if self.started {
            log::warn!("Audio capture has already started, ignoring start request.");
            return Ok(self.format.Format.into());
        }
        log::info!("Start to record audio...");
        unsafe { self.audio_client.Start() }?;
        log::info!("Audio recording started.");
        self.started = true;
        Ok(self.format.Format.into())
    }

    fn stop(&mut self) -> Result<(), DeskError> {
        if !self.started {
            log::warn!("Audio capture has not started, ignoring stop request.");
            return Ok(());
        }
        log::info!("stopping wasapi audio capture client");
        unsafe {
            self.audio_client.Stop()?;
        }
        log::info!("Wasapi audio capture client stopped");
        self.started = false;
        Ok(())
    }
}

impl Drop for WasapiAudioCapture {
    fn drop(&mut self) {
        log::info!("Dropping WasapiAudioCapture, stopping audio client");
        if self.started {
            let _ = self.stop();
        }
        log::info!("WasapiAudioCapture dropped");
        // Uninitialize COM
        let _ = destroy_thread();
    }
}

impl WasapiAudioCapture {
    pub fn get_device_id(device: &IMMDevice) -> Result<String, DeskError> {
        let device_id_ptr = unsafe { device.GetId() }?;
        let device_id = unsafe { device_id_ptr.to_string() }?;
        unsafe { CoTaskMemFree(Some(device_id_ptr.as_ptr() as *const _)) };
        return Ok(device_id);
    }

    /// Create a new instance of AudioRecord. Initializes COM
    /// see https://learn.microsoft.com/zh-cn/windows/win32/coreaudio/capturing-a-stream
    #[allow(unreachable_patterns)]
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        init_thread()?;

        let audio_device = if let Some(ref device) = desk_settings.audio_device {
            device.clone()
        } else {
            SelectedAudioDevice::default()
        };
        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let dataflow = match audio_device.audio_data_flow {
            AudioDataFlow::Render => eRender,
            AudioDataFlow::Capture => eCapture,
            _ => {
                return DeskError::custom_error(
                    ErrorCode::SYSTEM_ERROR,
                    format!(
                        "Unknown audio data flow: {:?}",
                        audio_device.audio_data_flow
                    ),
                );
            }
        };
        let device = unsafe {
            if let Some(device_id) = audio_device.audio_device_id {
                let raw_device_id = format!("{}\0", device_id)
                    .encode_utf16()
                    .collect::<Vec<u16>>();
                device_enumerator
                    .GetDevice(windows::core::PCWSTR::from_raw(raw_device_id.as_ptr()))?
            } else {
                device_enumerator.GetDefaultAudioEndpoint(dataflow, eConsole)?
            }
        };

        let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
        let p_mix_format = unsafe { audio_client.GetMixFormat()? };
        let pformat = p_mix_format;
        let format = WasapiAudioCapture::get_wave_format_tensible(p_mix_format);
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
            let format = WasapiAudioCapture::get_wave_format_tensible(closet_format);
            log_wave_format(&format);
        }
        // see https://learn.microsoft.com/zh-cn/windows/win32/coreaudio/loopback-recording
        let streamflags = if dataflow == eRender {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            0
        };
        unsafe { CoTaskMemFree(Some(closet_format_match as *mut _)) };
        unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                streamflags,
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
        Ok(WasapiAudioCapture {
            format,
            audio_client,
            audio_capture_client,
            hns_actual_duration,
            started: false,
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

    pub fn get_buffer_size(&self) -> Result<u32, DeskError> {
        let packet_size = unsafe { self.audio_capture_client.GetNextPacketSize() }?;
        Ok(packet_size)
    }

    pub fn get_one_buffer(&self) -> Result<WasapiAudioBuffer, DeskError> {
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
        log::trace!(
            "dwflags: {}, buffer pointer: {:#x}, frame number: {}",
            dwflags,
            pdata as usize,
            numframestoread
        );

        let pdata_len: usize = numframestoread as usize * self.format.Format.nBlockAlign as usize; // Calculate the length of the buffer in bytes

        let buffer_vec = if pdata.is_null() || numframestoread <= 0 {
            vec![]
        } else {
            if dwflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                vec![0; pdata_len]
            } else {
                let buffer = unsafe { std::slice::from_raw_parts(pdata, pdata_len) };
                buffer.to_vec()
            }
        };

        unsafe {
            self.audio_capture_client.ReleaseBuffer(numframestoread)?;
        }
        Ok(WasapiAudioBuffer {
            buffer: buffer_vec,
            num_frames: numframestoread as usize,
        })
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
            init_thread().unwrap();
        });
    }

    #[test]
    fn test_device_info() -> Result<(), DeskError> {
        initialize();
        let desk_settings = DeskSettings::default();
        let capture = WasapiAudioCapture::new(&desk_settings)?;
        let devices = capture.get_devices_list()?;
        log::debug!("all devices: {:?}", devices);
        Ok(())
    }

    #[test]
    fn test_write_wav() -> Result<(), DeskError> {
        initialize();
        let desk_settings = DeskSettings::default();
        let mut audio_record = WasapiAudioCapture::new(&desk_settings)?;
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
            let buffer = audio_record.get_buffer()?;
            let slice = buffer.get_f32_buffer_slice();
            log::info!("buffer{} len: {}", i, slice.len());
            for sample in slice {
                writer.write_sample(*sample).unwrap();
            }
        }
        audio_record.stop()?;
        Ok(())
    }
}
