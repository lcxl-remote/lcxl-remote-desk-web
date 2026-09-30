//! CoreGraphics physical display mode provider.

use std::{ffi::c_void, ops::Deref, ptr};

use core_foundation::{array::CFArray, base::TCFType};
use core_graphics::display::{
    CGConfigureOption, CGDisplay, CGDisplayCopyAllDisplayModes, CGDisplayMode, CGDisplayModeRetain,
};
use foreign_types::ForeignType;

use super::{DisplayTopology, PhysicalDisplaySnapshot, PhysicalMode, verify_after_apply};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayModeIsUsableForDesktopGUI(mode: *const c_void) -> bool;
}

fn all_display_modes(display_id: u32) -> Result<Vec<CGDisplayMode>, String> {
    let array_ref = unsafe { CGDisplayCopyAllDisplayModes(display_id, ptr::null()) };
    if array_ref.is_null() {
        return Err("CoreGraphics did not return display modes".into());
    }
    let array: CFArray = unsafe { CFArray::wrap_under_create_rule(array_ref) };
    Ok(array
        .into_iter()
        .map(|value| {
            let raw = *value.deref() as *mut core_graphics::sys::CGDisplayMode;
            // The CFArray owns its element references. Retain each mode for
            // the independently owned CGDisplayMode wrapper before the array
            // is released; the crate's all_display_modes wrapper omits this.
            unsafe {
                CGDisplayModeRetain(raw);
                CGDisplayMode::from_ptr(raw)
            }
        })
        .collect())
}

fn mode_data(mode: &CGDisplayMode) -> PhysicalMode {
    let refresh = (mode.refresh_rate() * 1000.0).round();
    let refresh_millihz = if refresh.is_finite() && refresh >= 0.0 {
        refresh as u32
    } else {
        0
    };
    PhysicalMode {
        selector: format!(
            "{}:{}:{}:{}:{}:{}:{}",
            mode.mode_id(),
            mode.width(),
            mode.height(),
            mode.pixel_width(),
            mode.pixel_height(),
            refresh_millihz,
            mode.bit_depth(),
        ),
        logical_width: mode.width() as u32,
        logical_height: mode.height() as u32,
        pixel_width: mode.pixel_width() as u32,
        pixel_height: mode.pixel_height() as u32,
        capture_width: mode.width() as u32,
        capture_height: mode.height() as u32,
        refresh_millihz,
    }
}

fn display_identity(display: &CGDisplay) -> String {
    format!(
        "mac:{}:{}:{}:{}:{}",
        display.id,
        display.vendor_number(),
        display.model_number(),
        display.serial_number(),
        display.unit_number(),
    )
}

fn display_for(device_name: &str) -> Result<CGDisplay, String> {
    let id = device_name
        .parse::<u32>()
        .map_err(|_| "invalid macOS display identifier".to_string())?;
    let ids = CGDisplay::active_displays()
        .map_err(|code| format!("CGGetActiveDisplayList failed: {code}"))?;
    if !ids.contains(&id) {
        return Err("selected physical display is not active".into());
    }
    let display = CGDisplay::new(id);
    if !display.is_online() || display.is_asleep() {
        return Err("selected physical display is unavailable".into());
    }
    if display.is_in_mirror_set() {
        return Err("mirrored displays cannot be adjusted automatically".into());
    }
    Ok(display)
}

fn topology() -> Result<Vec<DisplayTopology>, String> {
    let ids = CGDisplay::active_displays()
        .map_err(|code| format!("CGGetActiveDisplayList failed: {code}"))?;
    ids.into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let mode = display
                .display_mode()
                .ok_or_else(|| format!("display {id} has no current mode"))?;
            let bounds = display.bounds();
            Ok(DisplayTopology {
                device_name: id.to_string(),
                identity: display_identity(&display),
                mode_selector: mode_data(&mode).selector,
                main: display.is_main(),
                mirrored: display.is_in_mirror_set(),
                origin_x: bounds.origin.x.round() as i32,
                origin_y: bounds.origin.y.round() as i32,
            })
        })
        .collect()
}

pub(super) fn inspect(device_name: &str) -> Result<PhysicalDisplaySnapshot, String> {
    let display = display_for(device_name)?;
    let current = display
        .display_mode()
        .ok_or_else(|| "selected physical display has no current mode".to_string())?;
    let current_depth = current.bit_depth();
    let candidates = all_display_modes(display.id)?
        .into_iter()
        .filter(|mode| {
            // Keep the current color depth and skip modes macOS marks as
            // unsuitable for a normal desktop. The mode is re-enumerated at apply.
            mode.bit_depth() == current_depth
                && unsafe { CGDisplayModeIsUsableForDesktopGUI(mode.as_ptr().cast()) }
        })
        .map(|mode| mode_data(&mode))
        .collect();
    Ok(PhysicalDisplaySnapshot {
        device_name: device_name.into(),
        identity: display_identity(&display),
        current: mode_data(&current),
        candidates,
        topology: topology()?,
    })
}

fn apply_raw(
    display: &CGDisplay,
    mode: &CGDisplayMode,
    expected_current_selector: &str,
    expected_display_identity: &str,
) -> Result<(), String> {
    if display_identity(display) != expected_display_identity {
        return Err("physical display identity changed before mode apply".into());
    }
    let current = display
        .display_mode()
        .ok_or_else(|| "selected physical display has no current mode".to_string())?;
    if mode_data(&current).selector != expected_current_selector {
        return Err("physical display changed locally before mode apply".into());
    }
    let config = display
        .begin_configuration()
        .map_err(|code| format!("CGBeginDisplayConfiguration failed: {code}"))?;
    if let Err(code) = display.configure_display_with_display_mode(&config, mode) {
        let _ = display.cancel_configuration(&config);
        return Err(format!("CGConfigureDisplayWithDisplayMode failed: {code}"));
    }
    display
        .complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
        .map_err(|code| format!("CGCompleteDisplayConfiguration failed: {code}"))
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
    let display = display_for(device_name)?;
    let modes = all_display_modes(display.id)?;
    let target = modes
        .iter()
        .find(|mode| mode_data(mode).selector == selector)
        .ok_or_else(|| "selected mode disappeared before apply".to_string())?;
    let original = modes
        .iter()
        .find(|mode| mode_data(mode).selector == before.current.selector)
        .ok_or_else(|| "original mode disappeared before apply".to_string())?;
    apply_raw(
        &display,
        target,
        expected_current_selector,
        expected_display_identity,
    )?;
    verify_after_apply(&before, inspect(device_name), selector, || {
        apply_raw(&display, original, selector, expected_display_identity)?;
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
    use desk_capture_engine::model::image_capture::ImageCaptureType;
    use desk_capture_engine::{
        image_capture::{
            image_capture_factory::list_image_output,
            mac_screencapturekit::MacScreencaptureKitImageCapture,
        },
        model::image_capture::{CaptureRequest, CursorCaptureMode, ImageCapture},
    };
    use desk_input_injection::display_watcher;
    use desk_signal_facade::model::desk_settings::DeskSettings;
    use std::time::{Duration, Instant};

    struct RestoreGuard {
        device_name: String,
        identity: String,
        original_selector: String,
        applied_selector: String,
    }

    impl Drop for RestoreGuard {
        fn drop(&mut self) {
            if let Ok(snapshot) = inspect(&self.device_name)
                && snapshot.identity == self.identity
                && snapshot.current.selector == self.applied_selector
            {
                let _ = apply(
                    &self.device_name,
                    &self.original_selector,
                    &self.applied_selector,
                    &self.identity,
                );
            }
        }
    }

    fn closest_resolution_at_same_refresh(before: &PhysicalDisplaySnapshot) -> PhysicalMode {
        before
            .candidates
            .iter()
            .filter(|mode| {
                mode.logical_width >= 1024
                    && mode.logical_height >= 768
                    && mode.refresh_millihz == before.current.refresh_millihz
                    && (mode.logical_width != before.current.logical_width
                        || mode.logical_height != before.current.logical_height)
            })
            .min_by_key(|mode| {
                before.current.logical_width.abs_diff(mode.logical_width)
                    + before.current.logical_height.abs_diff(mode.logical_height)
            })
            .expect("alternate desktop resolution")
            .clone()
    }

    fn wait_for_capture_size(
        capture: &mut MacScreencaptureKitImageCapture,
        expected_width: u32,
        expected_height: u32,
    ) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = String::new();
        while Instant::now() < deadline {
            match capture.capture(CaptureRequest {
                cursor_mode: CursorCaptureMode::Disable,
            }) {
                Ok(frame) => {
                    let width = frame.image.get_width();
                    let height = frame.image.get_height();
                    if (width, height) == (expected_width, expected_height) {
                        return;
                    }
                    last = format!("received {width}x{height}");
                }
                Err(error) => last = format!("capture error: {error:?}"),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("no {expected_width}x{expected_height} frame within 15s; last={last}");
    }

    fn assert_enumerated_size(device_name: &str, expected_width: u32, expected_height: u32) {
        let displays =
            list_image_output(ImageCaptureType::SCKIT).expect("enumerate ScreenCaptureKit");
        let display = displays
            .iter()
            .find(|display| display.device_name == device_name)
            .expect("selected ScreenCaptureKit display");
        let rect = &display.desktop_coordinates;
        assert_eq!(
            (rect.right - rect.left, rect.bottom - rect.top),
            (expected_width as i32, expected_height as i32),
        );
    }

    fn wait_for_display_event(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<display_watcher::DisplayChangeEvent>,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if rx.try_recv().is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("CoreGraphics display-change callback did not arrive within 5s");
    }

    #[test]
    fn rejects_non_numeric_device_without_touching_core_graphics() {
        assert_eq!(
            display_for("\\\\.\\DISPLAY1").unwrap_err(),
            "invalid macOS display identifier"
        );
    }

    /// Run explicitly on a logged-in Mac with a real, non-mirrored display.
    /// The guard also tries to restore the original mode if an assertion fails.
    #[test]
    #[ignore = "temporarily changes the local physical display resolution"]
    fn applies_and_restores_a_real_resolution() {
        eprintln!("physical smoke: enumerate active displays");
        let device_name = CGDisplay::active_displays()
            .expect("active displays")
            .into_iter()
            .find(|id| {
                let display = CGDisplay::new(*id);
                display.is_online() && !display.is_in_mirror_set()
            })
            .expect("non-mirrored active display")
            .to_string();
        eprintln!("physical smoke: inspect {device_name}");
        let before = inspect(&device_name).expect("inspect original display");
        eprintln!("physical smoke: {} candidates", before.candidates.len());
        let alternative = closest_resolution_at_same_refresh(&before);
        eprintln!("physical smoke: apply {}", alternative.selector);
        let _restore_guard = RestoreGuard {
            device_name: device_name.clone(),
            identity: before.identity.clone(),
            original_selector: before.current.selector.clone(),
            applied_selector: alternative.selector.clone(),
        };
        let changed = apply(
            &device_name,
            &alternative.selector,
            &before.current.selector,
            &before.identity,
        )
        .expect("apply alternate resolution");
        eprintln!("physical smoke: applied");
        assert_eq!(changed.current.selector, alternative.selector);
        let restored = apply(
            &device_name,
            &before.current.selector,
            &alternative.selector,
            &before.identity,
        )
        .expect("restore original resolution");
        eprintln!("physical smoke: restored");
        assert_eq!(restored.current.selector, before.current.selector);
    }

    /// Explicit hardware check for the production mode provider together
    /// with the production ScreenCaptureKit backend's frame-size recovery.
    #[test]
    #[ignore = "temporarily changes the local physical display resolution and records the screen"]
    fn capture_frames_follow_real_resolution_change() {
        let device_name = CGDisplay::active_displays()
            .expect("active displays")
            .into_iter()
            .find(|id| {
                let display = CGDisplay::new(*id);
                display.is_online() && !display.is_in_mirror_set()
            })
            .expect("non-mirrored active display")
            .to_string();
        let before = inspect(&device_name).expect("inspect original display");
        let alternative = closest_resolution_at_same_refresh(&before);
        let settings = DeskSettings {
            video_device_name: device_name.clone(),
            ..Default::default()
        };
        let mut capture =
            MacScreencaptureKitImageCapture::new(&settings).expect("construct ScreenCaptureKit");
        wait_for_capture_size(
            &mut capture,
            before.current.capture_width,
            before.current.capture_height,
        );
        assert_enumerated_size(
            &device_name,
            before.current.capture_width,
            before.current.capture_height,
        );
        let _restore_guard = RestoreGuard {
            device_name: device_name.clone(),
            identity: before.identity.clone(),
            original_selector: before.current.selector.clone(),
            applied_selector: alternative.selector.clone(),
        };
        apply(
            &device_name,
            &alternative.selector,
            &before.current.selector,
            &before.identity,
        )
        .expect("apply alternate resolution");
        wait_for_capture_size(
            &mut capture,
            alternative.capture_width,
            alternative.capture_height,
        );
        assert_enumerated_size(
            &device_name,
            alternative.capture_width,
            alternative.capture_height,
        );
        apply(
            &device_name,
            &before.current.selector,
            &alternative.selector,
            &before.identity,
        )
        .expect("restore original resolution");
        wait_for_capture_size(
            &mut capture,
            before.current.capture_width,
            before.current.capture_height,
        );
        assert_enumerated_size(
            &device_name,
            before.current.capture_width,
            before.current.capture_height,
        );
    }

    /// Confirm the callback used to detect local display changes actually
    /// arrives for a real CoreGraphics mode transaction on this Mac.
    #[test]
    #[ignore = "temporarily changes the local physical display resolution"]
    fn real_mode_change_notifies_display_watcher() {
        let device_name = CGDisplay::active_displays()
            .expect("active displays")
            .into_iter()
            .find(|id| {
                let display = CGDisplay::new(*id);
                display.is_online() && !display.is_in_mirror_set()
            })
            .expect("non-mirrored active display")
            .to_string();
        let before = inspect(&device_name).expect("inspect original display");
        let alternative = closest_resolution_at_same_refresh(&before);
        let _restore_guard = RestoreGuard {
            device_name: device_name.clone(),
            identity: before.identity.clone(),
            original_selector: before.current.selector.clone(),
            applied_selector: alternative.selector.clone(),
        };
        let (_watcher, mut rx) = display_watcher::spawn().expect("register display watcher");
        apply(
            &device_name,
            &alternative.selector,
            &before.current.selector,
            &before.identity,
        )
        .expect("apply alternate resolution");
        wait_for_display_event(&mut rx);
        apply(
            &device_name,
            &before.current.selector,
            &alternative.selector,
            &before.identity,
        )
        .expect("restore original resolution");
        wait_for_display_event(&mut rx);
    }
}
