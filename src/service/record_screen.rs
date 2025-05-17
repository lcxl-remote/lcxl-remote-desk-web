use std::sync::Arc;

use crate::desk_error::DeskError;
use log::warn;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_REFERENCE,
            D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
        },
        Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
            ID3D11DeviceContext, ID3D11Texture2D,
        },
        Dxgi::{
            Common::DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ERROR_NOT_FOUND, DXGI_MAP_READ,
            DXGI_MAPPED_RECT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
            DXGI_RESOURCE_PRIORITY_MAXIMUM, IDXGIAdapter, IDXGIDevice, IDXGIOutput1,
            IDXGIOutputDuplication, IDXGIResource, IDXGISurface,
        },
    },
};
use windows_core::Interface;

pub struct ScreenRecordManager {
    pub device: ID3D11Device,
    pub device_context: ID3D11DeviceContext,
    pub dxgi_adapter: IDXGIAdapter,
}

impl ScreenRecordManager {
    pub fn new() -> Result<Arc<Self>, DeskError> {
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
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

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

    pub fn get_output_list(&self) -> Result<Vec<DXGI_OUTPUT_DESC>, DeskError> {
        let mut output_list = vec![];
        let mut output_index = 0;
        loop {
            let result = unsafe { self.dxgi_adapter.EnumOutputs(output_index) };
            if let Ok(output) = result {
                let output_desc: DXGI_OUTPUT_DESC = unsafe { output.GetDesc() }?;
                log::info!("Found output {:?}", output_desc);
                output_list.push(output_desc);
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
        log::info!("Start to convert frame_buffer from bgra format to rgba format.");
        let mut rgb_data = Vec::<u8>::with_capacity(frame_buffer.len());
        for chunk in frame_buffer.chunks(4) {
            rgb_data.push(chunk[2]);
            rgb_data.push(chunk[1]);
            rgb_data.push(chunk[0]);
            rgb_data.push(chunk[3]); // alpha channel
        }
        let frame_buffer = rgb_data;
        log::info!("End to convert frame_buffer.");

        Ok(SceenFrame {
            frame_info,
            frame_buffer,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SceenFrame {
    pub frame_info: DXGI_OUTDUPL_FRAME_INFO,
    pub frame_buffer: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    #[test]
    fn test_screen() -> Result<(), DeskError> {
        env_logger::init_from_env(env_logger::Env::new().default_filter_or("DEBUG"));
        let manager = ScreenRecordManager::new()?;
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

            let name = tmp_dir.join(format!("screenshot_{}.bmp", i));
            image::save_buffer(
                name.as_path(),
                &frame.frame_buffer,
                screent_output.dxgi_output_desc.ModeDesc.Width,
                screent_output.dxgi_output_desc.ModeDesc.Height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
            log::info!("saved screenshot to {}", name.to_string_lossy().to_string());
        }
        //std::fs::remove_dir_all(tmp_dir.as_path())?;

        Ok(())
    }
}
