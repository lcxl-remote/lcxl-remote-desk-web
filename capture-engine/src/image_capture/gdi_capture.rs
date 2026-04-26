use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect},
};
use desk_utils::error::DeskErrorCode;
use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
        CreateCompatibleDC, DEVMODEW, DIB_RGB_COLORS, DISPLAY_DEVICE_STATE_FLAGS, DISPLAY_DEVICEW,
        DeleteDC, DeleteObject, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
        GetDC, GetDIBits, GetObjectW, HBITMAP, HDC, RGBQUAD, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::WindowsAndMessaging::{
        CURSORINFO, DI_COMPAT, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, ICONINFO,
    },
};
use windows_core::PCWSTR;

use crate::{
    error::CaptureError,
    image_capture::windows::enum_display_resolutions,
    model::image_capture::CursorSyncData,
    model::image_capture::{
        CaptureRequest, CaptureResult, CursorCaptureMode, ImageCapture, ImageCaptureType,
        ImageInfo, ImageOutputEnumerator, ImageType,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GdiCursorFingerprint {
    Hidden,
    Shape(u64),
}

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

    pub fn get_output(idevnum: u32) -> Result<Option<DisplayInfo>, CaptureError> {
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
                left: unsafe { dev_mode.Anonymous1.Anonymous2.dmPosition.x },
                top: unsafe { dev_mode.Anonymous1.Anonymous2.dmPosition.y },
                right: unsafe { dev_mode.Anonymous1.Anonymous2.dmPosition.x }
                    + dev_mode.dmPelsWidth as i32,
                bottom: unsafe { dev_mode.Anonymous1.Anonymous2.dmPosition.y }
                    + dev_mode.dmPelsHeight as i32,
            },
            resolutions,
            attached_to_desktop: true,
            rotation: 0,
        }))
    }
}

impl ImageOutputEnumerator for GdiImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        let mut dev_index = 0u32;
        let mut display_info_list = Vec::new();
        loop {
            let display_info_opt = GdiImageOutputEnumerator::get_output(dev_index)?;
            let display_info = if let Some(display_info) = display_info_opt {
                display_info
            } else {
                break;
            };

            display_info_list.push(display_info);
            dev_index += 1;
        }
        Ok(display_info_list)
    }
}

pub struct GdiImageCapture {
    pub idevnum: u32,
    pub display_info: DisplayInfo,
    last_cursor_fingerprint: Option<GdiCursorFingerprint>,
}

impl GdiImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, CaptureError> {
        log::debug!("Creating GDIImageCapture with settings: {:?}", settings);
        let display_info_opt = GdiImageOutputEnumerator::get_output(settings.video_device_index)?;
        let display_info = if let Some(display_info) = display_info_opt {
            display_info
        } else {
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "Cannot get current output by index {}",
                    settings.video_device_index
                ),
            );
        };
        Ok(GdiImageCapture {
            idevnum: settings.video_device_index,
            display_info,
            last_cursor_fingerprint: None,
        })
    }

    fn capture_cursor_update(
        &self,
    ) -> Result<Option<(GdiCursorFingerprint, CursorSyncData)>, CaptureError> {
        let mut cursor_info = CURSORINFO::default();
        cursor_info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;

        let cursor_valid = unsafe {
            GetCursorInfo(&mut cursor_info)?;
            !cursor_info.hCursor.is_invalid()
        };

        let is_visible = cursor_valid
            && (cursor_info.flags == windows::Win32::UI::WindowsAndMessaging::CURSOR_SHOWING);

        if !is_visible {
            return Ok(Some((
                GdiCursorFingerprint::Hidden,
                CursorSyncData {
                    visible: false,
                    ..Default::default()
                },
            )));
        }

        let shape_id = cursor_info.hCursor.0 as u64;
        let mut icon_info = ICONINFO::default();
        unsafe {
            GetIconInfo(cursor_info.hCursor.into(), &mut icon_info)?;
        }

        let screen_dc = GDIHDC::get_hdc(None);
        let mut bmp = BITMAP::default();

        let is_color = !icon_info.hbmColor.is_invalid();
        let target_hbm = if is_color {
            icon_info.hbmColor
        } else {
            icon_info.hbmMask
        };

        unsafe {
            GetObjectW(
                target_hbm.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut _ as _),
            )
        };

        let width = bmp.bmWidth as u32;
        let height = if is_color {
            bmp.bmHeight as u32
        } else {
            (bmp.bmHeight / 2) as u32
        };

        if width == 0 || height == 0 {
            if !icon_info.hbmMask.is_invalid() {
                unsafe { DeleteObject(icon_info.hbmMask.into()) };
            }
            if !icon_info.hbmColor.is_invalid() {
                unsafe { DeleteObject(icon_info.hbmColor.into()) };
            }
            return Ok(None);
        }

        let mut rgba_buffer = Vec::new();

        if is_color {
            let bi_bit_count = 32u16;
            let mut color_buffer: Vec<u8> = vec![0u8; (width * height * 4) as usize];
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
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

            unsafe {
                GetDIBits(
                    screen_dc.hdc,
                    icon_info.hbmColor,
                    0,
                    height,
                    Some(color_buffer.as_mut_ptr() as *mut _),
                    &mut bi,
                    DIB_RGB_COLORS,
                )
            };

            for chunk in color_buffer.chunks_exact(4) {
                rgba_buffer.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
            }
        } else {
            let mask_height = bmp.bmHeight as u32;
            let mask_buffer_size = ((width + 31) / 32) * 4 * mask_height;
            let mut mask_buffer = vec![0u8; mask_buffer_size as usize];
            let mut mask_bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(mask_height as i32),
                    biPlanes: 1,
                    biBitCount: 1,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD::default()],
            };

            unsafe {
                GetDIBits(
                    screen_dc.hdc,
                    icon_info.hbmMask,
                    0,
                    mask_height,
                    Some(mask_buffer.as_mut_ptr() as *mut _),
                    &mut mask_bi,
                    DIB_RGB_COLORS,
                )
            };

            let pitch = ((width + 31) / 32) * 4;
            for y in 0..height {
                let and_row = y as usize * pitch as usize;
                let xor_row = (y + height) as usize * pitch as usize;
                for x in 0..width {
                    let byte_offset = (x / 8) as usize;
                    let bit_offset = x % 8;
                    let mask = 0x80 >> bit_offset;
                    let and_byte = mask_buffer.get(and_row + byte_offset).copied().unwrap_or(0);
                    let xor_byte = mask_buffer.get(xor_row + byte_offset).copied().unwrap_or(0);
                    let and_bit = (and_byte & mask) != 0;
                    let xor_bit = (xor_byte & mask) != 0;
                    let (r, g, b, a) = match (and_bit, xor_bit) {
                        (true, false) => (0, 0, 0, 0),
                        (false, false) => (0, 0, 0, 255),
                        (false, true) => (255, 255, 255, 255),
                        (true, true) => (0, 0, 0, 255),
                    };
                    rgba_buffer.extend_from_slice(&[r, g, b, a]);
                }
            }
        }

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

        use image::{ImageBuffer, Rgba};
        use std::io::Cursor;
        let img = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba_buffer)
            .unwrap_or_else(|| ImageBuffer::new(width, height));
        let mut png_data = Cursor::new(Vec::new());
        img.write_to(&mut png_data, image::ImageFormat::Png)
            .map_err(|e| {
                CaptureError::custom_error::<()>(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
                    .unwrap_err()
            })?;
        use base64::Engine;
        let base64_png = base64::engine::general_purpose::STANDARD.encode(png_data.into_inner());

        let screen_width = self.display_info.desktop_coordinates.width() as u32;
        let screen_height = self.display_info.desktop_coordinates.height() as u32;

        Ok(Some((
            GdiCursorFingerprint::Shape(shape_id),
            CursorSyncData {
                base64_png,
                hotspot_x: icon_info.xHotspot as i32,
                hotspot_y: icon_info.yHotspot as i32,
                visible: true,
                shape_id,
                screen_width,
                screen_height,
            },
        )))
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
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureResult, CaptureError> {
        let draw_mouse = matches!(request.cursor_mode, CursorCaptureMode::RenderInFrame);
        let display_info = &self.display_info;
        let width = display_info.desktop_coordinates.width();
        let height = display_info.desktop_coordinates.height();
        let left = display_info.desktop_coordinates.left;
        let top = display_info.desktop_coordinates.top;

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
                left,
                top,
                SRCCOPY,
            )?;
        }
        if draw_mouse {
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
                        cursor_pos.x - left,
                        cursor_pos.y - top,
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
        let mut cursor_update = None;
        if matches!(request.cursor_mode, CursorCaptureMode::SyncNative) {
            match self.capture_cursor_update() {
                Ok(Some((fingerprint, data))) => {
                    if self.last_cursor_fingerprint != Some(fingerprint) {
                        self.last_cursor_fingerprint = Some(fingerprint);
                        cursor_update = Some(data);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    log::warn!("Failed to capture cursor update in GDI backend: {}", err);
                }
            }
        } else {
            self.last_cursor_fingerprint = None;
        }

        Ok(CaptureResult {
            image: Box::new(GDIImageInfo {
                bitmap_buffer,
                width: width as u32,
                height: height as u32,
            }),
            cursor_update,
            content_changed: true,
            dirty_rects: None,
        })
    }

    fn supports_cursor_sync(&self) -> bool {
        true
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::GDI
    }

    fn get_current_output(&self) -> Result<DisplayInfo, CaptureError> {
        Ok(self.display_info.clone())
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

    use crate::model::image_capture::{CaptureRequest, CursorCaptureMode};
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
    fn test_capture_image() -> Result<(), CaptureError> {
        initialize();
        let display_info = GdiImageOutputEnumerator::get_output(0)?.unwrap();
        let mut image_capture = GdiImageCapture {
            idevnum: 0,
            display_info,
            last_cursor_fingerprint: None,
        };
        let capture_result = image_capture.capture(CaptureRequest {
            cursor_mode: CursorCaptureMode::RenderInFrame,
        })?;
        let image_info = capture_result.image;

        let tmp_dir = PathBuf::from("sample");
        if !tmp_dir.exists() {
            std::fs::create_dir_all(&tmp_dir).unwrap();
        }
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
