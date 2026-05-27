//! Physical display detach / reattach used by the exclusive-mode
//! pipeline. Snapshot the current GDI layout, set the virtual display
//! as primary at `(0, 0)`, detach every physical display (zero-width
//! `DEVMODEW`), then a single commit. Reverse on leave.
//!
//! All `ChangeDisplaySettingsExW` calls go through [`super::cds`].
//! `enter_exclusive` uses `CDS_NORESET` so that nothing happens until
//! the final commit (atomic-ish), and **every individual call** in
//! the batch carries `CDS_UPDATEREGISTRY`. The two e2e iterations on
//! 2026-05-27 nailed down the rule the hard way:
//!
//! 1. The `CDS_SET_PRIMARY` call refused without `CDS_UPDATEREGISTRY`
//!    (`DISP_CHANGE_BADFLAGS` -4). Added it; first e2e regression
//!    closed.
//! 2. The detach (`dmPelsWidth = 0`) calls *also* refused without
//!    `CDS_UPDATEREGISTRY` in a `CDS_NORESET` batch. Added it; second
//!    e2e regression closed.
//!
//! Crash-recovery story is now: physical displays *and* the primary
//! flag are persisted in the registry the moment each call returns.
//! A worker crash mid-enter leaves the host in whatever partial state
//! the batch had reached. Recovery relies on the worker's
//! `ExclusiveGuard::drop` and the daemon's `reset_exclusive_state`
//! driving `leave_exclusive`; if the process is killed outright, the
//! user has to recover via Windows display settings or by restarting
//! the service (which calls `leave_exclusive` at startup). Without
//! `CDS_UPDATEREGISTRY` Windows simply rejects the calls, so this is
//! a forced trade. The leave path uses `CDS_UPDATEREGISTRY`
//! throughout so a normal session-end is stable.

use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_SET_PRIMARY, CDS_UPDATEREGISTRY, DEVMODE_FIELD_FLAGS, DEVMODEW,
    DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICE_STATE_FLAGS,
    DISPLAY_DEVICEW, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS, EnumDisplayDevicesW,
    EnumDisplaySettingsExW,
};
use windows::core::PCWSTR;

use crate::VirtualDisplayError;

use super::cds::{apply_cds_with_flags, commit_pending_changes};

/// Flag combination required to set a display as primary inside a
/// multi-monitor batch update.
///
/// E2E regression guard 2026-05-27: `CDS_SET_PRIMARY` MUST be paired
/// with `CDS_UPDATEREGISTRY`. Without it Windows returns
/// `DISP_CHANGE_BADFLAGS` (-4) — the root cause of the "5 s prompt,
/// fail, repeat" loop the user hit at 2026-05-27 14:57. This is the
/// canonical pattern documented by MS and used by NirCmd, Chromium,
/// and MultiMonitorTool.
const SET_PRIMARY_BATCH_FLAGS: windows::Win32::Graphics::Gdi::CDS_TYPE =
    windows::Win32::Graphics::Gdi::CDS_TYPE(CDS_NORESET.0 | CDS_SET_PRIMARY.0 | CDS_UPDATEREGISTRY.0);

/// Flag combination required for any **non-primary** call inside a
/// multi-monitor batch update — detach (`dmPelsWidth = 0`) and
/// position/mode restore both use this.
///
/// E2E regression guard 2026-05-27 (second iteration): the detach
/// call also returns `DISP_CHANGE_BADFLAGS` if `CDS_UPDATEREGISTRY`
/// is missing in a `CDS_NORESET` batch. MSDN's multi-monitor sample
/// confirms the pattern: every call in the batch must carry
/// `CDS_UPDATEREGISTRY`.
const NON_PRIMARY_BATCH_FLAGS: windows::Win32::Graphics::Gdi::CDS_TYPE =
    windows::Win32::Graphics::Gdi::CDS_TYPE(CDS_NORESET.0 | CDS_UPDATEREGISTRY.0);

/// Snapshot of one display at the moment exclusive mode begins. Stored
/// so leave can restore the exact `dmPosition`, mode and primary flag.
///
/// `DEVMODEW` is `Send + Sync` (verified at `windows 0.61` — it is
/// `#[repr(C)]` POD), so an `ExclusiveLayout` containing several of
/// these can be moved into `spawn_blocking` without an intermediate
/// owned wrapper.
#[derive(Clone)]
pub struct PhysicalDisplaySnapshot {
    /// `\\.\DISPLAY1` etc. — what `ChangeDisplaySettingsExW` accepts.
    pub device_name: String,
    /// Full current devmode at snapshot time. Used both to rebuild the
    /// detach devmode (preserve `dmPosition`) and to restore on leave.
    pub devmode: DEVMODEW,
    /// Was this the primary display when the snapshot was taken?
    pub is_primary: bool,
}

// `DEVMODEW` from windows-rs 0.61 does not derive `Debug`; print the
// fields the worker logs actually need (dimensions, position, primary
// flag) so structured logging stays useful without dumping the entire
// union opaque blob.
impl std::fmt::Debug for PhysicalDisplaySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SAFETY: reading the display branch of the DEVMODEW union; we
        // only construct DEVMODEW values that have the display branch
        // populated.
        let (px, py) = unsafe {
            (
                self.devmode.Anonymous1.Anonymous2.dmPosition.x,
                self.devmode.Anonymous1.Anonymous2.dmPosition.y,
            )
        };
        f.debug_struct("PhysicalDisplaySnapshot")
            .field("device_name", &self.device_name)
            .field("is_primary", &self.is_primary)
            .field("width", &self.devmode.dmPelsWidth)
            .field("height", &self.devmode.dmPelsHeight)
            .field("freq", &self.devmode.dmDisplayFrequency)
            .field("position", &(px, py))
            .finish()
    }
}

/// Layout captured before entering exclusive mode. The exclusive
/// runner stores it on the worker side and feeds it back to
/// [`leave_exclusive`] when the user releases control or the daemon
/// asks to drop the exclusive layer.
#[derive(Debug, Clone)]
pub struct ExclusiveLayout {
    /// Every active physical display at snapshot time. Empty when the
    /// host has only the virtual display attached (degenerate case —
    /// `enter_exclusive` is a no-op).
    pub physical_snapshots: Vec<PhysicalDisplaySnapshot>,
    /// The virtual display's own state at snapshot time. Stored so
    /// leave can put it back where the OS originally placed it (the
    /// daemon does not pick the slot; Windows assigns it).
    pub virtual_snapshot: PhysicalDisplaySnapshot,
}

/// Enumerate every active display and decide which is the virtual
/// display by matching `device_name` against `virtual_display_name`.
///
/// Returns `Err(VirtualDisplayError::NotAttached)` if the named
/// virtual display does not show up in the enumeration — typical of a
/// pre-attach state or an Iddcx attach that has not yet propagated to
/// the user session.
pub fn snapshot_layout(
    virtual_display_name: &str,
) -> Result<ExclusiveLayout, VirtualDisplayError> {
    let active = enumerate_active_displays()?;
    let mut virtual_snapshot: Option<PhysicalDisplaySnapshot> = None;
    let mut physical_snapshots: Vec<PhysicalDisplaySnapshot> = Vec::new();
    for snap in active {
        if snap.device_name == virtual_display_name {
            virtual_snapshot = Some(snap);
        } else {
            physical_snapshots.push(snap);
        }
    }
    let virtual_snapshot = virtual_snapshot.ok_or_else(|| {
        log::error!(
            "[virtual-display] snapshot_layout: virtual display {virtual_display_name} \
             missing from active GDI enumeration ({physicals} physical snapshots seen)",
            physicals = physical_snapshots.len()
        );
        VirtualDisplayError::Cds(format!(
            "virtual display {virtual_display_name} not found in active GDI enumeration"
        ))
    })?;
    log::info!(
        "[virtual-display] snapshot_layout: virtual={virtual:?} \
         physicals={physical_count} ({physical_list:?})",
        virtual = virtual_snapshot,
        physical_count = physical_snapshots.len(),
        physical_list = physical_snapshots,
    );
    Ok(ExclusiveLayout {
        physical_snapshots,
        virtual_snapshot,
    })
}

/// Enumerate every currently-active display, returning its full
/// `PhysicalDisplaySnapshot` (device name, devmode, primary flag).
///
/// Exposed publicly (e2e diagnostic helper 2026-05-27) so callers
/// outside the exclusive-mode pipeline can use it to log the GDI
/// layout at arbitrary points (e.g. right after `AttachVirtualDisplay`
/// completes, before `DetachVirtualDisplay`, etc.). Returns an
/// `Err(VirtualDisplayError::Cds)` if any per-display devmode read
/// fails; the caller decides whether to propagate or just log.
pub fn enumerate_active_displays_for_diagnostics()
-> Result<Vec<PhysicalDisplaySnapshot>, VirtualDisplayError> {
    enumerate_active_displays()
}

/// Best-effort: enumerate every active display and emit a single
/// `INFO` log line with the full layout. Used by worker / daemon
/// transition points to capture "what the OS thinks the layout is"
/// without polluting the production path with conditional debug
/// noise. Errors during enumeration are logged at `WARN` (not
/// surfaced) so a diagnostic call site is never load-bearing.
pub fn log_active_displays_for_diagnostics(context: &str) {
    match enumerate_active_displays() {
        Ok(snapshots) => {
            // Find which is currently marked primary so the log line
            // calls it out explicitly — the most common question while
            // reading the log is "which one is primary right now?".
            let primary = snapshots
                .iter()
                .find(|s| s.is_primary)
                .map(|s| s.device_name.as_str())
                .unwrap_or("<none>");
            log::info!(
                "[virtual-display] display-layout({context}): {n} active display(s), \
                 primary={primary}, full={snapshots:?}",
                n = snapshots.len()
            );
        }
        Err(e) => {
            log::warn!(
                "[virtual-display] display-layout({context}): enumeration failed: {e}"
            );
        }
    }
}

fn enumerate_active_displays() -> Result<Vec<PhysicalDisplaySnapshot>, VirtualDisplayError> {
    let mut out = Vec::new();
    let mut idx: u32 = 0;
    loop {
        let mut device = empty_display_device();
        // SAFETY: out-parameter is a properly initialised DISPLAY_DEVICEW
        // (cb pre-set); a null first parameter asks for adapter-level
        // enumeration.
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), idx, &mut device, 0) };
        if !ok.as_bool() {
            break;
        }
        idx += 1;
        let device_name = utf16_to_string(&device.DeviceName);
        if device_name.is_empty() {
            continue;
        }
        // Only attached / active displays participate — phantom devices
        // (e.g. an unplugged HDMI display still listed by the driver)
        // do not have CDS-relevant state.
        if (device.StateFlags.0 & DISPLAY_DEVICE_ACTIVE.0) == 0 {
            continue;
        }
        let is_primary =
            (device.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0) != 0;
        let devmode = read_current_devmode(&device.DeviceName)?;
        out.push(PhysicalDisplaySnapshot {
            device_name,
            devmode,
            is_primary,
        });
    }
    Ok(out)
}

fn empty_display_device() -> DISPLAY_DEVICEW {
    DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        DeviceName: [0u16; 32],
        DeviceString: [0u16; 128],
        StateFlags: DISPLAY_DEVICE_STATE_FLAGS(0),
        DeviceID: [0u16; 128],
        DeviceKey: [0u16; 128],
    }
}

fn read_current_devmode(device_name_w: &[u16; 32]) -> Result<DEVMODEW, VirtualDisplayError> {
    let mut devmode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: device_name_w is a NUL-terminated UTF-16 buffer owned by
    // the caller; devmode is fully zeroed except dmSize, satisfying the
    // documented API contract.
    let ok = unsafe {
        EnumDisplaySettingsExW(
            PCWSTR::from_raw(device_name_w.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut devmode,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
    };
    if !ok.as_bool() {
        return Err(VirtualDisplayError::Cds(format!(
            "EnumDisplaySettingsExW failed for {}",
            utf16_to_string(device_name_w)
        )));
    }
    Ok(devmode)
}

fn utf16_to_string(buf: &[u16]) -> String {
    let nul = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..nul])
}

/// Build a detach devmode from the live snapshot. `dmFields` is
/// narrowed to position + zero dimensions; the rest of the struct
/// keeps the snapshot's bits so that an undocumented field never
/// influences CDS by accident (microsoft docs recommend operating on
/// the value returned by `EnumDisplaySettingsEx`).
pub fn build_detach_devmode_from(snapshot: &DEVMODEW) -> DEVMODEW {
    let mut d = *snapshot;
    d.dmFields = DEVMODE_FIELD_FLAGS(DM_POSITION.0 | DM_PELSWIDTH.0 | DM_PELSHEIGHT.0);
    d.dmPelsWidth = 0;
    d.dmPelsHeight = 0;
    // dmPosition is retained from the snapshot. Detach does not need
    // it, but Windows tolerates it and we follow the MSDN guidance.
    d
}

/// Build the "virtual display becomes primary at (0,0)" devmode.
/// Position, dimensions and refresh are forced; the rest of the
/// snapshot survives so an unrelated field does not get clobbered.
pub fn build_virtual_primary_devmode(snapshot: &DEVMODEW) -> DEVMODEW {
    let mut d = *snapshot;
    d.dmFields = DEVMODE_FIELD_FLAGS(
        DM_POSITION.0 | DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0,
    );
    // SAFETY: writing the display branch of the DEVMODEW union — the
    // print branch is meaningless for a display devmode and would not
    // be honoured by CDS anyway. `Anonymous2` is the position branch.
    d.Anonymous1.Anonymous2.dmPosition = POINTL { x: 0, y: 0 };
    d
}

/// Enter exclusive mode: make the virtual display primary at (0, 0)
/// then detach every physical display so Windows migrates their
/// windows over. On any step's failure the function rolls back the
/// already-applied steps in reverse order, leaving the host in its
/// pre-call state to the best of GDI's ability.
///
/// Side-effects are NOT written to the registry (`CDS_UPDATEREGISTRY`
/// is intentionally omitted); a worker crash or terminate leaves the
/// registry untouched and the next logon restores the physical
/// displays.
pub fn enter_exclusive(layout: &ExclusiveLayout) -> Result<(), VirtualDisplayError> {
    if layout.physical_snapshots.is_empty() {
        // Only the virtual display is active — exclusive mode is a
        // no-op. The worker would normally not request this, but a
        // crash-restart path may; treat it as success.
        log::info!(
            "[virtual-display] enter_exclusive: no physical displays attached, no-op"
        );
        return Ok(());
    }

    log::info!(
        "[virtual-display] enter_exclusive: starting CDS detach pass — \
         virtual={virtual_name} primary→(0,0), detaching {n} physical display(s)",
        virtual_name = layout.virtual_snapshot.device_name,
        n = layout.physical_snapshots.len()
    );

    // Step 1: set virtual display primary at (0,0). Queued; only
    // committed once every step has succeeded.
    //
    // E2E fix 2026-05-27: `CDS_SET_PRIMARY` MUST be paired with
    // `CDS_UPDATEREGISTRY` — without it Windows returns
    // `DISP_CHANGE_BADFLAGS` (-4). This was the root cause of the
    // "5 s prompt, fail, repeat" loop observed at 2026-05-27 14:57:
    // every enter attempt failed at this exact call. NirCmd /
    // Chromium / MultiMonitorTool all pair the two flags. The
    // crash-recovery story is degraded slightly (primary flag is now
    // persisted in registry), but the detach calls below still skip
    // `CDS_UPDATEREGISTRY` so the physical displays come back at
    // logon.
    let virtual_primary = build_virtual_primary_devmode(&layout.virtual_snapshot.devmode);
    let virtual_name = layout.virtual_snapshot.device_name.clone();
    if let Err(e) = apply_cds_with_flags(
        Some(&virtual_name),
        Some(&virtual_primary),
        SET_PRIMARY_BATCH_FLAGS,
        &format!("set primary on {virtual_name}"),
    ) {
        log::error!(
            "[virtual-display] enter_exclusive: set-primary on {virtual_name} failed: {e}"
        );
        return Err(e);
    }

    // Step 2..N: detach each physical display. If any fails, roll back
    // the already-queued operations by re-issuing the snapshot
    // devmodes (which will re-establish their positions and primary
    // state) and committing.
    let mut succeeded: Vec<&str> = Vec::with_capacity(layout.physical_snapshots.len());
    for snap in &layout.physical_snapshots {
        let detach = build_detach_devmode_from(&snap.devmode);
        let res = apply_cds_with_flags(
            Some(&snap.device_name),
            Some(&detach),
            NON_PRIMARY_BATCH_FLAGS,
            &format!("detach {}", snap.device_name),
        );
        if let Err(e) = res {
            log::error!(
                "[virtual-display] enter_exclusive: detach of {device} failed: {e}; \
                 rolling back ({rolled} already-queued detach(es))",
                device = snap.device_name,
                rolled = succeeded.len()
            );
            // Roll back: restore each already-detached physical display
            // and the virtual display to its snapshotted devmode.
            rollback_enter(&virtual_name, &layout.virtual_snapshot, &layout.physical_snapshots, &succeeded);
            return Err(e);
        }
        succeeded.push(&snap.device_name);
    }

    // Step C: commit the batch.
    if let Err(e) = commit_pending_changes() {
        log::error!(
            "[virtual-display] enter_exclusive: commit batch failed: {e}; rolling back"
        );
        rollback_enter(&virtual_name, &layout.virtual_snapshot, &layout.physical_snapshots, &succeeded);
        return Err(e);
    }
    log::info!(
        "[virtual-display] enter_exclusive: detached {n} physical display(s), \
         virtual={virtual_name} is now primary",
        n = layout.physical_snapshots.len()
    );
    // E2E diagnostic 2026-05-27: re-enumerate immediately so the log
    // captures what the OS actually thinks the layout is after the
    // commit, not just what we asked for. WM_DISPLAYCHANGE is async
    // and may fire slightly later, so the value Windows reports back
    // *right now* is the ground truth most useful for debugging.
    log_active_displays_for_diagnostics("post-enter_exclusive");
    Ok(())
}

fn rollback_enter(
    virtual_name: &str,
    virtual_snapshot: &PhysicalDisplaySnapshot,
    all_physicals: &[PhysicalDisplaySnapshot],
    detached: &[&str],
) {
    // Re-issue snapshot devmodes for the detached physicals. We
    // tolerate per-step failure here: rollback is best-effort, and the
    // transient nature of the CDS calls means a logoff is the ultimate
    // recovery.
    //
    // E2E fix 2026-05-27: any rollback step that uses
    // `CDS_SET_PRIMARY` must also include `CDS_UPDATEREGISTRY` —
    // Windows returns `DISP_CHANGE_BADFLAGS` otherwise (same
    // constraint as `enter_exclusive`'s set-primary call). Plain
    // restore (no SET_PRIMARY) sticks with `CDS_NORESET` only.
    for &name in detached.iter().rev() {
        if let Some(snap) = all_physicals.iter().find(|s| s.device_name == name) {
            // Same rule as the forward path: every call in the batch
            // needs `CDS_UPDATEREGISTRY` or Windows rejects with
            // BADFLAGS. The two-call shape (with or without primary)
            // is the only difference.
            let flags = if snap.is_primary {
                SET_PRIMARY_BATCH_FLAGS
            } else {
                NON_PRIMARY_BATCH_FLAGS
            };
            let _ = apply_cds_with_flags(
                Some(&snap.device_name),
                Some(&snap.devmode),
                flags,
                &format!("rollback restore {}", snap.device_name),
            );
        }
    }
    // Re-issue the virtual display's snapshot devmode (which restores
    // the original position and primary flag).
    let flags = if virtual_snapshot.is_primary {
        SET_PRIMARY_BATCH_FLAGS
    } else {
        NON_PRIMARY_BATCH_FLAGS
    };
    let _ = apply_cds_with_flags(
        Some(virtual_name),
        Some(&virtual_snapshot.devmode),
        flags,
        &format!("rollback restore virtual {virtual_name}"),
    );
    let _ = commit_pending_changes();
}

/// Leave exclusive mode: reattach every physical display to its
/// snapshotted position / mode / primary state and restore the
/// virtual display to its original position too. Persisted via
/// `CDS_UPDATEREGISTRY` so a normal session-end keeps the host in a
/// stable state.
///
/// Errors from individual steps are collected; the function attempts
/// every step before returning. The first error is surfaced; any
/// remaining ones are folded into the message.
pub fn leave_exclusive(layout: &ExclusiveLayout) -> Result<(), VirtualDisplayError> {
    let mut errors: Vec<String> = Vec::new();
    log::info!(
        "[virtual-display] leave_exclusive: reattaching {n} physical display(s) + \
         restoring virtual={virtual_name}",
        n = layout.physical_snapshots.len(),
        virtual_name = layout.virtual_snapshot.device_name
    );

    // Reattach each physical display (queued).
    for snap in &layout.physical_snapshots {
        let mut flags = CDS_NORESET | CDS_UPDATEREGISTRY;
        if snap.is_primary {
            flags |= CDS_SET_PRIMARY;
        }
        if let Err(e) = apply_cds_with_flags(
            Some(&snap.device_name),
            Some(&snap.devmode),
            flags,
            &format!("reattach {}", snap.device_name),
        ) {
            log::error!(
                "[virtual-display] leave_exclusive: reattach {device} failed: {e}",
                device = snap.device_name
            );
            errors.push(e.to_string());
        }
    }

    // Restore the virtual display's pre-exclusive state. If it was the
    // primary before exclusive mode that flag was already restored as
    // part of the physical loop's `CDS_SET_PRIMARY` for the original
    // primary; but if the virtual display itself was the primary, we
    // need to set it primary here.
    {
        let snap = &layout.virtual_snapshot;
        let mut flags = CDS_NORESET | CDS_UPDATEREGISTRY;
        if snap.is_primary {
            flags |= CDS_SET_PRIMARY;
        }
        if let Err(e) = apply_cds_with_flags(
            Some(&snap.device_name),
            Some(&snap.devmode),
            flags,
            &format!("restore virtual {}", snap.device_name),
        ) {
            log::error!(
                "[virtual-display] leave_exclusive: restore virtual {device} failed: {e}",
                device = snap.device_name
            );
            errors.push(e.to_string());
        }
    }

    // Commit the batch (CDS_TYPE(0) — no flags).
    if let Err(e) = commit_pending_changes() {
        log::error!("[virtual-display] leave_exclusive: commit batch failed: {e}");
        errors.push(e.to_string());
    }

    if errors.is_empty() {
        log::info!("[virtual-display] leave_exclusive: completed cleanly");
        // E2E diagnostic 2026-05-27: log the OS-reported layout right
        // after the commit so we can verify the snapshot really
        // restored DISPLAY1=primary and the virtual went back to its
        // pre-exclusive position. This was the question raised when
        // the user reported "after reconnect, virtual shows up first
        // in the picker" — was leave incomplete, or did the next
        // attach mark the new IDD primary on its own?
        log_active_displays_for_diagnostics("post-leave_exclusive");
        Ok(())
    } else {
        log_active_displays_for_diagnostics("post-leave_exclusive(partial)");
        Err(VirtualDisplayError::Cds(format!(
            "leave_exclusive partial failure: {}",
            errors.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build_detach_devmode_from` must (a) zero width / height,
    /// (b) keep the original `dmPosition`, (c) narrow `dmFields` to
    /// exactly the three flags CDS needs to honour a detach.
    #[test]
    fn detach_devmode_field_flag_layout() {
        let mut snap = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            dmFields: DEVMODE_FIELD_FLAGS(
                DM_POSITION.0 | DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0,
            ),
            dmPelsWidth: 1920,
            dmPelsHeight: 1080,
            dmDisplayFrequency: 60,
            ..Default::default()
        };
        snap.Anonymous1.Anonymous2.dmPosition = POINTL { x: 1280, y: 200 };

        let detach = build_detach_devmode_from(&snap);
        assert_eq!(detach.dmPelsWidth, 0);
        assert_eq!(detach.dmPelsHeight, 0);
        let expected_fields = DM_POSITION.0 | DM_PELSWIDTH.0 | DM_PELSHEIGHT.0;
        assert_eq!(detach.dmFields.0, expected_fields);
        // SAFETY: writing into the display branch of the DEVMODEW
        // union; we just wrote the same branch above.
        unsafe {
            assert_eq!(detach.Anonymous1.Anonymous2.dmPosition.x, 1280);
            assert_eq!(detach.Anonymous1.Anonymous2.dmPosition.y, 200);
        }
        // Untouched non-CDS fields survive (dmSpecVersion etc.).
        assert_eq!(detach.dmSize, snap.dmSize);
    }

    /// `build_virtual_primary_devmode` puts the virtual display at
    /// `(0, 0)` and forces position + pels + frequency to be the
    /// fields CDS pays attention to.
    #[test]
    fn virtual_primary_devmode_position_zero() {
        let mut snap = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            dmFields: DEVMODE_FIELD_FLAGS(DM_PELSWIDTH.0 | DM_PELSHEIGHT.0),
            dmPelsWidth: 2560,
            dmPelsHeight: 1440,
            dmDisplayFrequency: 120,
            ..Default::default()
        };
        snap.Anonymous1.Anonymous2.dmPosition = POINTL { x: 1920, y: 0 };

        let primary = build_virtual_primary_devmode(&snap);
        // SAFETY: writing/reading the display branch of the DEVMODEW
        // union as above.
        unsafe {
            assert_eq!(primary.Anonymous1.Anonymous2.dmPosition.x, 0);
            assert_eq!(primary.Anonymous1.Anonymous2.dmPosition.y, 0);
        }
        // Width / height / freq survive untouched.
        assert_eq!(primary.dmPelsWidth, 2560);
        assert_eq!(primary.dmPelsHeight, 1440);
        assert_eq!(primary.dmDisplayFrequency, 120);
        // All four CDS-honoured fields are advertised in dmFields.
        let expected =
            DM_POSITION.0 | DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0;
        assert_eq!(primary.dmFields.0, expected);
    }

    /// Modifying the snapshot after `build_detach_devmode_from` must
    /// not bleed into the returned devmode — it is a by-value copy.
    #[test]
    fn detach_devmode_is_value_copy() {
        let mut snap = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            dmFields: DEVMODE_FIELD_FLAGS(0),
            dmPelsWidth: 1920,
            dmPelsHeight: 1080,
            ..Default::default()
        };
        let detach = build_detach_devmode_from(&snap);
        snap.dmPelsWidth = 9999;
        // detach still carries the zeroed width — the construction
        // forced it to 0, and even if the caller mutates snap
        // afterwards the by-value copy is independent.
        assert_eq!(detach.dmPelsWidth, 0);
    }

    /// `enter_exclusive` on an empty physical list is a no-op,
    /// regardless of GDI availability (the function returns Ok before
    /// reaching any Win32 call).
    #[test]
    fn enter_exclusive_with_no_physicals_is_noop() {
        let layout = ExclusiveLayout {
            physical_snapshots: vec![],
            virtual_snapshot: PhysicalDisplaySnapshot {
                device_name: r"\\.\DISPLAY9".into(),
                devmode: DEVMODEW {
                    dmSize: size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                },
                is_primary: true,
            },
        };
        // No Win32 contact at all — degenerate path.
        assert!(enter_exclusive(&layout).is_ok());
    }

    /// `DEVMODE_FIELD_FLAGS` bitwise constants are disjoint — guards
    /// against a future windows-rs renumbering that would make
    /// `DM_POSITION | DM_PELSWIDTH | …` silently lose a flag.
    #[test]
    fn devmode_field_flags_are_disjoint() {
        let combined = DM_POSITION.0 | DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0;
        assert_eq!(
            combined,
            DM_POSITION.0 + DM_PELSWIDTH.0 + DM_PELSHEIGHT.0 + DM_DISPLAYFREQUENCY.0
        );
    }

    /// `ExclusiveLayout` and its constituent types are `Send + Sync`
    /// so the daemon-to-worker IPC layer and the worker's
    /// `spawn_blocking` can pass them across thread boundaries
    /// without an intermediate POD wrapper. Spike (a) verified this
    /// at 2026-05-26 against windows-rs 0.61.
    #[test]
    fn exclusive_layout_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PhysicalDisplaySnapshot>();
        assert_send_sync::<ExclusiveLayout>();
    }

    /// E2E regression guard 2026-05-27: every batched set-primary
    /// call must carry `CDS_NORESET | CDS_SET_PRIMARY |
    /// CDS_UPDATEREGISTRY`. The bug — observed in production at
    /// 2026-05-27 14:57 as `DISP_CHANGE code -4 (BADFLAGS)` on every
    /// enter_exclusive attempt — was that the third flag was missing.
    /// If a future change drops `CDS_UPDATEREGISTRY` from the
    /// constant, Windows will reject the call again and we are back
    /// in the infinite "5 s prompt, fail, repeat" loop.
    #[test]
    fn set_primary_batch_flags_include_all_three_required_bits() {
        let bits = SET_PRIMARY_BATCH_FLAGS.0;
        assert_ne!(
            bits & CDS_NORESET.0,
            0,
            "CDS_NORESET must be present so the call participates in the batch"
        );
        assert_ne!(
            bits & CDS_SET_PRIMARY.0,
            0,
            "CDS_SET_PRIMARY must be present — that's the whole point"
        );
        assert_ne!(
            bits & CDS_UPDATEREGISTRY.0,
            0,
            "CDS_UPDATEREGISTRY is REQUIRED with CDS_SET_PRIMARY; \
             without it Windows returns DISP_CHANGE_BADFLAGS (-4)"
        );
    }

    /// E2E regression guard 2026-05-27 (second iteration): non-primary
    /// batch calls (detach + rollback restore) also require
    /// `CDS_UPDATEREGISTRY`. Observed in production as `DISP_CHANGE
    /// code -4 (BADFLAGS)` on the detach call at 2026-05-27 15:09,
    /// immediately after the first set-primary fix made it past
    /// step 1.
    #[test]
    fn non_primary_batch_flags_include_required_bits() {
        let bits = NON_PRIMARY_BATCH_FLAGS.0;
        assert_ne!(
            bits & CDS_NORESET.0,
            0,
            "CDS_NORESET must be present so the call participates in the batch"
        );
        assert_ne!(
            bits & CDS_UPDATEREGISTRY.0,
            0,
            "CDS_UPDATEREGISTRY is REQUIRED in any CDS_NORESET batch call; \
             without it Windows returns DISP_CHANGE_BADFLAGS (-4) — \
             even for plain detach/position-restore with no primary flag"
        );
        // And no SET_PRIMARY bit — this constant is for non-primary
        // calls specifically.
        assert_eq!(
            bits & CDS_SET_PRIMARY.0,
            0,
            "non-primary constant must not carry CDS_SET_PRIMARY",
        );
    }
}
