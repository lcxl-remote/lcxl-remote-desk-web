use std::sync::{Mutex, OnceLock};

pub fn prepare_tauri_window_backend() {
    let should_use_x11 = should_default_tauri_to_x11(
        std::env::var_os("GDK_BACKEND").is_some(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    );
    if !should_use_x11 {
        return;
    }

    // GTK3's native Wayland backend creates ordinary xdg_toplevel surfaces.
    // That protocol has no standard requests for skip-taskbar/skip-overview or
    // always-on-top, so GNOME ignores the hints used by the status indicator.
    // XWayland exposes the EWMH states that Tauri already sets for those two
    // behaviours. This only selects the GUI backend; WAYLAND_DISPLAY remains
    // intact, so capture and input continue to use the Wayland Portal.
    //
    // Safety: run() invokes this before Tauri initializes GTK or starts any
    // application threads, so no other thread can read the environment while
    // it is being changed.
    unsafe { std::env::set_var("GDK_BACKEND", "x11") };
}

fn should_default_tauri_to_x11(
    has_explicit_backend: bool,
    has_wayland_display: bool,
    has_x11_display: bool,
) -> bool {
    !has_explicit_backend && has_wayland_display && has_x11_display
}

use desk_input_injection::linux_input_block::{EndReason, InputBlock};

static LINUX_GRABBER: OnceLock<Mutex<Option<InputBlock>>> = OnceLock::new();

fn grabber_slot() -> &'static Mutex<Option<InputBlock>> {
    LINUX_GRABBER.get_or_init(|| Mutex::new(None))
}

fn toggle_xrandr_brightness(on: bool) {
    if let Ok(output) = std::process::Command::new("xrandr").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains(" connected") {
                if let Some(output_name) = line.split_whitespace().next() {
                    let brightness = if on { "0.0" } else { "1.0" };
                    let _ = std::process::Command::new("xrandr")
                        .args(&["--output", output_name, "--brightness", brightness])
                        .status();
                }
            }
        }
    }
}

pub fn block_input(
    block: bool,
    on_local_escape: Option<super::LocalEscapeCallback>,
) -> Result<(), String> {
    if block {
        let mut guard = grabber_slot().lock().map_err(|e| e.to_string())?;
        if guard.as_ref().is_some_and(InputBlock::is_active) {
            return Ok(());
        }
        // Dispose of an expired owner before acquiring another device set.
        *guard = None;
        let input = InputBlock::acquire(std::time::Duration::from_secs(300), move |reason| {
            if reason != EndReason::Released {
                if let Some(callback) = on_local_escape {
                    callback();
                }
            }
        })
        .map_err(|error| error.to_string())?;
        let report = input.report();
        // The legacy privacy UI has no partial-coverage indicator. Do not
        // present it as active after only a subset of devices was acquired.
        if report.failed != 0 {
            return Err(format!(
                "Input blocking is incomplete: {} blocked, {} failed",
                report.grabbed, report.failed
            ));
        }
        toggle_xrandr_brightness(true);
        *guard = Some(input);
    } else {
        let input = grabber_slot().lock().map_err(|e| e.to_string())?.take();
        drop(input);
        toggle_xrandr_brightness(false);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::should_default_tauri_to_x11;

    #[test]
    fn defaults_wayland_session_with_xwayland_to_x11() {
        assert!(should_default_tauri_to_x11(false, true, true));
    }

    #[test]
    fn preserves_explicit_gdk_backend() {
        assert!(!should_default_tauri_to_x11(true, true, true));
    }

    #[test]
    fn does_not_select_unavailable_x11_backend() {
        assert!(!should_default_tauri_to_x11(false, true, false));
        assert!(!should_default_tauri_to_x11(false, false, true));
    }
}
