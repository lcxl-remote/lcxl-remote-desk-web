//! Shared Linux X11 display backend.
//!
//! Centralizes the X11 / RandR / DPMS plumbing used by two callers:
//! - `host_control::linux_host_control` for `change_display_settings`
//!   and `control_monitor_power`.
//! - `display_watcher::linux` for the RandR display-change watcher.
//!
//! ## Index alignment with the capture engine
//!
//! The browser addresses displays by `device_name` strings of the form
//! `"X11 Display {index}"`, produced by the capture engine
//! (`desk-capture-engine`'s `x11_capture::get_displays`). That index is
//! the position within the `GetScreenResourcesCurrent.crtcs` list, with
//! one entry pushed per CRTC whose `GetCrtcInfo` request succeeds. This
//! module's [`enumerate`] mirrors that iteration order and push rule
//! exactly so a `device_name` selected against the capture list resolves
//! to the same physical CRTC here.
//!
//! ## Real X11 calls behind a trait
//!
//! All raw RandR / DPMS requests go through [`X11DisplayOps`] so the
//! enumeration, mode-matching, and stale-retry orchestration can be unit
//! tested with a fake backend (no live X server).

use std::collections::HashMap;

use desk_utils::error::DeskErrorCode;
pub use desk_utils::linux_display::LinuxDisplayServer as Backend;
use desk_utils::linux_display::detect_linux_display_environment;
use x11rb::connection::Connection;
use x11rb::protocol::dpms::{ConnectionExt as _, DPMSMode};
use x11rb::protocol::randr::{
    self, ConnectionExt as _, Crtc, Mode, ModeFlag, Output, Rotation, SetConfig,
};
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;

use crate::error::InputError;

/// uinput virtual-device name used by the keyboard injection backend.
/// Shared so the local-input grabber can skip the device we inject
/// through (see `host_control::input_grab`).
pub const UINPUT_KEYBOARD_DEVICE_NAME: &str = "lcxl-web-remote-desk-keyboard";
/// uinput virtual-device name used by the mouse injection backend.
pub const UINPUT_MOUSE_DEVICE_NAME: &str = "lcxl-web-remote-desk-mouse";

/// Detect the active display backend from one coherent environment snapshot.
pub fn detect_backend() -> Backend {
    detect_linux_display_environment().active_server()
}

/// Synthetic device name for the X11 display at `index`, matching the
/// capture engine's naming so the wire `device_name` is interchangeable.
pub fn x11_device_name(index: usize) -> String {
    format!("X11 Display {index}")
}

fn parse_x11_device_name(name: &str) -> Option<usize> {
    name.strip_prefix("X11 Display ")
        .and_then(|rest| rest.trim().parse().ok())
}

/// A display mode resolved against the global RandR mode database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    /// RandR mode id (globally unique within a screen-resources reply).
    pub id: Mode,
    pub width: u16,
    pub height: u16,
    /// Refresh rate in millihertz, kept at sub-Hz precision so nearby
    /// rates (e.g. 59.94 vs 60.00) can be compared without rounding.
    pub rate_mhz: u32,
}

/// One CRTC entry, carrying everything needed to set a new mode while
/// preserving the rest of the configuration.
#[derive(Debug, Clone)]
pub struct X11Crtc {
    /// Position in the capture-aligned CRTC iteration order.
    pub index: usize,
    pub crtc: Crtc,
    pub x: i16,
    pub y: i16,
    pub rotation: Rotation,
    /// Outputs currently driven by this CRTC. Preserved verbatim on
    /// `SetCrtcConfig` so a mode change does not detach them.
    pub outputs: Vec<Output>,
    /// Currently active mode (used to keep the refresh rate when the
    /// caller does not specify one).
    pub current_mode: Mode,
    /// Name of the first attached output, if any.
    pub name: Option<String>,
    /// Modes settable on this CRTC: the intersection of the mode lists
    /// of all attached outputs (a clone/mirror configuration can only
    /// use a mode every attached output supports).
    pub modes: Vec<DisplayMode>,
}

/// A coherent snapshot of the screen configuration.
#[derive(Debug, Clone)]
pub struct DisplaySnapshot {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub crtcs: Vec<X11Crtc>,
}

/// Raw `GetScreenResourcesCurrent` data.
pub struct ScreenResources {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub crtcs: Vec<Crtc>,
    pub modes: Vec<randr::ModeInfo>,
}

/// Raw `GetCrtcInfo` data.
pub struct CrtcInfo {
    pub status: SetConfig,
    pub x: i16,
    pub y: i16,
    pub mode: Mode,
    pub rotation: Rotation,
    pub outputs: Vec<Output>,
}

/// Raw `GetOutputInfo` data.
pub struct OutputInfo {
    pub status: SetConfig,
    pub name: String,
    pub modes: Vec<Mode>,
}

/// Seam over the raw RandR / DPMS requests, so the higher-level logic is
/// testable with a fake backend.
pub trait X11DisplayOps {
    fn get_screen_resources(&self) -> Result<ScreenResources, InputError>;
    fn get_crtc_info(&self, crtc: Crtc, config_timestamp: u32) -> Result<CrtcInfo, InputError>;
    fn get_output_info(
        &self,
        output: Output,
        config_timestamp: u32,
    ) -> Result<OutputInfo, InputError>;
    #[allow(clippy::too_many_arguments)]
    fn set_crtc_config(
        &self,
        crtc: Crtc,
        timestamp: u32,
        config_timestamp: u32,
        x: i16,
        y: i16,
        mode: Mode,
        rotation: Rotation,
        outputs: &[Output],
    ) -> Result<SetConfig, InputError>;
    /// Force DPMS on (`on = true`) or off. Enables DPMS first so the
    /// forced level takes effect (mirrors `xset dpms force`).
    fn set_dpms(&self, on: bool) -> Result<(), InputError>;
}

/// Production [`X11DisplayOps`] backed by a live X11 connection.
pub struct RealX11Ops {
    conn: RustConnection,
    root: Window,
}

impl RealX11Ops {
    pub fn new() -> Result<Self, InputError> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        Ok(Self { conn, root })
    }
}

impl X11DisplayOps for RealX11Ops {
    fn get_screen_resources(&self) -> Result<ScreenResources, InputError> {
        // Prefer the cached "current" resources; fall back to the
        // active probe on older servers (mirrors the capture engine).
        let reply = match self.conn.randr_get_screen_resources_current(self.root) {
            Ok(cookie) => {
                let r = cookie.reply()?;
                ScreenResources {
                    timestamp: r.timestamp,
                    config_timestamp: r.config_timestamp,
                    crtcs: r.crtcs,
                    modes: r.modes,
                }
            }
            Err(_) => {
                let r = self.conn.randr_get_screen_resources(self.root)?.reply()?;
                ScreenResources {
                    timestamp: r.timestamp,
                    config_timestamp: r.config_timestamp,
                    crtcs: r.crtcs,
                    modes: r.modes,
                }
            }
        };
        Ok(reply)
    }

    fn get_crtc_info(&self, crtc: Crtc, config_timestamp: u32) -> Result<CrtcInfo, InputError> {
        let r = self
            .conn
            .randr_get_crtc_info(crtc, config_timestamp)?
            .reply()?;
        Ok(CrtcInfo {
            status: r.status,
            x: r.x,
            y: r.y,
            mode: r.mode,
            rotation: r.rotation,
            outputs: r.outputs,
        })
    }

    fn get_output_info(
        &self,
        output: Output,
        config_timestamp: u32,
    ) -> Result<OutputInfo, InputError> {
        let r = self
            .conn
            .randr_get_output_info(output, config_timestamp)?
            .reply()?;
        Ok(OutputInfo {
            status: r.status,
            name: String::from_utf8_lossy(&r.name).into_owned(),
            modes: r.modes,
        })
    }

    fn set_crtc_config(
        &self,
        crtc: Crtc,
        timestamp: u32,
        config_timestamp: u32,
        x: i16,
        y: i16,
        mode: Mode,
        rotation: Rotation,
        outputs: &[Output],
    ) -> Result<SetConfig, InputError> {
        let r = self
            .conn
            .randr_set_crtc_config(
                crtc,
                timestamp,
                config_timestamp,
                x,
                y,
                mode,
                rotation,
                outputs,
            )?
            .reply()?;
        Ok(r.status)
    }

    fn set_dpms(&self, on: bool) -> Result<(), InputError> {
        // `dpms_enable` / `dpms_force_level` are void requests; consume
        // the cookie via `.check()` so an X error surfaces instead of
        // being silently dropped.
        self.conn.dpms_enable()?.check()?;
        let level = if on { DPMSMode::ON } else { DPMSMode::OFF };
        self.conn.dpms_force_level(level)?.check()?;
        Ok(())
    }
}

/// Refresh rate of a RandR mode in millihertz, applying the
/// double-scan / interlace corrections to the vertical total.
fn mode_rate_mhz(m: &randr::ModeInfo) -> u32 {
    if m.htotal == 0 || m.vtotal == 0 {
        return 0;
    }
    let mut vtotal = m.vtotal as u64;
    let flags = u32::from(m.mode_flags);
    if flags & u32::from(ModeFlag::DOUBLE_SCAN) != 0 {
        vtotal *= 2;
    }
    if flags & u32::from(ModeFlag::INTERLACE) != 0 {
        vtotal /= 2;
    }
    let denom = m.htotal as u64 * vtotal;
    if denom == 0 {
        return 0;
    }
    ((m.dot_clock as u64 * 1000) / denom) as u32
}

/// Modes present in both lists, compared by RandR mode id (mode ids are
/// global within a screen, so an id common to two outputs is settable on
/// either).
fn intersect_modes(a: &[DisplayMode], b: &[DisplayMode]) -> Vec<DisplayMode> {
    a.iter()
        .filter(|m| b.iter().any(|n| n.id == m.id))
        .copied()
        .collect()
}

/// Build a configuration snapshot, mirroring the capture engine's CRTC
/// iteration order and push rule. Returns `Ok(None)` when a CRTC or
/// output reports `INVALID_CONFIG_TIME` (the configuration changed
/// mid-enumeration) so the caller can re-enumerate once.
fn enumerate(ops: &dyn X11DisplayOps) -> Result<Option<DisplaySnapshot>, InputError> {
    let res = ops.get_screen_resources()?;
    let mode_db: HashMap<Mode, &randr::ModeInfo> = res.modes.iter().map(|m| (m.id, m)).collect();

    let mut crtcs = Vec::with_capacity(res.crtcs.len());
    for (index, &crtc) in res.crtcs.iter().enumerate() {
        let info = ops.get_crtc_info(crtc, 0)?;
        if info.status == SetConfig::INVALID_CONFIG_TIME {
            return Ok(None);
        }

        let mut name: Option<String> = None;
        let mut modes: Option<Vec<DisplayMode>> = None;
        for &output in &info.outputs {
            let oi = ops.get_output_info(output, res.config_timestamp)?;
            if oi.status == SetConfig::INVALID_CONFIG_TIME {
                return Ok(None);
            }
            if name.is_none() && !oi.name.is_empty() {
                name = Some(oi.name);
            }
            let out_modes: Vec<DisplayMode> = oi
                .modes
                .iter()
                .filter_map(|mid| mode_db.get(mid))
                .map(|m| DisplayMode {
                    id: m.id,
                    width: m.width,
                    height: m.height,
                    rate_mhz: mode_rate_mhz(m),
                })
                .collect();
            modes = Some(match modes {
                None => out_modes,
                Some(existing) => intersect_modes(&existing, &out_modes),
            });
        }

        crtcs.push(X11Crtc {
            index,
            crtc,
            x: info.x,
            y: info.y,
            rotation: info.rotation,
            outputs: info.outputs,
            current_mode: info.mode,
            name,
            modes: modes.unwrap_or_default(),
        });
    }

    Ok(Some(DisplaySnapshot {
        timestamp: res.timestamp,
        config_timestamp: res.config_timestamp,
        crtcs,
    }))
}

/// Enumerate, retrying once if the configuration changed mid-scan.
fn enumerate_with_retry(ops: &dyn X11DisplayOps) -> Result<DisplaySnapshot, InputError> {
    if let Some(snap) = enumerate(ops)? {
        return Ok(snap);
    }
    if let Some(snap) = enumerate(ops)? {
        return Ok(snap);
    }
    InputError::custom_error(
        DeskErrorCode::SYSTEM_ERROR,
        "X11 RandR configuration kept changing during enumeration",
    )
}

/// Resolve a `device_name` to its CRTC. Unknown / unparseable / out of
/// range names, and CRTCs with no settable mode, are hard errors with
/// the available device list — there is no silent fallback to a default
/// display (matches the capture engine's selection policy).
pub fn select_crtc<'a>(crtcs: &'a [X11Crtc], device_name: &str) -> Result<&'a X11Crtc, InputError> {
    let available = || {
        crtcs
            .iter()
            .map(|c| x11_device_name(c.index))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let idx = parse_x11_device_name(device_name).ok_or_else(|| {
        InputError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            &format!(
                "unrecognized X11 device_name '{device_name}'; available: [{}]",
                available()
            ),
        )
    })?;

    let crtc = crtcs.get(idx).ok_or_else(|| {
        InputError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            &format!(
                "X11 display index {idx} out of range; available: [{}]",
                available()
            ),
        )
    })?;

    if crtc.modes.is_empty() {
        return InputError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            &format!("X11 display {idx} has no settable modes (disabled or no attached output)"),
        );
    }

    Ok(crtc)
}

/// Pick the mode id matching `width`x`height` whose refresh rate is
/// closest to the request. `frequency = None` keeps the CRTC's current
/// refresh rate (falling back to the highest available at that
/// resolution when the current rate is unknown). Returns `None` when no
/// mode matches the requested resolution.
pub fn match_mode(crtc: &X11Crtc, width: u32, height: u32, frequency: Option<u32>) -> Option<Mode> {
    let candidates: Vec<&DisplayMode> = crtc
        .modes
        .iter()
        .filter(|m| m.width as u32 == width && m.height as u32 == height)
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let target_mhz = match frequency {
        Some(f) => f * 1000,
        None => crtc
            .modes
            .iter()
            .find(|m| m.id == crtc.current_mode)
            .map(|m| m.rate_mhz)
            .unwrap_or(0),
    };

    let best = if target_mhz == 0 {
        candidates.iter().max_by_key(|m| m.rate_mhz)
    } else {
        candidates
            .iter()
            .min_by_key(|m| (i64::from(m.rate_mhz) - i64::from(target_mhz)).unsigned_abs())
    };
    best.map(|m| m.id)
}

fn set_status_error(status: SetConfig) -> InputError {
    InputError::new_custom_error(
        DeskErrorCode::SYSTEM_ERROR,
        &format!("X11 SetCrtcConfig failed with status {:?}", status),
    )
}

fn try_apply(
    ops: &dyn X11DisplayOps,
    snap: &DisplaySnapshot,
    device_name: &str,
    width: u32,
    height: u32,
    frequency: Option<u32>,
) -> Result<SetConfig, InputError> {
    let crtc = select_crtc(&snap.crtcs, device_name)?;
    let mode = match_mode(crtc, width, height, frequency).ok_or_else(|| {
        let modes = crtc
            .modes
            .iter()
            .map(|m| format!("{}x{}@{}", m.width, m.height, m.rate_mhz / 1000))
            .collect::<Vec<_>>()
            .join(", ");
        InputError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            &format!(
                "no X11 mode matches {width}x{height}@{frequency:?} on '{device_name}'; available: [{modes}]"
            ),
        )
    })?;
    ops.set_crtc_config(
        crtc.crtc,
        snap.timestamp,
        snap.config_timestamp,
        crtc.x,
        crtc.y,
        mode,
        crtc.rotation,
        &crtc.outputs,
    )
}

/// Change the mode of the display addressed by `device_name`, preserving
/// its position, rotation, and attached outputs. Re-enumerates and
/// retries once if the configuration changes between snapshot and set.
pub fn apply_display_settings(
    ops: &dyn X11DisplayOps,
    device_name: &str,
    width: u32,
    height: u32,
    frequency: Option<u32>,
) -> Result<(), InputError> {
    let snap = enumerate_with_retry(ops)?;
    let status = try_apply(ops, &snap, device_name, width, height, frequency)?;
    if status == SetConfig::SUCCESS {
        return Ok(());
    }
    if status == SetConfig::INVALID_CONFIG_TIME {
        let snap = enumerate_with_retry(ops)?;
        let status = try_apply(ops, &snap, device_name, width, height, frequency)?;
        if status == SetConfig::SUCCESS {
            return Ok(());
        }
        return Err(set_status_error(status));
    }
    Err(set_status_error(status))
}

/// Turn the monitor(s) off (`turn_off = true`) or back on via DPMS.
pub fn control_monitor_power(ops: &dyn X11DisplayOps, turn_off: bool) -> Result<(), InputError> {
    ops.set_dpms(!turn_off)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn dm(id: Mode, w: u16, h: u16, rate_hz: u32) -> DisplayMode {
        DisplayMode {
            id,
            width: w,
            height: h,
            rate_mhz: rate_hz * 1000,
        }
    }

    fn crtc_with(index: usize, current_mode: Mode, modes: Vec<DisplayMode>) -> X11Crtc {
        X11Crtc {
            index,
            crtc: 100 + index as u32,
            x: 0,
            y: 0,
            rotation: Rotation::from(1u16),
            outputs: vec![200 + index as u32],
            current_mode,
            name: Some(format!("OUT-{index}")),
            modes,
        }
    }

    // ----- select_crtc -----

    #[test]
    fn select_crtc_hits_nth_entry() {
        let crtcs = vec![
            crtc_with(0, 1, vec![dm(1, 1920, 1080, 60)]),
            crtc_with(1, 2, vec![dm(2, 2560, 1440, 60)]),
        ];
        let c = select_crtc(&crtcs, "X11 Display 1").expect("index 1 exists");
        assert_eq!(c.index, 1);
    }

    #[test]
    fn select_crtc_rejects_unparseable_name() {
        let crtcs = vec![crtc_with(0, 1, vec![dm(1, 1920, 1080, 60)])];
        let err = select_crtc(&crtcs, "eDP-1").unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn select_crtc_rejects_empty_name() {
        let crtcs = vec![crtc_with(0, 1, vec![dm(1, 1920, 1080, 60)])];
        let err = select_crtc(&crtcs, "").unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn select_crtc_rejects_out_of_range_index() {
        let crtcs = vec![crtc_with(0, 1, vec![dm(1, 1920, 1080, 60)])];
        let err = select_crtc(&crtcs, "X11 Display 5").unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn select_crtc_rejects_crtc_without_modes() {
        // A disabled CRTC stays in the vector (index alignment) but is
        // not settable.
        let crtcs = vec![crtc_with(0, 0, vec![])];
        let err = select_crtc(&crtcs, "X11 Display 0").unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::INVALID_PARAMS);
    }

    // ----- intersect_modes -----

    #[test]
    fn intersect_modes_keeps_only_common_ids() {
        let a = vec![dm(1, 1920, 1080, 60), dm(2, 1280, 720, 60)];
        let b = vec![dm(2, 1280, 720, 60), dm(3, 800, 600, 60)];
        let inter = intersect_modes(&a, &b);
        assert_eq!(inter.len(), 1);
        assert_eq!(inter[0].id, 2);
    }

    #[test]
    fn intersect_modes_empty_when_disjoint() {
        let a = vec![dm(1, 1920, 1080, 60)];
        let b = vec![dm(2, 1280, 720, 60)];
        assert!(intersect_modes(&a, &b).is_empty());
    }

    // ----- match_mode -----

    #[test]
    fn match_mode_picks_closest_frequency() {
        let crtc = crtc_with(
            0,
            10,
            vec![
                dm(10, 1920, 1080, 60),
                dm(11, 1920, 1080, 144),
                dm(12, 1920, 1080, 120),
            ],
        );
        let id = match_mode(&crtc, 1920, 1080, Some(119)).expect("a 1080p mode");
        assert_eq!(id, 12, "119 Hz should snap to the 120 Hz mode");
    }

    #[test]
    fn match_mode_none_keeps_current_rate() {
        let crtc = crtc_with(
            0,
            11, // current is the 144 Hz mode
            vec![
                dm(10, 1920, 1080, 60),
                dm(11, 1920, 1080, 144),
                dm(12, 1920, 1080, 120),
            ],
        );
        let id = match_mode(&crtc, 1920, 1080, None).expect("a 1080p mode");
        assert_eq!(id, 11, "None should preserve the current 144 Hz rate");
    }

    #[test]
    fn match_mode_none_unknown_current_picks_highest() {
        let crtc = crtc_with(
            0,
            999, // current mode not present in the list
            vec![dm(10, 1920, 1080, 60), dm(12, 1920, 1080, 120)],
        );
        let id = match_mode(&crtc, 1920, 1080, None).expect("a 1080p mode");
        assert_eq!(id, 12, "unknown current rate falls back to the highest");
    }

    #[test]
    fn match_mode_returns_none_for_unknown_resolution() {
        let crtc = crtc_with(0, 10, vec![dm(10, 1920, 1080, 60)]);
        assert!(match_mode(&crtc, 3840, 2160, Some(60)).is_none());
    }

    // ----- fake ops + orchestration -----

    #[derive(Clone)]
    struct SetCall {
        crtc: Crtc,
        timestamp: u32,
        config_timestamp: u32,
        x: i16,
        y: i16,
        mode: Mode,
        rotation: Rotation,
        outputs: Vec<Output>,
    }

    struct FakeOps {
        // One ScreenResources per enumerate() call (popped front to back).
        resources: RefCell<Vec<ScreenResources>>,
        crtc_infos: HashMap<Crtc, CrtcInfo>,
        output_infos: HashMap<Output, OutputInfo>,
        // SetConfig statuses returned per set_crtc_config call, in order.
        set_statuses: RefCell<Vec<SetConfig>>,
        set_calls: RefCell<Vec<SetCall>>,
        dpms_calls: RefCell<Vec<bool>>,
    }

    impl X11DisplayOps for FakeOps {
        fn get_screen_resources(&self) -> Result<ScreenResources, InputError> {
            let mut r = self.resources.borrow_mut();
            if r.len() > 1 {
                Ok(r.remove(0))
            } else {
                // Reuse the last one for any extra enumerations.
                let last = r.last().expect("at least one ScreenResources");
                Ok(ScreenResources {
                    timestamp: last.timestamp,
                    config_timestamp: last.config_timestamp,
                    crtcs: last.crtcs.clone(),
                    modes: last.modes.clone(),
                })
            }
        }

        fn get_crtc_info(
            &self,
            crtc: Crtc,
            _config_timestamp: u32,
        ) -> Result<CrtcInfo, InputError> {
            let i = self.crtc_infos.get(&crtc).expect("known crtc");
            Ok(CrtcInfo {
                status: i.status,
                x: i.x,
                y: i.y,
                mode: i.mode,
                rotation: i.rotation,
                outputs: i.outputs.clone(),
            })
        }

        fn get_output_info(
            &self,
            output: Output,
            _config_timestamp: u32,
        ) -> Result<OutputInfo, InputError> {
            let o = self.output_infos.get(&output).expect("known output");
            Ok(OutputInfo {
                status: o.status,
                name: o.name.clone(),
                modes: o.modes.clone(),
            })
        }

        fn set_crtc_config(
            &self,
            crtc: Crtc,
            timestamp: u32,
            config_timestamp: u32,
            x: i16,
            y: i16,
            mode: Mode,
            rotation: Rotation,
            outputs: &[Output],
        ) -> Result<SetConfig, InputError> {
            self.set_calls.borrow_mut().push(SetCall {
                crtc,
                timestamp,
                config_timestamp,
                x,
                y,
                mode,
                rotation,
                outputs: outputs.to_vec(),
            });
            let mut s = self.set_statuses.borrow_mut();
            Ok(if s.is_empty() {
                SetConfig::SUCCESS
            } else {
                s.remove(0)
            })
        }

        fn set_dpms(&self, on: bool) -> Result<(), InputError> {
            self.dpms_calls.borrow_mut().push(on);
            Ok(())
        }
    }

    fn modeinfo(id: Mode, w: u16, h: u16, rate_hz: u16) -> randr::ModeInfo {
        // dot_clock = rate * htotal * vtotal, with htotal=w, vtotal=h so
        // mode_rate_mhz recovers `rate_hz`.
        randr::ModeInfo {
            id,
            width: w,
            height: h,
            dot_clock: rate_hz as u32 * w as u32 * h as u32,
            hsync_start: 0,
            hsync_end: 0,
            htotal: w,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: h,
            name_len: 0,
            mode_flags: ModeFlag::from(0u32),
        }
    }

    fn single_display_ops(set_statuses: Vec<SetConfig>) -> FakeOps {
        let modes = vec![modeinfo(10, 1920, 1080, 60), modeinfo(11, 1920, 1080, 120)];
        let resources = ScreenResources {
            timestamp: 555,
            config_timestamp: 777,
            crtcs: vec![100],
            modes,
        };
        let mut crtc_infos = HashMap::new();
        crtc_infos.insert(
            100,
            CrtcInfo {
                status: SetConfig::SUCCESS,
                x: 11,
                y: 22,
                mode: 10,
                rotation: Rotation::from(1u16),
                outputs: vec![200],
            },
        );
        let mut output_infos = HashMap::new();
        output_infos.insert(
            200,
            OutputInfo {
                status: SetConfig::SUCCESS,
                name: "OUT-0".to_string(),
                modes: vec![10, 11],
            },
        );
        FakeOps {
            resources: RefCell::new(vec![resources]),
            crtc_infos,
            output_infos,
            set_statuses: RefCell::new(set_statuses),
            set_calls: RefCell::new(vec![]),
            dpms_calls: RefCell::new(vec![]),
        }
    }

    #[test]
    fn apply_preserves_position_rotation_outputs_and_timestamps() {
        let ops = single_display_ops(vec![SetConfig::SUCCESS]);
        apply_display_settings(&ops, "X11 Display 0", 1920, 1080, Some(120)).expect("apply ok");
        let calls = ops.set_calls.borrow();
        assert_eq!(calls.len(), 1);
        let c = &calls[0];
        assert_eq!(c.crtc, 100);
        assert_eq!(c.mode, 11, "120 Hz mode id");
        assert_eq!((c.x, c.y), (11, 22), "position preserved");
        assert_eq!(c.rotation, Rotation::from(1u16), "rotation preserved");
        assert_eq!(c.outputs, vec![200], "outputs preserved");
        assert_eq!(c.timestamp, 555);
        assert_eq!(c.config_timestamp, 777);
    }

    #[test]
    fn apply_retries_once_on_set_stale_config_time() {
        // First set returns stale, second succeeds.
        let ops = single_display_ops(vec![SetConfig::INVALID_CONFIG_TIME, SetConfig::SUCCESS]);
        apply_display_settings(&ops, "X11 Display 0", 1920, 1080, Some(60)).expect("retry ok");
        assert_eq!(ops.set_calls.borrow().len(), 2, "one retry after stale set");
    }

    #[test]
    fn apply_gives_up_after_second_set_failure() {
        let ops = single_display_ops(vec![
            SetConfig::INVALID_CONFIG_TIME,
            SetConfig::INVALID_CONFIG_TIME,
        ]);
        let err = apply_display_settings(&ops, "X11 Display 0", 1920, 1080, Some(60)).unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::SYSTEM_ERROR);
    }

    #[test]
    fn enumerate_retries_once_when_crtc_info_stale() {
        // First enumerate: crtc reports stale. Second: success.
        let modes = vec![modeinfo(10, 1920, 1080, 60)];
        let res_stale = ScreenResources {
            timestamp: 1,
            config_timestamp: 2,
            crtcs: vec![100],
            modes: modes.clone(),
        };
        let res_ok = ScreenResources {
            timestamp: 3,
            config_timestamp: 4,
            crtcs: vec![100],
            modes,
        };
        let mut crtc_infos = HashMap::new();
        // The fake returns the same crtc info for both passes; emulate
        // staleness by toggling via a dedicated stale crtc id is awkward,
        // so model it through the resources list instead: a stale first
        // pass is represented by a crtc whose status is stale.
        crtc_infos.insert(
            100,
            CrtcInfo {
                status: SetConfig::SUCCESS,
                x: 0,
                y: 0,
                mode: 10,
                rotation: Rotation::from(1u16),
                outputs: vec![200],
            },
        );
        let mut output_infos = HashMap::new();
        output_infos.insert(
            200,
            OutputInfo {
                status: SetConfig::SUCCESS,
                name: "OUT-0".to_string(),
                modes: vec![10],
            },
        );
        // Use a stale-crtc on the first resources pass.
        crtc_infos.insert(
            101,
            CrtcInfo {
                status: SetConfig::INVALID_CONFIG_TIME,
                x: 0,
                y: 0,
                mode: 0,
                rotation: Rotation::from(1u16),
                outputs: vec![],
            },
        );
        let res_stale = ScreenResources {
            crtcs: vec![101],
            ..res_stale
        };
        let ops = FakeOps {
            resources: RefCell::new(vec![res_stale, res_ok]),
            crtc_infos,
            output_infos,
            set_statuses: RefCell::new(vec![SetConfig::SUCCESS]),
            set_calls: RefCell::new(vec![]),
            dpms_calls: RefCell::new(vec![]),
        };
        apply_display_settings(&ops, "X11 Display 0", 1920, 1080, Some(60))
            .expect("second enumerate succeeds");
        assert_eq!(
            ops.set_calls.borrow()[0].config_timestamp,
            4,
            "used fresh snapshot"
        );
    }

    #[test]
    fn control_monitor_power_maps_to_dpms() {
        let ops = single_display_ops(vec![]);
        control_monitor_power(&ops, true).expect("dpms off");
        control_monitor_power(&ops, false).expect("dpms on");
        assert_eq!(*ops.dpms_calls.borrow(), vec![false, true]);
    }

    #[test]
    fn mode_rate_mhz_handles_interlace_and_doublescan() {
        let mut progressive = modeinfo(1, 1920, 1080, 60);
        assert_eq!(mode_rate_mhz(&progressive), 60_000);
        // Interlace halves vtotal -> doubles the recovered rate.
        progressive.mode_flags = ModeFlag::INTERLACE;
        assert_eq!(mode_rate_mhz(&progressive), 120_000);
    }
}
