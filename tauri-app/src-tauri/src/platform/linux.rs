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

struct LinuxGrabber {
    grabbed_devices: Vec<evdev::Device>,
}

static LINUX_GRABBER: OnceLock<Mutex<Option<LinuxGrabber>>> = OnceLock::new();

fn grabber_slot() -> &'static Mutex<Option<LinuxGrabber>> {
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

pub fn block_input(block: bool) -> Result<(), String> {
    if block {
        let mut guard = grabber_slot()
            .lock()
            .map_err(|e| format!("Failed to acquire grabber lock: {}", e))?;
        if guard.is_some() {
            return Ok(());
        }

        toggle_xrandr_brightness(true);

        let mut grabbed_devices = Vec::new();
        // Iterate over all /dev/input/event* devices
        if let Ok(entries) = std::fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.to_string_lossy().contains("event") {
                    if let Ok(mut device) = evdev::Device::open(&path) {
                        let name = device.name().unwrap_or("");
                        // Skip our own virtual input devices
                        if name == "lcxl-web-remote-desk-mouse"
                            || name == "lcxl-web-remote-desk-keyboard"
                        {
                            continue;
                        }
                        // Attempt to grab physical devices exclusively
                        if device.grab().is_ok() {
                            grabbed_devices.push(device);
                        }
                    }
                }
            }
        }

        log::info!("Linux: {} physical devices grabbed", grabbed_devices.len());
        *guard = Some(LinuxGrabber { grabbed_devices });
    } else {
        let mut guard = grabber_slot()
            .lock()
            .map_err(|e| format!("Failed to acquire grabber lock: {}", e))?;

        toggle_xrandr_brightness(false);

        if let Some(mut grabber) = guard.take() {
            for device in grabber.grabbed_devices.iter_mut() {
                let _ = device.ungrab();
            }
            log::info!("Linux: physical devices ungrabbed");
        }
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
