use desk_signal_facade::model::image_capture::Resolution;
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW};
use windows_core::HSTRING;

use crate::error::CaptureError;

pub fn enum_display_resolutions(device_name: &str) -> Result<Vec<Resolution>, CaptureError> {
    let mut resolutions = vec![];
    let device_name_hstr = HSTRING::from(device_name);
    let mut imodenum = 0;
    loop {
        let mut devmode = DEVMODEW::default();
        devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        let enum_result = unsafe {
            EnumDisplaySettingsW(
                &device_name_hstr,
                ENUM_DISPLAY_SETTINGS_MODE(imodenum),
                &mut devmode,
            )
        };
        if !enum_result.as_bool() {
            break;
        }
        log::info!(
            "Found display mode, width: {}, height: {}, bits per pixel: {}, display frequency: {}",
            devmode.dmPelsWidth,
            devmode.dmPelsHeight,
            devmode.dmBitsPerPel,
            devmode.dmDisplayFrequency
        );
        resolutions.push(Resolution::new(devmode.dmPelsWidth, devmode.dmPelsHeight));
        imodenum += 1;
    }

    Ok(resolutions)
}
