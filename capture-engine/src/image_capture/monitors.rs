//! GDI-layer monitor enumeration shared by every capture backend that
//! must address a target through a GDI device name (`\\.\DISPLAYn`)
//! or a raw `HMONITOR`.
//!
//! ## Why this exists
//!
//! WGC capture binds via
//! `IGraphicsCaptureItemInterop::CreateForMonitor(HMONITOR)`, and
//! `HMONITOR` is a GDI-layer handle — the natural enumeration source
//! is `EnumDisplayMonitors`, not `IDXGIAdapter::EnumOutputs`. DXGI
//! also enumerates the same monitors (including IDD virtual displays
//! attached through `LcxlVirtualDisplay`, which register a virtual
//! `IDXGIAdapter`), but DXGI hands back `IDXGIOutput`, not
//! `HMONITOR`, so WGC needs its own enumerator regardless.
//!
//! PoC spike B confirmed end-to-end that `EnumDisplayMonitors` + WGC
//! `CreateForMonitor(HMONITOR)` captures the IDD's independent
//! desktop. The original spike's "DXGI cannot see IDD" claim was
//! later corrected: DXGI does enumerate IDD outputs, but hands back
//! `IDXGIOutput` rather than `HMONITOR`, so WGC still needs its own
//! GDI-layer enumerator. This module is the production version of
//! spike A.
//!
//! ## Layering
//!
//! [`enum_monitors`] is the raw enumeration. [`enum_display_infos`]
//! upgrades the raw entries to facade-level [`DisplayInfo`] by
//! cross-referencing `EnumDisplayDevicesW` (for the friendly display
//! string) and `enum_display_resolutions` (for the per-device mode
//! list). Capture backends consume the rich form; the WGC capture
//! instance constructor reaches back for the raw HMONITOR via
//! [`find_monitor_by_device_name`].

use core::mem::size_of;

use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
use desk_utils::error::DeskErrorCode;
use windows::Win32::Foundation::{GetLastError, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    DISPLAY_DEVICE_STATE_FLAGS, DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors,
    GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows_core::{BOOL, PCWSTR};

use crate::error::CaptureError;
use crate::image_capture::windows::enum_display_resolutions;

/// One monitor as reported by `EnumDisplayMonitors`. The HMONITOR is
/// stored as `isize` so the struct is trivially `Send` — the underlying
/// raw type is `*mut c_void`, but the value is process-global and only
/// needs to be reconstructed at use sites that already speak DXGI/WGC
/// (see `WgcImageCapture::ensure_pipeline`).
#[derive(Debug, Clone)]
pub struct MonitorEntry {
    /// Raw HMONITOR value cast to `isize`. Reconstruct with
    /// `HMONITOR(value as *mut c_void)` at the call site.
    pub hmonitor_raw: isize,
    /// GDI device name, e.g. `\\.\DISPLAY1`. Stable across capture
    /// invocations as long as the display is not detached.
    pub device_name: String,
    /// Desktop coordinates in virtual screen space.
    pub rect: DisplayRect,
    /// `true` when `MONITORINFOF_PRIMARY` is set on `MONITORINFOEX.dwFlags`.
    pub is_primary: bool,
}

const TRUE: BOOL = BOOL(1);

/// Drive `EnumDisplayMonitors` over the entire virtual desktop and
/// collect one [`MonitorEntry`] per monitor, including IDD virtual
/// monitors. Used by backends (notably WGC) that need raw HMONITORs
/// rather than IDXGIOutput handles.
pub fn enum_monitors() -> Result<Vec<MonitorEntry>, CaptureError> {
    let mut list: Vec<MonitorEntry> = Vec::new();
    let list_ptr: *mut Vec<MonitorEntry> = &mut list;

    unsafe extern "system" fn callback(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        // SAFETY: lparam carries the &mut Vec<MonitorEntry> we passed
        // in from `enum_monitors`. EnumDisplayMonitors invokes the
        // callback synchronously, so the borrow stays live for the
        // duration of every callback invocation.
        let list = unsafe { &mut *(lparam.0 as *mut Vec<MonitorEntry>) };

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        let ok =
            unsafe { GetMonitorInfoW(hmonitor, &mut info.monitorInfo as *mut _ as *mut _) };
        if ok.as_bool() {
            // `szDevice` is a `[u16; 32]` null-terminated GDI device name.
            let nul_pos = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            let device_name = String::from_utf16_lossy(&info.szDevice[..nul_pos]);
            let r = info.monitorInfo.rcMonitor;
            list.push(MonitorEntry {
                hmonitor_raw: hmonitor.0 as isize,
                device_name,
                rect: DisplayRect {
                    left: r.left,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                },
                // MONITORINFOF_PRIMARY = 1
                is_primary: (info.monitorInfo.dwFlags & 1) != 0,
            });
        }
        TRUE
    }

    let ok = unsafe {
        EnumDisplayMonitors(None, None, Some(callback), LPARAM(list_ptr as isize))
    };
    if !ok.as_bool() {
        let code = unsafe { GetLastError().0 };
        return CaptureError::custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("EnumDisplayMonitors returned FALSE; GetLastError={}", code),
        );
    }
    Ok(list)
}

/// Reverse-lookup helper: given a GDI device name (`\\.\DISPLAYn`),
/// return the full [`MonitorEntry`] (so the caller has the HMONITOR).
/// Returns `Ok(None)` when no monitor with that name exists, so the
/// caller can decide whether to error and with what context.
pub fn find_monitor_by_device_name(
    device_name: &str,
) -> Result<Option<MonitorEntry>, CaptureError> {
    let entries = enum_monitors()?;
    Ok(entries.into_iter().find(|e| e.device_name == device_name))
}

/// Pure device-name selection over a [`DisplayInfo`] slice. Shared by
/// every capture backend that selects a target by GDI device name.
/// Empty `requested` is a hard error: the fresh-install "no display
/// selected" state must never silently fall back to a default, so the
/// failure surfaces all the way to the worker / signaling layer. The
/// not-found branch carries the list of enumerated names for triage.
pub fn select_display_info_by_name(
    infos: &[DisplayInfo],
    requested: &str,
) -> Result<DisplayInfo, CaptureError> {
    if requested.is_empty() {
        return CaptureError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "video_device_name is empty: no display has been selected. \
             Open the desktop dialog in the browser and pick a display \
             before starting media.",
        );
    }
    if let Some(found) = infos.iter().find(|i| i.device_name == requested) {
        return Ok(found.clone());
    }
    let summary = if infos.is_empty() {
        "(none)".to_string()
    } else {
        infos
            .iter()
            .map(|i| format!("{:?}", i.device_name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    CaptureError::custom_error(
        DeskErrorCode::INVALID_PARAMS,
        &format!(
            "device_name {:?} not enumerated by capture backend; enumerated: [{}]",
            requested, summary
        ),
    )
}

/// Look up `EnumDisplayDevicesW(device_name, 0)` to get the friendly
/// `DeviceString` (e.g. `"Generic PnP Monitor"`). Returns `None` when
/// the device cannot be resolved — the field is purely informational
/// and a missing value never blocks capture.
fn lookup_display_device_name(device_name: &str) -> Option<String> {
    // Build a null-terminated UTF-16 buffer the Win32 API can read.
    let mut wide: Vec<u16> = device_name.encode_utf16().collect();
    wide.push(0);
    let mut display_device = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        DeviceName: [0u16; 32],
        DeviceString: [0u16; 128],
        StateFlags: DISPLAY_DEVICE_STATE_FLAGS(0),
        DeviceID: [0u16; 128],
        DeviceKey: [0u16; 128],
    };
    let ok = unsafe {
        EnumDisplayDevicesW(
            PCWSTR::from_raw(wide.as_ptr()),
            0,
            &mut display_device,
            0,
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let nul_pos = display_device
        .DeviceString
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(display_device.DeviceString.len());
    Some(String::from_utf16_lossy(
        &display_device.DeviceString[..nul_pos],
    ))
}

/// Upgrade every [`MonitorEntry`] to a [`DisplayInfo`] so capture
/// backend enumerators can return the same struct the frontend
/// already consumes from DXGI. `display_device_name` and `resolutions`
/// are best-effort — failure to fill them never aborts enumeration,
/// since the WGC capture path only needs `device_name` to bind a
/// monitor via `CreateForMonitor`.
///
/// Entries whose device_name does not follow the standard `\\.\DISPLAY`
/// prefix are filtered out: Windows occasionally surfaces phantom
/// devices (e.g. `"WinDisc"`, reported after hot-disconnecting an
/// external monitor on a still-running session) that are not
/// addressable through any capture API. They would only clutter the
/// dropdown and produce confusing errors at selection time.
pub fn enum_display_infos() -> Result<Vec<DisplayInfo>, CaptureError> {
    let entries = enum_monitors()?;
    let infos = entries
        .into_iter()
        .filter(|m| m.device_name.starts_with(r"\\.\DISPLAY"))
        .map(|m| {
            let display_device_name = lookup_display_device_name(&m.device_name);
            let resolutions = enum_display_resolutions(&m.device_name).unwrap_or_default();
            DisplayInfo {
                device_name: m.device_name,
                display_device_name,
                desktop_coordinates: m.rect,
                resolutions,
                // EnumDisplayMonitors only ever yields HMONITORs that
                // are attached to the desktop; if a monitor were
                // detached we would not see it here at all.
                attached_to_desktop: true,
                // Rotation is not directly available from
                // MONITORINFOEX; the consumers that care about it
                // (per-frame transforms inside the DXGI pipeline) use
                // the DXGI_OUTPUT_DESC.Rotation value. For backends
                // that only need to bind a HMONITOR (WGC), zero is
                // the safe identity rotation.
                rotation: 0,
            }
        })
        .collect();
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `enum_monitors` must never panic, including on headless CI
    /// where it may legitimately return zero entries. The
    /// structural-assertion smoke (one physical + one IDD attached)
    /// is run manually via the `poc-indirect-display enum-monitors`
    /// CLI subcommand — see the spike archive.
    #[test]
    fn enum_monitors_does_not_panic() {
        let _ = enum_monitors();
    }

    /// `enum_display_infos` is the layer the WGC enumerator calls;
    /// it must also be panic-safe on headless CI.
    #[test]
    fn enum_display_infos_does_not_panic() {
        let _ = enum_display_infos();
    }

    /// Sanity check on the rect helper — purely arithmetic, runs on
    /// every platform's `cargo test`.
    #[test]
    fn monitor_entry_rect_width_height() {
        let r = DisplayRect {
            left: 100,
            top: 200,
            right: 1380,
            bottom: 1000,
        };
        assert_eq!(r.width(), 1280);
        assert_eq!(r.height(), 800);
    }

    fn make_info(name: &str) -> DisplayInfo {
        DisplayInfo {
            device_name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn select_display_info_by_name_finds_matching_display() {
        let infos = vec![make_info(r"\\.\DISPLAY1"), make_info(r"\\.\DISPLAY7")];
        let chosen = select_display_info_by_name(&infos, r"\\.\DISPLAY7").expect("match");
        assert_eq!(chosen.device_name, r"\\.\DISPLAY7");
    }

    /// Empty `video_device_name` represents the legal-but-unselected
    /// state on fresh installs. The capture-engine must never silently
    /// fall back to the primary monitor on this path — instead, every
    /// backend's `new` surfaces INVALID_PARAMS so the frontend can
    /// prompt the user. The dialog already gates submit on a non-empty
    /// name, so this is purely a defensive guarantee on the lower
    /// layer.
    #[test]
    fn select_display_info_by_name_returns_invalid_params_when_empty_string() {
        let infos = vec![make_info(r"\\.\DISPLAY1")];
        let err = select_display_info_by_name(&infos, "").expect_err("empty must error");
        let msg = format!("{}", err);
        assert!(
            msg.contains("video_device_name is empty"),
            "error must mention the empty selection: {}",
            msg
        );
    }

    #[test]
    fn select_display_info_by_name_returns_invalid_params_when_no_match() {
        let infos = vec![make_info(r"\\.\DISPLAY1"), make_info(r"\\.\DISPLAY7")];
        let err = select_display_info_by_name(&infos, r"\\.\DISPLAY99")
            .expect_err("no match must error");
        let msg = format!("{}", err);
        // The Debug formatter double-escapes backslashes inside the
        // message, so the assertion targets the human-recognisable
        // suffix that is stable across Display / Debug rendering.
        assert!(
            msg.contains("DISPLAY99")
                && msg.contains("DISPLAY1")
                && msg.contains("DISPLAY7"),
            "error must list the requested name and the enumerated list: {}",
            msg
        );
    }
}
