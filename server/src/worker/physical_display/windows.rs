//! Windows physical display mode provider using the installed windows-rs API.

use std::mem::size_of;

use windows::{
    Win32::Graphics::Gdi::{
        CDS_TEST, CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
        DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_MIRRORING_DRIVER,
        DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
        ENUM_DISPLAY_SETTINGS_FLAGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW,
        EnumDisplaySettingsExW,
    },
    Win32::UI::WindowsAndMessaging::EDD_GET_DEVICE_INTERFACE_NAME,
    core::PCWSTR,
};

use super::{DisplayTopology, PhysicalDisplaySnapshot, PhysicalMode, verify_after_apply};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn utf16(value: &[u16]) -> String {
    String::from_utf16_lossy(
        &value[..value
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(value.len())],
    )
}

fn devices() -> Vec<DISPLAY_DEVICEW> {
    let mut result = Vec::new();
    for index in 0..256 {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0) }.as_bool() {
            break;
        }
        if device
            .StateFlags
            .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
        {
            result.push(device);
        }
    }
    result
}

fn selected_device(device_name: &str) -> Result<DISPLAY_DEVICEW, String> {
    let device = devices()
        .into_iter()
        .find(|device| utf16(&device.DeviceName) == device_name)
        .ok_or_else(|| "selected physical display is not attached".to_string())?;
    let identity = format!(
        "{} {}",
        utf16(&device.DeviceID),
        utf16(&device.DeviceString)
    );
    if device.StateFlags.contains(DISPLAY_DEVICE_MIRRORING_DRIVER)
        || identity.to_ascii_lowercase().contains("lcxlvirtualdisplay")
    {
        return Err("selected display is virtual or mirrored".into());
    }
    Ok(device)
}

fn monitor_identity(device_name: &str) -> Result<String, String> {
    let name = wide(device_name);
    let mut interfaces = Vec::new();
    for index in 0..256 {
        let mut monitor = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplayDevicesW(
                PCWSTR::from_raw(name.as_ptr()),
                index,
                &mut monitor,
                EDD_GET_DEVICE_INTERFACE_NAME,
            )
        }
        .as_bool()
        {
            break;
        }
        let interface = utf16(&monitor.DeviceID);
        if !interface.is_empty() {
            interfaces.push(interface);
        }
    }
    if interfaces.is_empty() {
        return Err("physical monitor device interface is unavailable".into());
    }
    interfaces.sort_unstable();
    Ok(format!("win:{device_name}:{}", interfaces.join("|")))
}

fn enumerate_mode(device_name: &str, index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
    let name = wide(device_name);
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe {
        EnumDisplaySettingsExW(
            PCWSTR::from_raw(name.as_ptr()),
            index,
            &mut mode,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
    }
    .as_bool()
    .then_some(mode)
}

fn mode_data(mode: &DEVMODEW) -> PhysicalMode {
    let orientation = unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation.0 };
    let fixed_output = unsafe { mode.Anonymous1.Anonymous2.dmDisplayFixedOutput.0 };
    PhysicalMode {
        selector: format!(
            "{}:{}:{}:{}:{}:{}",
            mode.dmPelsWidth,
            mode.dmPelsHeight,
            mode.dmDisplayFrequency,
            mode.dmBitsPerPel,
            orientation,
            fixed_output,
        ),
        logical_width: mode.dmPelsWidth,
        logical_height: mode.dmPelsHeight,
        pixel_width: mode.dmPelsWidth,
        pixel_height: mode.dmPelsHeight,
        capture_width: mode.dmPelsWidth,
        capture_height: mode.dmPelsHeight,
        refresh_millihz: mode.dmDisplayFrequency.saturating_mul(1000),
    }
}

fn modes(device_name: &str, current: &DEVMODEW) -> Vec<PhysicalMode> {
    let current_orientation = unsafe { current.Anonymous1.Anonymous2.dmDisplayOrientation };
    let current_fixed = unsafe { current.Anonymous1.Anonymous2.dmDisplayFixedOutput };
    (0..2048)
        .map_while(|index| enumerate_mode(device_name, ENUM_DISPLAY_SETTINGS_MODE(index)))
        .filter(|mode| {
            mode.dmBitsPerPel == current.dmBitsPerPel
                && unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation } == current_orientation
                && unsafe { mode.Anonymous1.Anonymous2.dmDisplayFixedOutput } == current_fixed
        })
        .map(|mode| mode_data(&mode))
        .collect()
}

fn topology() -> Result<Vec<DisplayTopology>, String> {
    devices()
        .into_iter()
        .map(|device| {
            let device_name = utf16(&device.DeviceName);
            let current = enumerate_mode(&device_name, ENUM_CURRENT_SETTINGS)
                .ok_or_else(|| format!("current mode unavailable for {device_name}"))?;
            let position = unsafe { current.Anonymous1.Anonymous2.dmPosition };
            let identity = monitor_identity(&device_name)
                .unwrap_or_else(|_| format!("unresolved:{}", utf16(&device.DeviceID)));
            Ok(DisplayTopology {
                device_name,
                identity,
                mode_selector: mode_data(&current).selector,
                main: device.StateFlags.contains(DISPLAY_DEVICE_PRIMARY_DEVICE),
                mirrored: device.StateFlags.contains(DISPLAY_DEVICE_MIRRORING_DRIVER),
                origin_x: position.x,
                origin_y: position.y,
            })
        })
        .collect()
}

pub(super) fn inspect(device_name: &str) -> Result<PhysicalDisplaySnapshot, String> {
    selected_device(device_name)?;
    let identity = monitor_identity(device_name)?;
    let current = enumerate_mode(device_name, ENUM_CURRENT_SETTINGS)
        .ok_or_else(|| "selected physical display has no current mode".to_string())?;
    Ok(PhysicalDisplaySnapshot {
        device_name: device_name.into(),
        identity,
        current: mode_data(&current),
        candidates: modes(device_name, &current),
        topology: topology()?,
    })
}

fn apply_raw(
    device_name: &str,
    selector: &str,
    expected_current_selector: &str,
    expected_display_identity: &str,
) -> Result<(), String> {
    if monitor_identity(device_name)? != expected_display_identity {
        return Err("physical display identity changed before mode apply".into());
    }
    let current = enumerate_mode(device_name, ENUM_CURRENT_SETTINGS)
        .ok_or_else(|| "current mode vanished before apply".to_string())?;
    if mode_data(&current).selector != expected_current_selector {
        return Err("physical display changed locally before mode apply".into());
    }
    let current_orientation = unsafe { current.Anonymous1.Anonymous2.dmDisplayOrientation };
    let current_fixed = unsafe { current.Anonymous1.Anonymous2.dmDisplayFixedOutput };
    let candidate = (0..2048)
        .map_while(|index| enumerate_mode(device_name, ENUM_DISPLAY_SETTINGS_MODE(index)))
        .find(|mode| {
            mode_data(mode).selector == selector
                && mode.dmBitsPerPel == current.dmBitsPerPel
                && unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation } == current_orientation
                && unsafe { mode.Anonymous1.Anonymous2.dmDisplayFixedOutput } == current_fixed
        })
        .ok_or_else(|| "selected mode disappeared before apply".to_string())?;
    let name = wide(device_name);
    let name = PCWSTR::from_raw(name.as_ptr());
    let tested = unsafe { ChangeDisplaySettingsExW(name, Some(&candidate), None, CDS_TEST, None) };
    if tested != DISP_CHANGE_SUCCESSFUL {
        return Err(format!(
            "CDS_TEST rejected physical display mode: {}",
            tested.0
        ));
    }
    let changed =
        unsafe { ChangeDisplaySettingsExW(name, Some(&candidate), None, CDS_TYPE(0), None) };
    if changed != DISP_CHANGE_SUCCESSFUL {
        return Err(format!(
            "physical display mode switch failed: {}",
            changed.0
        ));
    }
    Ok(())
}

pub(super) fn apply(
    device_name: &str,
    selector: &str,
    expected_current_selector: &str,
    expected_display_identity: &str,
) -> Result<PhysicalDisplaySnapshot, String> {
    let before = inspect(device_name)?;
    if before.identity != expected_display_identity {
        return Err("physical display identity changed before mode apply".into());
    }
    if before.current.selector != expected_current_selector {
        return Err("physical display changed locally before mode apply".into());
    }
    if before.current.selector == selector {
        return Ok(before);
    }
    if !before
        .candidates
        .iter()
        .any(|mode| mode.selector == selector)
    {
        return Err("selected mode is not in the current display enumeration".into());
    }
    apply_raw(
        device_name,
        selector,
        expected_current_selector,
        expected_display_identity,
    )?;
    verify_after_apply(&before, inspect(device_name), selector, || {
        apply_raw(
            device_name,
            &before.current.selector,
            selector,
            expected_display_identity,
        )?;
        let restored = inspect(device_name)?;
        if restored.identity != before.identity
            || restored.current.selector != before.current.selector
        {
            return Err("original mode readback did not match".into());
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_gdi_device_name() {
        let name = r"\\.\DISPLAY1";
        assert_eq!(utf16(&wide(name)), name);
    }
}
