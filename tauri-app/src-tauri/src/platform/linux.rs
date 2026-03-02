use std::sync::{Mutex, OnceLock};

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
        // 遍历所有 /dev/input/event* 设备
        if let Ok(entries) = std::fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.to_string_lossy().contains("event") {
                    if let Ok(mut device) = evdev::Device::open(&path) {
                        let name = device.name().unwrap_or("");
                        // 跳过我们自己的虚拟输入设备
                        if name == "lcxl-web-remote-desk-mouse"
                            || name == "lcxl-web-remote-desk-keyboard"
                        {
                            continue;
                        }
                        // 尝试独占抓取物理设备
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
