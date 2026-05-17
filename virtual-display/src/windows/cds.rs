//! `ChangeDisplaySettingsExW` glue — committing the negotiated mode into
//! the user session.
//!
//! The driver-side `IddCxMonitorUpdateModes` only *advertises* a new mode
//! list to the OS; the desktop window manager keeps using the previously
//! selected mode until the user (or some user-session caller) invokes
//! `ChangeDisplaySettingsExW`. The worker runs in the user's session, so
//! this is the natural place to issue the call.

use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODE_FIELD_FLAGS, DEVMODEW, DISP_CHANGE,
    DISP_CHANGE_BADMODE, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
};
use windows::core::PCWSTR;

use crate::{VirtualDisplayError, VirtualDisplayMode};

fn encode_device_name(name: &str) -> Vec<u16> {
    let mut v: Vec<u16> = name.encode_utf16().collect();
    v.push(0);
    v
}

/// Build the `DEVMODEW` we will hand to `ChangeDisplaySettingsExW`.
/// Split out so unit tests can pin the field-flag layout without
/// invoking GDI.
fn build_devmode(mode: VirtualDisplayMode) -> DEVMODEW {
    DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        dmFields: DEVMODE_FIELD_FLAGS(DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0),
        dmPelsWidth: mode.width,
        dmPelsHeight: mode.height,
        dmDisplayFrequency: mode.refresh_hz,
        ..Default::default()
    }
}

/// Apply `mode` against the display identified by `device_name`
/// (e.g. `\\.\DISPLAY3`). `CDS_UPDATEREGISTRY` makes the change
/// persistent across logoff/login.
pub fn apply_cds(device_name: &str, mode: VirtualDisplayMode) -> Result<(), VirtualDisplayError> {
    let name = encode_device_name(device_name);
    let devmode = build_devmode(mode);

    // SAFETY: name and devmode outlive this call; CDS_UPDATEREGISTRY is
    // a documented flag value.
    let result: DISP_CHANGE = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(&devmode),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if result == DISP_CHANGE_SUCCESSFUL {
        return Ok(());
    }
    let msg = if result == DISP_CHANGE_BADMODE {
        format!(
            "BADMODE for {device_name} @ {}x{}@{}; driver did not advertise this mode",
            mode.width, mode.height, mode.refresh_hz
        )
    } else {
        format!(
            "DISP_CHANGE code {} for {device_name} @ {}x{}@{}",
            result.0, mode.width, mode.height, mode.refresh_hz
        )
    };
    Err(VirtualDisplayError::Cds(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_devmode_sets_size_and_required_fields() {
        let devmode = build_devmode(VirtualDisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });
        assert_eq!(devmode.dmSize as usize, std::mem::size_of::<DEVMODEW>());
        assert_eq!(devmode.dmPelsWidth, 1920);
        assert_eq!(devmode.dmPelsHeight, 1080);
        assert_eq!(devmode.dmDisplayFrequency, 60);
        // dmFields must include all three required field flags;
        // missing any one of them silently makes CDS ignore that field.
        let expected = DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0;
        assert_eq!(devmode.dmFields.0, expected);
    }

    #[test]
    fn encode_device_name_appends_single_nul() {
        let v = encode_device_name(r"\\.\DISPLAY3");
        assert_eq!(v.last(), Some(&0u16));
        assert_eq!(v.len(), r"\\.\DISPLAY3".len() + 1);
        // Decoded prefix matches the input.
        let decoded: String = v[..v.len() - 1]
            .iter()
            .map(|c| char::from_u32(*c as u32).unwrap_or('?'))
            .collect();
        assert_eq!(decoded, r"\\.\DISPLAY3");
    }

    #[test]
    fn devmode_field_flag_constants_are_disjoint() {
        // Catch a regression where DM_PELSWIDTH / HEIGHT / FREQ bits
        // would overlap (which would make our OR clobber a flag).
        let combined = DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0;
        assert_eq!(
            combined,
            DM_PELSWIDTH.0 + DM_PELSHEIGHT.0 + DM_DISPLAYFREQUENCY.0
        );
    }
}
