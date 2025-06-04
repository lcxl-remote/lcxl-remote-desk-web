use std::{backtrace::Backtrace, sync::Arc};

use crate::{
    desk_error::DeskError,
    model::{common::ErrorCode, record_screen::DisplayInfo, settings::Settings},
};
use log::warn;
use openh264::{
    OpenH264API,
    encoder::{BitRate, IntraFramePeriod},
};
use std::fmt::Debug;
use windows::Win32::{
    Foundation::{GENERIC_ALL, HMODULE},
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_REFERENCE,
            D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
        },
        Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
            ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        },
        Dxgi::{
            Common::DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_MAP_READ,
            DXGI_MAPPED_RECT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
            DXGI_RESOURCE_PRIORITY_MAXIMUM, IDXGIAdapter, IDXGIDevice, IDXGIOutput1,
            IDXGIOutputDuplication, IDXGIResource, IDXGISurface,
        },
    },
    System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop,
        SetThreadDesktop,
    },
};
use windows_core::Interface;
use yuv::{
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
    bgra_to_yuv420,
};
pub struct ScreenRecordManager {
    pub device: ID3D11Device,
    pub device_context: ID3D11DeviceContext,
    pub dxgi_adapter: IDXGIAdapter,
}

impl ScreenRecordManager {
    pub fn set_thread_desktop() -> Result<(), DeskError> {
        unsafe {
            let current_deskop = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )?;
            SetThreadDesktop(current_deskop)?;
            let result = CloseDesktop(current_deskop);
            if let Err(err) = result {
                log::warn!("Failed to close desktop, ignore, error: {:?}", err);
            }
        };
        Ok(())
    }

    pub fn new(settings: &Settings) -> Result<Arc<Self>, DeskError> {
        // get desktop
        Self::set_thread_desktop()?;

        // init dxgi factory
        let driver_types: [D3D_DRIVER_TYPE; 3] = [
            D3D_DRIVER_TYPE_HARDWARE,
            D3D_DRIVER_TYPE_WARP,
            D3D_DRIVER_TYPE_REFERENCE,
        ];
        let feature_levels: [D3D_FEATURE_LEVEL; 4] = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_9_1,
        ];
        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        if settings.desk.enable_d3d_debug {
            log::info!("Enable d3d debug flag");
            flags |= D3D11_CREATE_DEVICE_DEBUG;
        }

        let mut device = None;
        //let mut feature_level = D3D_FEATURE_LEVEL_11_1;

        let mut device_context = None;
        let mut result = Ok(());

        for driver_type in driver_types {
            result = unsafe {
                D3D11CreateDevice(
                    None,
                    driver_type,
                    HMODULE::default(),
                    flags,
                    Some(&feature_levels),
                    //None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    //Some(&mut feature_level),
                    None,
                    Some(&mut device_context),
                )
            };
            if let Err(error) = result.clone() {
                warn!(
                    "Failed to create device with driver type {:?}, code: {}",
                    driver_type,
                    error.code()
                );
            } else if let Ok(_) = result.clone() {
                break;
            }
        }
        // Check if device creation was successful
        result?;

        let device = device.unwrap();
        let device_context = device_context.unwrap();

        let dxgi_device = device.cast::<IDXGIDevice>()?;
        let dxgi_adapter = unsafe { dxgi_device.GetParent::<IDXGIAdapter>() }?;
        log::info!("ScreenRecordManager initialized successfully");
        Ok(Arc::new(ScreenRecordManager {
            device,
            device_context,
            dxgi_adapter,
        }))
    }

    pub fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        let mut output_list = vec![];
        let mut output_index = 0;
        loop {
            let result = unsafe { self.dxgi_adapter.EnumOutputs(output_index) };
            if let Ok(output) = result {
                let output_desc: DXGI_OUTPUT_DESC = unsafe { output.GetDesc() }?;

                output_list.push(DisplayInfo::from(output_desc));
            } else if let Err(error) = result {
                if error.code() != DXGI_ERROR_NOT_FOUND {
                    log::error!(
                        "Failed to enumerate outputs, code: {}, message: {}",
                        error.code(),
                        error.message()
                    );
                    return Err(DeskError::from(error));
                }
                log::warn!(
                    "Output index not found, finished enumeration. Total outputs found: {}",
                    output_index
                );
                break;
            }
            output_index += 1;
        }
        Ok(output_list)
    }
}

pub trait ScreenRecordManagerArc {
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, DeskError>;
}

impl ScreenRecordManagerArc for Arc<ScreenRecordManager> {
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, DeskError> {
        ScreenOutput::new(self.clone(), output_index)
    }
}

pub struct ScreenOutput {
    pub manager: Arc<ScreenRecordManager>,
    pub output_index: u32,
    pub dup_output: IDXGIOutputDuplication,
    pub dxgi_output_desc: DXGI_OUTDUPL_DESC,
    pub texture2d: ID3D11Texture2D,
    pub surface: IDXGISurface,
}

impl ScreenOutput {
    pub fn new(
        screen_record_manager: Arc<ScreenRecordManager>,
        output_index: u32,
    ) -> Result<Self, DeskError> {
        let output = unsafe { screen_record_manager.dxgi_adapter.EnumOutputs(output_index) }?;

        let output1 = output.cast::<IDXGIOutput1>()?;
        // get the device from the manager and pass it to DuplicateOutput
        let pdevice = &screen_record_manager.device;

        let dup_output = unsafe { output1.DuplicateOutput(pdevice) }?;
        let dxgi_output_desc = unsafe { dup_output.GetDesc() };
        log::info!(
            "output_index {}, dxgi_output_desc {:?}",
            output_index,
            dxgi_output_desc
        );

        // Staging buffer/texture
        let mut copy_buffer_desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };

        copy_buffer_desc.Width = dxgi_output_desc.ModeDesc.Width;
        copy_buffer_desc.Height = dxgi_output_desc.ModeDesc.Height;
        copy_buffer_desc.MipLevels = 1;
        copy_buffer_desc.ArraySize = 1;
        //The format must be DXGI_FORMAT_B8G8R8A8_UNORM, see https://learn.microsoft.com/zh-cn/windows/win32/direct3ddxgi/desktop-dup-api#updating-the-desktop-image-data
        copy_buffer_desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        copy_buffer_desc.SampleDesc.Count = 1;
        copy_buffer_desc.SampleDesc.Quality = 0;
        copy_buffer_desc.Usage = D3D11_USAGE_STAGING;
        copy_buffer_desc.BindFlags = 0;
        copy_buffer_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        copy_buffer_desc.MiscFlags = 0;

        // create a texture to hold the screen capture

        let mut texture2d = None;
        unsafe {
            screen_record_manager.device.CreateTexture2D(
                &copy_buffer_desc,
                None,
                Some(&mut texture2d),
            )
        }?;
        let texture2d = texture2d.unwrap();

        unsafe { texture2d.SetEvictionPriority(DXGI_RESOURCE_PRIORITY_MAXIMUM.0) };
        let surface = texture2d.cast::<IDXGISurface>()?;

        Ok(ScreenOutput {
            manager: screen_record_manager,
            output_index,
            dup_output,
            dxgi_output_desc,
            texture2d,
            surface,
        })
    }
    /// DXGI_ERROR_WAIT_TIMEOUT
    pub fn get_frame(&self, draw_mouse: bool) -> Result<SceenFrame, DeskError> {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut desktop_resource: Option<IDXGIResource> = None;

        unsafe {
            let ummap_result = self.surface.Unmap();
            if let Err(e) = ummap_result {
                log::warn!(
                    "Failed to unmap surface: code: {}, message: {}",
                    e.code(),
                    e.message()
                );
            }

            let release_result = self.dup_output.ReleaseFrame();
            if let Err(e) = release_result {
                log::warn!(
                    "Failed to release frame: code: {}, message: {}",
                    e.code(),
                    e.message()
                );
            }

            self.dup_output
                .AcquireNextFrame(500, &mut frame_info, &mut desktop_resource)?;
        };
        let desktop_resource = desktop_resource.unwrap();

        let acquired_desktop_image = desktop_resource.cast::<ID3D11Texture2D>()?;

        unsafe {
            self.manager
                .device_context
                .CopyResource(&self.texture2d, &acquired_desktop_image)
        };
        let mut locked_rect = DXGI_MAPPED_RECT::default();

        let frame_buffer = unsafe {
            self.surface.Map(&mut locked_rect, DXGI_MAP_READ)?;
            core::slice::from_raw_parts(
                locked_rect.pBits,
                locked_rect.Pitch as usize * self.dxgi_output_desc.ModeDesc.Height as usize,
            )
        };

        Ok(SceenFrame {
            frame_info,
            frame_buffer,
        })
    }
}

pub struct NalInfo {
    pub nal_bytes: bytes::Bytes,
}

pub trait ScreenOutputVideoNal {
    fn get_nal(&mut self) -> Result<NalInfo, DeskError>;
}

#[derive(Debug)]
pub struct YuvPlanarImageWrapper<'a, T>
where
    T: Copy + Debug,
{
    pub inner: YuvPlanarImageMut<'a, T>,
}

impl<'a, T> YuvPlanarImageWrapper<'a, T>
where
    T: Copy + Debug,
{
    pub fn new(inner: YuvPlanarImageMut<'a, T>) -> Self {
        Self { inner }
    }
}

impl openh264::formats::YUVSource for YuvPlanarImageWrapper<'_, u8> {
    fn dimensions(&self) -> (usize, usize) {
        (self.inner.width as usize, self.inner.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (
            self.inner.y_stride as usize,
            self.inner.u_stride as usize,
            self.inner.v_stride as usize,
        )
    }

    fn y(&self) -> &[u8] {
        self.inner.y_plane.borrow()
    }

    fn u(&self) -> &[u8] {
        self.inner.u_plane.borrow()
    }

    fn v(&self) -> &[u8] {
        self.inner.v_plane.borrow()
    }
}
pub struct H264ScreenOutput {
    pub manager: Arc<ScreenRecordManager>,
    pub output_index: u32,
    pub screen_output: Option<ScreenOutput>,
    pub encoder: openh264::encoder::Encoder,
}

impl H264ScreenOutput {
    pub fn new(manager: Arc<ScreenRecordManager>, output_index: u32) -> Self {
        let config = openh264::encoder::EncoderConfig::new()
            .intra_frame_period(IntraFramePeriod::from_num_frames(30))
            .bitrate(BitRate::from_bps(10_000_000));
        let api = OpenH264API::from_source();
        let encoder = openh264::encoder::Encoder::with_api_config(api, config).unwrap();
        Self {
            manager,
            output_index,
            screen_output: None,
            encoder,
        }
    }
}

impl ScreenOutputVideoNal for H264ScreenOutput {
    fn get_nal(&mut self) -> Result<NalInfo, DeskError> {
        log::debug!("Start to get screen output frame");
        if self.screen_output.is_none() {
            log::info!("screen output is none, need to create screen output");
            let new_screen_output = self.manager.get_screen_output(self.output_index)?;
            self.screen_output = Some(new_screen_output);
        }
        let mut screen_output = self.screen_output.as_mut().unwrap();
        let mut result = screen_output.get_frame(true);
        if let Err(error) = result {
            if let DeskError::WindowsResultError(bt, err) = error {
                if err.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    log::warn!("capture frame timeout, will retry, error={:?}", err);
                    return DeskError::custom_error(
                        ErrorCode::CAPTURE_SCREEN_TIMEOUT_ERROR,
                        format!("capture frame timeout, will retry, error={:?}", err),
                    );
                } else if err.code() == DXGI_ERROR_ACCESS_LOST
                    || err.code() == DXGI_ERROR_INVALID_CALL
                {
                    log::error!(
                        "We lost access to the screen output, need to reinitialize the screen output, error={:?}, backtrace={}",
                        err,
                        bt
                    );
                    self.screen_output = None;
                    let new_screen_output = self.manager.get_screen_output(self.output_index)?;
                    self.screen_output = Some(new_screen_output);
                    screen_output = self.screen_output.as_mut().unwrap();

                    result = screen_output.get_frame(true);
                } else {
                    if err.code() == DXGI_ERROR_DEVICE_REMOVED {
                        let removed_reason =
                            unsafe { self.manager.device.GetDeviceRemovedReason() };
                        log::error!("Device removed reason: {:?}", removed_reason);
                        return Err(DeskError::WindowsResultError(Backtrace::disabled(), err));
                    }
                    return Err(DeskError::WindowsResultError(bt, err));
                }
            } else {
                return Err(error);
            }
        }

        let screen_frame = result?;
        log::debug!(
            "Got screen output frame, info={:?}",
            screen_frame.frame_info
        );
        let width = screen_output.dxgi_output_desc.ModeDesc.Width;
        let height = screen_output.dxgi_output_desc.ModeDesc.Height;
        let src_stride = width * 4;
        let mut planar_image = YuvPlanarImageMut::<u8>::alloc(
            width as u32,
            height as u32,
            YuvChromaSubsampling::Yuv420,
        );

        bgra_to_yuv420(
            &mut planar_image,
            screen_frame.frame_buffer,
            src_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt601,
            YuvConversionMode::Balanced,
        )?;
        log::debug!("Converted to YUV420 format");
        let yuv_source = YuvPlanarImageWrapper::<u8>::new(planar_image);

        let encoded_bit_stream = self.encoder.encode(&yuv_source)?;
        log::debug!("Encoded to H.264 format");
        let encoded_bit_bytes = bytes::Bytes::from(encoded_bit_stream.to_vec());
        log::debug!(
            "frame_type={:?}, num_layers={:?}",
            encoded_bit_stream.frame_type(),
            encoded_bit_stream.num_layers()
        );
        Ok(NalInfo {
            nal_bytes: encoded_bit_bytes,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SceenFrame<'a> {
    pub frame_info: DXGI_OUTDUPL_FRAME_INFO,
    pub frame_buffer: &'a [u8],
}

#[cfg(test)]
mod tests {
    use std::env;

    use std::sync::Once;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::System::StationsAndDesktops::{
        CloseWindowStation, EnumDesktopsW, EnumWindowStationsW, GetProcessWindowStation, HWINSTA,
        OpenWindowStationW,
    };
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    use yuv::bgra_to_rgba;

    use super::*;

    static INIT: Once = Once::new();

    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            env_logger::init_from_env(env_logger::Env::new().default_filter_or("DEBUG"));

            let result = ScreenRecordManager::set_thread_desktop();
            log::info!("set thread desktop result: {:?}", result);
        });
    }

    #[test]
    fn test_screen() -> Result<(), DeskError> {
        initialize();
        let settings = Settings::default();
        let manager = ScreenRecordManager::new(&settings)?;
        let list = manager.get_output_list()?;
        assert!(!list.is_empty());

        let screent_output = manager.get_screen_output(0)?;
        let tmp_dir = env::temp_dir();
        let tmp_dir = tmp_dir.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(tmp_dir.as_path())?;
        for i in 0..10 {
            let frame = screent_output.get_frame(false)?;
            log::info!(
                "frame_info={:?}, frame_buffer.len={}",
                frame.frame_info,
                frame.frame_buffer.len()
            );
            let mut rgb_data = vec![0u8; frame.frame_buffer.len()];
            let rgb_data_array = rgb_data.as_mut_slice();
            let width = screent_output.dxgi_output_desc.ModeDesc.Width;
            let height = screent_output.dxgi_output_desc.ModeDesc.Height;
            let src_stride = width * 4;
            let dst_stride = width * 4;
            // convert bgra to rgba
            bgra_to_rgba(
                frame.frame_buffer,
                src_stride,
                rgb_data_array,
                dst_stride,
                width,
                height,
            )?;
            let name = tmp_dir.join(format!("screenshot_{}.bmp", i));
            image::save_buffer(
                name.as_path(),
                rgb_data_array,
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
            log::info!("saved screenshot to {}", name.to_string_lossy().to_string());
        }
        std::fs::remove_dir_all(tmp_dir.as_path())?;

        Ok(())
    }

    unsafe extern "system" fn enum_proc(
        param0: windows_core::PCWSTR,
        param1: LPARAM,
    ) -> windows_core::BOOL {
        let result = unsafe { param0.to_string() };
        let windows_station_list_pointer = param1.0 as *mut Vec<String>;
        if let Ok(name) = result {
            log::info!("add: {}", name);
            let windows_station_list = unsafe { windows_station_list_pointer.as_mut().unwrap() };
            windows_station_list.push(name);
        } else if let Err(e) = result {
            log::error!("failed to add: {:?}", e);
        }

        return windows_core::BOOL::from(true);
    }

    fn list_desktop_by_station_handle(handle: HWINSTA) {
        let mut desktop_list = Vec::<String>::new();
        let desktop_list_pointer = &raw mut desktop_list;
        let lparam = LPARAM(desktop_list_pointer as isize);
        let enum_result = unsafe { EnumDesktopsW(Some(handle), Some(enum_proc), lparam) };
        log::info!("EnumDesktopsW result: {:?}", enum_result);
        log::info!("desktop_list: {:?}", desktop_list);
    }
    #[test]
    fn test_windows_api() -> Result<(), DeskError> {
        initialize();
        let is_admin = unsafe { IsUserAnAdmin() };

        log::info!("is user an admin: {}", is_admin.as_bool());
        let mut windows_station_list = Vec::<String>::new();
        let windows_station_list_pointer = &raw mut windows_station_list;
        let lparam = LPARAM(windows_station_list_pointer as isize);
        let result = unsafe { EnumWindowStationsW(Some(enum_proc), lparam) };
        log::info!("EnumWindowStationsW result: {:?}", result);
        log::info!("windows_station_list: {:?}", windows_station_list);

        for station in &windows_station_list {
            log::info!("station: {}", station);
            let station_name_utf16: Vec<u16> = station.encode_utf16().collect();
            let station_name_ptr = windows::core::PCWSTR::from_raw(station_name_utf16.as_ptr());
            let open_result = unsafe { OpenWindowStationW(station_name_ptr, true, 0) };

            if let Ok(handle) = open_result {
                list_desktop_by_station_handle(handle);

                let close_result = unsafe { CloseWindowStation(handle) };
                log::info!("CloseWindowStation result: {:?}", close_result);
            } else if let Err(e) = open_result {
                log::error!("OpenWindowStationW error: {}", e);
            }
        }
        let result = unsafe { GetProcessWindowStation() };
        if let Ok(handle) = result {
            log::info!("GetProcessWindowStation handle: {:?}", handle);
            list_desktop_by_station_handle(handle);
        } else if let Err(e) = result {
            log::error!("GetProcessWindowStation error: {}", e);
        }
        Ok(())
    }
}
