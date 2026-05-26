//! `ChangeDisplaySettingsExW` glue — committing the negotiated mode into
//! the user session.
//!
//! The driver-side `IddCxMonitorUpdateModes` only *advertises* a new mode
//! list to the OS; the desktop window manager keeps using the previously
//! selected mode until the user (or some user-session caller) invokes
//! `ChangeDisplaySettingsExW`. The worker runs in the user's session, so
//! this is the natural place to issue the call.

use windows::Win32::Graphics::Gdi::{
    CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODE_FIELD_FLAGS, DEVMODEW,
    DISP_CHANGE, DISP_CHANGE_BADMODE, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
    DM_PELSWIDTH,
};
use windows::core::PCWSTR;

use crate::{VirtualDisplayError, VirtualDisplayMode};

pub(crate) fn encode_device_name(name: &str) -> Vec<u16> {
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
    let devmode = build_devmode(mode);
    let context = format!("{device_name} @ {}x{}@{}", mode.width, mode.height, mode.refresh_hz);
    apply_cds_with_flags(Some(device_name), Some(&devmode), CDS_UPDATEREGISTRY, &context)
}

/// Lower-level CDS commit. `device_name = None` + `devmode = None`
/// performs the "apply pending changes" call (after a batch of
/// `CDS_NORESET` ops). `flags` controls persistence and side-effects:
///
/// - `CDS_UPDATEREGISTRY` → write through to the registry; the change
///   survives logoff/restart.
/// - `CDS_NORESET` → queue the change without committing; pair with a
///   subsequent call with `flags = CDS_TYPE(0)` and both args `None`
///   to commit the batch.
/// - `CDS_SET_PRIMARY` → also make this display the primary monitor.
///
/// The `context` string is only used to build the error message; it
/// has no semantic meaning to the OS.
pub fn apply_cds_with_flags(
    device_name: Option<&str>,
    devmode: Option<&DEVMODEW>,
    flags: CDS_TYPE,
    context: &str,
) -> Result<(), VirtualDisplayError> {
    let wide = device_name.map(encode_device_name);
    let pcwstr = wide
        .as_ref()
        .map_or(PCWSTR::null(), |buf| PCWSTR(buf.as_ptr()));
    let devmode_ptr = devmode.map(|d| d as *const DEVMODEW);
    // SAFETY: PCWSTR is null or points at the locally owned `wide`
    // buffer which outlives the call; devmode_ptr (if Some) is borrowed
    // from the caller for the duration of the call (lifetime tied to
    // the borrow that produced the &DEVMODEW).
    let result: DISP_CHANGE =
        unsafe { ChangeDisplaySettingsExW(pcwstr, devmode_ptr, None, flags, None) };
    if result == DISP_CHANGE_SUCCESSFUL {
        return Ok(());
    }
    let msg = if result == DISP_CHANGE_BADMODE {
        format!("BADMODE for {context}; driver did not advertise this mode")
    } else {
        format!("DISP_CHANGE code {} for {context}", result.0)
    };
    Err(VirtualDisplayError::Cds(msg))
}

/// Commit any queued `CDS_NORESET` operations. Equivalent to
/// `ChangeDisplaySettingsEx(NULL, NULL, NULL, 0, NULL)` per MSDN.
pub fn commit_pending_changes() -> Result<(), VirtualDisplayError> {
    apply_cds_with_flags(None, None, CDS_TYPE(0), "(commit batch)")
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
