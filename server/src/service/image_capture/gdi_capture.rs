use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
        CreateCompatibleDC, DEVMODEW, DIB_RGB_COLORS, DISPLAY_DEVICE_STATE_FLAGS, DISPLAY_DEVICEW,
        DeleteDC, DeleteObject, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
        GetDC, GetDIBits, GetObjectW, HBITMAP, HDC, RGBQUAD, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::WindowsAndMessaging::{
        CURSORINFO, DI_COMPAT, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics,
        ICONINFO, SM_CXSCREEN, SM_CYSCREEN,
    },
};
use windows_core::PCWSTR;

use crate::{
    desk_error::DeskError,
    model::{
        common::ErrorCode,
        image_capture::{
            DisplayInfo, DisplayRect, ImageCapture, ImageCaptureType, ImageInfo,
            ImageOutputEnumerator, ImageType,
        },
        settings::DeskSettings,
    },
    service::image_capture::windows::enum_display_resolutions,
};

enum GDIHDCType {
    Get(Option<HWND>),
    Create,
}

struct GDIHDC {
    dc_type: GDIHDCType,
    hdc: HDC,
}

impl GDIHDC {
    fn get_hdc(hwnd: Option<HWND>) -> Self {
        let hdc = unsafe { GetDC(hwnd) };
        Self {
            dc_type: GDIHDCType::Get(hwnd),
            hdc,
        }
    }

    fn create_compatible_hdc(hdc: Option<HDC>) -> Self {
        let new_hdc = unsafe { CreateCompatibleDC(hdc) };
        Self {
            dc_type: GDIHDCType::Create,
            hdc: new_hdc,
        }
    }
}

impl Drop for GDIHDC {
    fn drop(&mut self) {
        match self.dc_type {
            GDIHDCType::Get(hwnd) => unsafe {
                ReleaseDC(hwnd, self.hdc);
            },
            GDIHDCType::Create => unsafe {
                let result = DeleteDC(self.hdc);
                if !result.as_bool() {
                    log::error!("Failed to release DC when drop GDIHDC");
                }
            },
        }
    }
}

struct GDIHBITMAP {
    hbitmap: HBITMAP,
}

impl Drop for GDIHBITMAP {
    fn drop(&mut self) {
        let result = unsafe { DeleteObject(self.hbitmap.into()) };
        if !result.as_bool() {
            log::error!("Failed to delete object when drop GDIHBITMAP");
        }
    }
}

pub struct GdiImageOutputEnumerator {}

impl GdiImageOutputEnumerator {
    pub fn new() -> Self {
        GdiImageOutputEnumerator {}
    }

    pub fn get_output(idevnum: u32) -> Result<Option<DisplayInfo>, DeskError> {
        let mut display_device = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            DeviceName: [0u16; 32],
            DeviceString: [0u16; 128],
            StateFlags: DISPLAY_DEVICE_STATE_FLAGS(0),
            DeviceID: [0u16; 128],
            DeviceKey: [0u16; 128],
        };
        let null_str = PCWSTR::null();
        let result = unsafe { EnumDisplayDevicesW(null_str, idevnum, &mut display_device, 0) };
        if !result.as_bool() {
            log::debug!("End of enum display devices: idevnum: {}", idevnum);
            return Ok(None);
        }

        let display_name = u16array_to_string(&display_device.DeviceString);
        let device_name = u16array_to_string(&display_device.DeviceName);
        let device_id = u16array_to_string(&display_device.DeviceID);
        let device_key = u16array_to_string(&display_device.DeviceKey);
        log::debug!(
            "idevnum: {}, display device: {}, device name: {}, device id: {}, device key: {}",
            idevnum,
            display_name,
            device_name,
            device_id,
            device_key
        );

        let mut dev_mode = DEVMODEW::default();
        dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;

        let device_name_utf16 = format!("{}\0", device_name)
            .encode_utf16()
            .collect::<Vec<u16>>();
        let result = unsafe {
            EnumDisplaySettingsW(
                PCWSTR::from_raw(device_name_utf16.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            )
        };
        if !result.as_bool() {
            log::warn!("Failed to get current settings for device {}", device_name);
            return Ok(None);
        }
        let resolutions = enum_display_resolutions(&device_name)?;
        Ok(Some(DisplayInfo {
            device_name,
            display_device_name: Some(display_name),
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: dev_mode.dmPelsWidth as i32,
                bottom: dev_mode.dmPelsHeight as i32,
            },
            resolutions,
            attached_to_desktop: true,
            rotation: 0,
        }))
    }
}

impl ImageOutputEnumerator for GdiImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        let mut dev_index = 0u32;
        let mut display_info_list = Vec::new();
        loop {
            let display_info_opt = GdiImageOutputEnumerator::get_output(dev_index)?;
            if display_info_opt.is_none() {
                break;
            }
            let display_info = display_info_opt.unwrap();

            display_info_list.push(display_info);
            dev_index += 1;
        }
        Ok(display_info_list)
    }
}

pub struct GdiImageCapture {
    pub idevnum: u32,
}

impl GdiImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, DeskError> {
        log::debug!("Creating GDIImageCapture with settings: {:?}", settings);

        Ok(GdiImageCapture {
            idevnum: settings.video_device_index,
        })
    }
}

pub struct GDIImageInfo {
    pub bitmap_buffer: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl ImageInfo for GDIImageInfo {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }

    fn get_data(&self) -> &[u8] {
        self.bitmap_buffer.as_slice()
    }

    fn get_width(&self) -> u32 {
        self.width
    }

    fn get_height(&self) -> u32 {
        self.height
    }
}

impl ImageCapture for GdiImageCapture {
    fn capture(&mut self, show_mouse: bool) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };

        let screen_dc = GDIHDC::get_hdc(None);

        let mem_dc = GDIHDC::create_compatible_hdc(Some(screen_dc.hdc));
        // Create a compatible bitmap from the Window DC.
        let mem_hbm = unsafe { CreateCompatibleBitmap(screen_dc.hdc, width, height) };
        let mem_hbm = GDIHBITMAP { hbitmap: mem_hbm };
        // Select the compatible bitmap into the compatible memory DC.
        unsafe {
            SelectObject(mem_dc.hdc, mem_hbm.hbitmap.into());
            BitBlt(
                mem_dc.hdc,
                0,
                0,
                width,
                height,
                Some(screen_dc.hdc),
                0,
                0,
                SRCCOPY,
            )?;
        }
        if show_mouse {
            /*
             In the code for obtaining the cursor image, the coordinates returned by GetCursorPos are the midpoint of the cursor.
            The DrawIconEx function requires the top-left coordinate of the cursor. Therefore, GetIconInfo should be called to convert the coordinate point.
            */
            let mut cursor_info = CURSORINFO::default();
            cursor_info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;

            let cursor_valid = unsafe {
                GetCursorInfo(&mut cursor_info)?;
                !cursor_info.hCursor.is_invalid()
            };

            if cursor_valid {
                let mut icon_info = ICONINFO::default();
                unsafe {
                    GetIconInfo(cursor_info.hCursor.into(), &mut icon_info)?;
                }
                let mut cursor_pos = cursor_info.ptScreenPos;
                cursor_pos.x -= icon_info.xHotspot as i32;
                cursor_pos.y -= icon_info.yHotspot as i32;
                if !icon_info.hbmMask.is_invalid() {
                    let result = unsafe { DeleteObject(icon_info.hbmMask.into()) };
                    if !result.as_bool() {
                        log::error!("Failed to delete cursor mask bitmap");
                    }
                }
                if !icon_info.hbmColor.is_invalid() {
                    let result = unsafe { DeleteObject(icon_info.hbmColor.into()) };
                    if !result.as_bool() {
                        log::error!("Failed to delete cursor color bitmap");
                    }
                }
                unsafe {
                    DrawIconEx(
                        mem_dc.hdc,
                        cursor_pos.x,
                        cursor_pos.y,
                        cursor_info.hCursor.into(),
                        0,
                        0,
                        0,
                        None,
                        DI_NORMAL | DI_COMPAT,
                    )?;
                }
            }
        }

        // Get the BITMAP from the HBITMAP.
        let mut mem_bmp = BITMAP::default();
        unsafe {
            GetObjectW(
                mem_hbm.hbitmap.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut mem_bmp as *mut _ as _),
            )
        };
        let bi_bit_count = 32u16;
        let bmp_size = ((width * bi_bit_count as i32 + 31) / 32) * 4 * height;
        let mut bitmap_buffer: Vec<u8> = vec![0u8; bmp_size as usize];

        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: bi_bit_count,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD::default()],
        };

        // Gets the "bits" from the bitmap, and copies them into a buffer
        // that's pointed to by lpbitmap.
        unsafe {
            GetDIBits(
                mem_dc.hdc,
                mem_hbm.hbitmap,
                0,
                height as u32,
                Some(bitmap_buffer.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            )
        };
        Ok(Box::new(GDIImageInfo {
            bitmap_buffer,
            width: width as u32,
            height: height as u32,
        }))
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::DGI
    }

    fn get_current_output(&self) -> Result<DisplayInfo, DeskError> {
        let opt = GdiImageOutputEnumerator::get_output(self.idevnum)?;
        if let Some(display_info) = opt {
            Ok(display_info)
        } else {
            DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!("Cannot get current output by index {}", self.idevnum),
            )
        }
    }
}

fn u16array_to_string(u16array: &[u16]) -> String {
    let null_char_index = u16array
        .iter()
        .position(|&item| item == 0u16)
        .unwrap_or(u16array.len());
    String::from_utf16_lossy(&u16array[..null_char_index])
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Once};

    use desk_utils::logs::init_logs;
    use log::LevelFilter;
    use yuv::bgra_to_rgba;

    use super::*;
    static INIT: Once = Once::new();

    fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            let _ = init_logs(LevelFilter::Debug);
        });
    }
    #[test]
    fn test_capture_image() -> Result<(), DeskError> {
        initialize();
        let mut image_capture = GdiImageCapture { idevnum: 0 };
        let image_info = image_capture.capture(true)?;

        let tmp_dir = PathBuf::from("sample");
        let bmp_path = tmp_dir.join(format!("screenshot_gdi.bmp"));

        let src_stride = image_info.get_width() * 4;
        let dst_stride = image_info.get_width() * 4;
        let mut rgb_data = vec![0u8; image_info.get_data().len()];
        let rgb_data_array = rgb_data.as_mut_slice();
        // convert bgra to rgba
        bgra_to_rgba(
            image_info.get_data(),
            src_stride,
            rgb_data_array,
            dst_stride,
            image_info.get_width(),
            image_info.get_height(),
        )?;
        image::save_buffer(
            bmp_path,
            rgb_data_array,
            image_info.get_width(),
            image_info.get_height(),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();

        let output_list = GdiImageOutputEnumerator::new().get_output_list()?;
        log::info!("output_list={:?}", output_list);
        Ok(())
    }
}
