//! GDI-layer monitor enumeration shared by every capture backend that
//! must select a target by GDI device name (`\\.\DISPLAYn`).
//!
//! ## Why this exists
//!
//! The DXGI enumerator in `dxgi_capture::enumerate_all_outputs` walks
//! `IDXGIFactory1::EnumAdapters1 → IDXGIAdapter1::EnumOutputs`, which
//! is the wrong layer for picking up Indirect Display Driver (IDD)
//! virtual monitors. The OS does not allocate a dedicated
//! `IDXGIAdapter` for an IDD device, so its monitor never appears on
//! the DXGI output chain — yet it is fully visible to GDI (it has its
//! own HMONITOR, its own `\\.\DISPLAYn` name, and its own composed
//! desktop that WGC can capture via `CreateForMonitor`). PoC spike B
//! (see `pocs/poc-indirect-display/src/wgc.rs` and the archive at
//! `agent_works/workspace/2026-05-18_virtual-display-bug2-spike.md`)
//! confirmed end-to-end that `EnumDisplayMonitors` + WGC
//! `CreateForMonitor(HMONITOR)` captures the IDD's independent
//! desktop. This module is the production version of spike A.
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
use std::ffi::c_void;

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
/// collect one [`MonitorEntry`] per monitor. Includes IDD virtual
/// monitors that DXGI never enumerates.
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
pub fn enum_display_infos() -> Result<Vec<DisplayInfo>, CaptureError> {
    let entries = enum_monitors()?;
    let infos = entries
        .into_iter()
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

// Silence the unused-import warning on non-test builds while the
// pointer round-trip is exercised only from the test module.
#[cfg(test)]
const _: () = {
    let _ = size_of::<*mut c_void>();
};

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
}
