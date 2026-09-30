//! Physical display mode selection and platform operations.

use desk_ipc_protocol::message::{
    PhysicalDisplayAction, PhysicalDisplayModeData, PhysicalDisplayModeOutcome,
    PhysicalDisplayModeResponsePayload, SetPhysicalDisplayModePayload, WorkerToService,
};
use desk_signal_facade::model::{
    image_capture::Resolution,
    media_capability::{VideoEncoderCapability, VideoEncoderId, check_encoder_input},
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalMode {
    /// Platform mode identifier, scoped to one freshly enumerated display.
    pub selector: String,
    pub logical_width: u32,
    pub logical_height: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Expected encoded frame size for the selected capture backend. On
    /// macOS the current ScreenCaptureKit stream is configured in display
    /// points, which may differ from the CoreGraphics physical pixel size.
    pub capture_width: u32,
    pub capture_height: u32,
    pub refresh_millihz: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayTopology {
    pub device_name: String,
    pub identity: String,
    pub mode_selector: String,
    pub main: bool,
    pub mirrored: bool,
    pub origin_x: i32,
    pub origin_y: i32,
}

#[derive(Clone, Debug)]
pub struct PhysicalDisplaySnapshot {
    pub device_name: String,
    pub identity: String,
    pub current: PhysicalMode,
    pub candidates: Vec<PhysicalMode>,
    pub topology: Vec<DisplayTopology>,
}

/// A successful OS write still needs a readback of the selected mode and
/// surrounding topology. If that readback fails or disagrees, try to restore
/// the previous mode while the caller still owns the topology operation.
fn verify_after_apply(
    before: &PhysicalDisplaySnapshot,
    after: Result<PhysicalDisplaySnapshot, String>,
    selector: &str,
    rollback: impl FnOnce() -> Result<(), String>,
) -> Result<PhysicalDisplaySnapshot, String> {
    let checked = after.and_then(|after| {
        let other_displays_unchanged = before.topology.iter().all(|entry| {
            after.topology.iter().any(|next| {
                next.device_name == entry.device_name
                    && next.identity == entry.identity
                    && next.main == entry.main
                    && next.mirrored == entry.mirrored
                    && (entry.device_name == before.device_name
                        || next.mode_selector == entry.mode_selector)
            })
        });
        if after.identity != before.identity
            || after.current.selector != selector
            || before.topology.len() != after.topology.len()
            || !other_displays_unchanged
        {
            Err("display topology or selected mode changed unexpectedly".into())
        } else {
            Ok(after)
        }
    });
    match checked {
        Ok(after) => Ok(after),
        Err(reason) => match rollback() {
            Ok(()) => Err(format!("{reason}; original mode restored")),
            Err(error) => Err(format!("{reason}; guarded rollback not confirmed: {error}")),
        },
    }
}

/// Read a selected physical monitor and its desktop-usable modes.
pub fn inspect(device_name: &str) -> Result<PhysicalDisplaySnapshot, String> {
    #[cfg(target_os = "macos")]
    return macos::inspect(device_name);
    #[cfg(target_os = "windows")]
    return windows::inspect(device_name);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = device_name;
        Err("physical display mode changes are unsupported on this platform".into())
    }
}

/// Apply a mode that must still occur in the display's current enumeration.
pub fn apply(
    device_name: &str,
    selector: &str,
    expected_current_selector: &str,
    expected_display_identity: &str,
) -> Result<PhysicalDisplaySnapshot, String> {
    #[cfg(target_os = "macos")]
    return macos::apply(
        device_name,
        selector,
        expected_current_selector,
        expected_display_identity,
    );
    #[cfg(target_os = "windows")]
    return windows::apply(
        device_name,
        selector,
        expected_current_selector,
        expected_display_identity,
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (
            device_name,
            selector,
            expected_current_selector,
            expected_display_identity,
        );
        Err("physical display mode changes are unsupported on this platform".into())
    }
}

/// Score predicted encoded pixels against the browser's device-pixel viewport.
/// The aspect ratio has a larger weight than absolute size to avoid black bars.
pub fn mode_score(mode: &PhysicalMode, target_width: u32, target_height: u32) -> f64 {
    if mode.capture_width == 0
        || mode.capture_height == 0
        || target_width == 0
        || target_height == 0
    {
        return f64::INFINITY;
    }
    let width = mode.capture_width as f64 / target_width as f64;
    let height = mode.capture_height as f64 / target_height as f64;
    3.0 * (width / height).ln().abs() + width.ln().abs() + height.ln().abs()
}

/// Return a replacement only when it improves on the current mode by 20%.
/// The caller applies encoder limits and platform safety checks first.
pub fn choose_mode(
    snapshot: &PhysicalDisplaySnapshot,
    target_width: u32,
    target_height: u32,
    max_width: u32,
    max_height: u32,
) -> Option<&PhysicalMode> {
    let current = &snapshot.current;
    let current_score = mode_score(current, target_width, target_height);
    let mut candidates: Vec<&PhysicalMode> = snapshot
        .candidates
        .iter()
        .filter(|mode| {
            mode.logical_width >= 1024
                && mode.logical_height >= 720
                && mode.capture_width <= max_width
                && mode.capture_height <= max_height
                && mode.capture_width > 0
                && mode.capture_height > 0
        })
        .collect();
    candidates.sort_by(|left, right| {
        mode_score(left, target_width, target_height)
            .total_cmp(&mode_score(right, target_width, target_height))
            .then_with(|| {
                (left.refresh_millihz != current.refresh_millihz)
                    .cmp(&(right.refresh_millihz != current.refresh_millihz))
            })
            .then_with(|| {
                (left.selector != current.selector).cmp(&(right.selector != current.selector))
            })
            .then_with(|| left.selector.cmp(&right.selector))
    });
    let best = candidates.first().copied()?;
    let best_score = mode_score(best, target_width, target_height);
    let current_within_limit = current.capture_width <= max_width
        && current.capture_height <= max_height
        && snapshot
            .candidates
            .iter()
            .any(|mode| mode.selector == current.selector);
    if best.selector == current.selector
        || (current_within_limit && best_score >= current_score * 0.8)
    {
        return None;
    }
    Some(best)
}

fn mode_supported_by_all_encoders(mode: &PhysicalMode, encoder_ids: &[VideoEncoderId]) -> bool {
    encoder_ids.iter().all(|encoder_id| {
        let support = VideoEncoderCapability::for_id(*encoder_id).input_support;
        check_encoder_input(
            Resolution::new(mode.capture_width, mode.capture_height),
            &support,
        )
        .is_ok()
    })
}

fn execute(
    payload: &SetPhysicalDisplayModePayload,
    encoder_ids: &[VideoEncoderId],
) -> Result<PhysicalDisplayModeData, String> {
    let mut before = inspect(&payload.device_name)?;
    if !encoder_ids.is_empty() {
        before
            .candidates
            .retain(|mode| mode_supported_by_all_encoders(mode, encoder_ids));
    }
    let selector = match &payload.action {
        PhysicalDisplayAction::Auto {
            viewport_width,
            viewport_height,
            max_capture_width,
            max_capture_height,
            ..
        } => choose_mode(
            &before,
            *viewport_width,
            *viewport_height,
            *max_capture_width,
            *max_capture_height,
        )
        .map(|mode| mode.selector.clone()),
        PhysicalDisplayAction::Select { selector } => {
            if !before
                .candidates
                .iter()
                .any(|mode| mode.selector == *selector)
            {
                return Err("selected physical mode is not currently available".into());
            }
            Some(selector.clone())
        }
        PhysicalDisplayAction::Restore {
            original_selector,
            expected_applied_selector,
            expected_display_identity,
        } => {
            if before.identity != *expected_display_identity {
                return Err("physical display identity changed; restore skipped".into());
            }
            if before.current.selector == *original_selector {
                None
            } else if before.current.selector != *expected_applied_selector {
                return Err("physical display was changed locally; restore skipped".into());
            } else {
                Some(original_selector.clone())
            }
        }
    };
    let changed = selector
        .as_ref()
        .is_some_and(|selector| *selector != before.current.selector);
    let after = if let Some(selector) = selector {
        apply(
            &payload.device_name,
            &selector,
            &before.current.selector,
            &before.identity,
        )?
    } else {
        before.clone()
    };
    Ok(PhysicalDisplayModeData {
        device_name: payload.device_name.clone(),
        display_identity: after.identity,
        previous_selector: before.current.selector,
        selector: after.current.selector,
        pixel_width: after.current.capture_width,
        pixel_height: after.current.capture_height,
        refresh_millihz: after.current.refresh_millihz,
        changed,
        restored: matches!(&payload.action, PhysicalDisplayAction::Restore { .. }),
    })
}

pub async fn run_mode(
    payload: SetPhysicalDisplayModePayload,
    capture_encoders: Option<Vec<VideoEncoderId>>,
) -> WorkerToService {
    let request_id = payload.request_id.clone();
    let connection_id = payload.connection_id.clone();
    let connection_epoch = payload.connection_epoch.clone();
    let operation_id = payload.operation_id;
    let allowed = matches!(&payload.action, PhysicalDisplayAction::Restore { .. })
        || capture_encoders.is_some();
    let outcome = if !allowed {
        PhysicalDisplayModeOutcome::Failed(
            "physical display is not the active capture target".into(),
        )
    } else {
        match tokio::task::spawn_blocking(move || {
            execute(&payload, capture_encoders.as_deref().unwrap_or(&[]))
        })
        .await
        {
            Ok(Ok(data)) => PhysicalDisplayModeOutcome::Applied(data),
            Ok(Err(reason)) => PhysicalDisplayModeOutcome::Failed(reason),
            Err(error) => PhysicalDisplayModeOutcome::Failed(format!(
                "physical display mode worker failed: {error}"
            )),
        }
    };
    WorkerToService::PhysicalDisplayMode(PhysicalDisplayModeResponsePayload {
        request_id,
        connection_id,
        connection_epoch,
        operation_id,
        outcome,
    })
}

/// A video failure after an OS mode switch is not necessarily a display
/// rollback. Report the mode as still applied when a guarded rollback fails
/// and a fresh readback confirms it, so the daemon can restore it later.
pub async fn rollback_after_failed_frame(
    device_name: String,
    data: PhysicalDisplayModeData,
    reason: String,
) -> PhysicalDisplayModeOutcome {
    let previous = data.previous_selector.clone();
    let applied = data.selector.clone();
    let identity = data.display_identity.clone();
    let rollback = tokio::task::spawn_blocking(move || {
        let result = apply(&device_name, &previous, &applied, &identity);
        let observed = result
            .as_ref()
            .err()
            .and_then(|_| inspect(&device_name).ok())
            .map(|snapshot| snapshot.current.selector);
        (result, observed)
    })
    .await;
    match rollback {
        Ok((Ok(_), _)) => PhysicalDisplayModeOutcome::Failed(reason),
        Ok((Err(error), observed)) => {
            let reason = format!("{reason}; mode rollback failed: {error}");
            if observed.as_deref() == Some(data.selector.as_str()) {
                PhysicalDisplayModeOutcome::AppliedWithoutVideo { data, reason }
            } else {
                PhysicalDisplayModeOutcome::Failed(reason)
            }
        }
        Err(error) => PhysicalDisplayModeOutcome::Failed(format!(
            "{reason}; mode rollback task failed: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(selector: &str, width: u32, height: u32) -> PhysicalMode {
        PhysicalMode {
            selector: selector.into(),
            logical_width: width,
            logical_height: height,
            pixel_width: width,
            pixel_height: height,
            capture_width: width,
            capture_height: height,
            refresh_millihz: 60_000,
        }
    }

    fn snapshot(selector: &str) -> PhysicalDisplaySnapshot {
        PhysicalDisplaySnapshot {
            device_name: "display".into(),
            identity: "monitor-one".into(),
            current: mode(selector, 1920, 1080),
            candidates: vec![],
            topology: vec![DisplayTopology {
                device_name: "display".into(),
                identity: "monitor-one".into(),
                mode_selector: selector.into(),
                main: true,
                mirrored: false,
                origin_x: 0,
                origin_y: 0,
            }],
        }
    }

    #[test]
    fn unreadable_post_apply_state_attempts_guarded_rollback() {
        let before = snapshot("original");
        let mut rollback_called = false;
        let result = verify_after_apply(
            &before,
            Err("display temporarily unavailable".into()),
            "target",
            || {
                rollback_called = true;
                Ok(())
            },
        );
        assert!(rollback_called);
        assert!(result.unwrap_err().contains("original mode restored"));
    }

    #[test]
    fn failed_guarded_rollback_does_not_claim_restoration() {
        let before = snapshot("original");
        let result = verify_after_apply(&before, Ok(snapshot("unexpected")), "target", || {
            Err("current mode changed locally".into())
        });
        let reason = result.unwrap_err();
        assert!(reason.contains("guarded rollback not confirmed"));
        assert!(reason.contains("current mode changed locally"));
    }

    #[test]
    fn extended_desktop_can_reflow_coordinates_but_not_other_monitor_modes() {
        let mut before = snapshot("original");
        before.topology.push(DisplayTopology {
            device_name: "other".into(),
            identity: "monitor-two".into(),
            mode_selector: "other-original".into(),
            main: false,
            mirrored: false,
            origin_x: 1920,
            origin_y: 0,
        });
        let mut after = snapshot("target");
        let mut moved_other = before.topology[1].clone();
        moved_other.origin_x = 1280;
        after.topology.push(moved_other);
        assert!(
            verify_after_apply(&before, Ok(after.clone()), "target", || {
                panic!("coordinate reflow should not require rollback")
            })
            .is_ok()
        );

        after.topology[1].mode_selector = "other-changed".into();
        let mut rolled_back = false;
        let result = verify_after_apply(&before, Ok(after), "target", || {
            rolled_back = true;
            Ok(())
        });
        assert!(rolled_back);
        assert!(result.unwrap_err().contains("original mode restored"));
    }

    #[test]
    fn candidate_must_fit_every_active_viewers_encoder() {
        let large = mode("large", 4096, 2160);
        assert!(mode_supported_by_all_encoders(
            &large,
            &[VideoEncoderId::X264]
        ));
        assert!(!mode_supported_by_all_encoders(
            &large,
            &[VideoEncoderId::X264, VideoEncoderId::OpenH264]
        ));
    }

    #[test]
    fn chooses_existing_mode_and_rejects_small_improvement() {
        let snapshot = PhysicalDisplaySnapshot {
            device_name: "selected".into(),
            identity: "screen-1".into(),
            current: mode("current", 2560, 1440),
            candidates: vec![mode("current", 2560, 1440), mode("near", 1920, 1080)],
            topology: vec![],
        };
        assert_eq!(
            choose_mode(&snapshot, 1900, 1060, 4096, 4096)
                .unwrap()
                .selector,
            "near"
        );
        assert!(choose_mode(&snapshot, 2500, 1400, 4096, 4096).is_none());
    }

    #[test]
    fn keeps_aspect_ratio_and_encoder_limits() {
        let snapshot = PhysicalDisplaySnapshot {
            device_name: "selected".into(),
            identity: "screen-1".into(),
            current: mode("current", 3840, 2160),
            candidates: vec![
                mode("current", 3840, 2160),
                mode("wide", 1920, 1080),
                mode("square", 1600, 1200),
            ],
            topology: vec![],
        };
        assert_eq!(
            choose_mode(&snapshot, 1600, 900, 2000, 1200)
                .unwrap()
                .selector,
            "wide"
        );
        assert!(choose_mode(&snapshot, 1600, 900, 1000, 700).is_none());
    }

    #[test]
    fn retina_mode_scores_the_capture_output_not_panel_pixels() {
        let mut retina = mode("retina", 1512, 982);
        retina.pixel_width = 3024;
        retina.pixel_height = 1964;
        let snapshot = PhysicalDisplaySnapshot {
            device_name: "selected".into(),
            identity: "screen-1".into(),
            current: retina.clone(),
            candidates: vec![retina, mode("standard", 1920, 1080)],
            topology: vec![],
        };
        assert!(choose_mode(&snapshot, 1500, 980, 2000, 1200).is_none());
    }
}
