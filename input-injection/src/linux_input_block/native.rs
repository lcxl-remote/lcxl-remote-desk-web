//! Native evdev access; enumeration is a single snapshot, with no hotplug hook.
use super::{BlockReport, Device};
use evdev::{Device as EvDevice, EventType, KeyCode};
use std::{
    fs::{self, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
};

pub(super) fn acquire() -> io::Result<(Vec<NativeDevice>, BlockReport)> {
    let mut report = BlockReport::default();
    let mut devices = Vec::new();
    let mut escape_available = false;
    let mut failures = OpenFailures::default();
    for entry in fs::read_dir("/dev/input")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !event_name(name) {
            continue;
        }
        if devices.len() + report.failed + report.skipped >= 128 {
            return Err(io::Error::other("Too many input devices"));
        }
        match open(entry.path()) {
            Ok(Some((device, can_escape))) => {
                escape_available |= can_escape;
                devices.push(device);
                report.grabbed += 1;
            }
            Ok(None) => report.skipped += 1,
            Err(error) => {
                report.failed += 1;
                failures.record(&error);
            }
        }
    }
    require_escape(&report, escape_available, &failures)?;
    Ok((devices, report))
}

#[derive(Default)]
struct OpenFailures {
    permission_denied: usize,
    first_other: Option<String>,
}
impl OpenFailures {
    fn record(&mut self, error: &io::Error) {
        if error.kind() == io::ErrorKind::PermissionDenied {
            self.permission_denied += 1;
        } else if self.first_other.is_none() {
            // Keep the native failure useful without unbounded IPC messages.
            self.first_other = Some(
                error
                    .to_string()
                    .chars()
                    .take(160)
                    .map(|ch| if ch.is_control() { ' ' } else { ch })
                    .collect(),
            );
        }
    }
}

fn require_escape(
    report: &BlockReport,
    available: bool,
    failures: &OpenFailures,
) -> io::Result<()> {
    if available {
        return Ok(());
    }
    let kind = if failures.permission_denied > 0 {
        io::ErrorKind::PermissionDenied
    } else {
        io::ErrorKind::Other
    };
    let detail = failures.first_other.as_deref().unwrap_or("none");
    Err(io::Error::new(
        kind,
        format!(
            "Input control is unavailable: no acquired keyboard supports Ctrl+Alt+L; acquired={}, failed={}, permission_denied={}, first_other_error={detail}. Acquired devices are released; device permissions are not changed.",
            report.grabbed, report.failed, failures.permission_denied,
        ),
    ))
}

fn event_name(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

fn open(path: std::path::PathBuf) -> io::Result<Option<(NativeDevice, bool)>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let mut device = EvDevice::from_fd(file.into())?;
    // Retain the existing product virtual-device exclusions. Names are not an
    // authentication boundary and this backend does not claim full isolation.
    if matches!(
        device.name(),
        Some("lcxl-web-remote-desk-mouse" | "lcxl-web-remote-desk-keyboard")
    ) {
        return Ok(None);
    }
    let Some(keys) = device.supported_keys() else {
        return Ok(None);
    };
    if !keys.contains(KeyCode::KEY_A)
        && !keys.contains(KeyCode::BTN_LEFT)
        && !keys.contains(KeyCode::BTN_TOUCH)
    {
        return Ok(None);
    }
    let can_escape = keys.contains(KeyCode::KEY_L)
        && (keys.contains(KeyCode::KEY_LEFTCTRL) || keys.contains(KeyCode::KEY_RIGHTCTRL))
        && (keys.contains(KeyCode::KEY_LEFTALT) || keys.contains(KeyCode::KEY_RIGHTALT));
    if device.get_key_state()?.iter().next().is_some() {
        return Err(io::Error::other(
            "Release held keys and buttons before blocking input",
        ));
    }
    device.grab()?;
    Ok(Some((
        NativeDevice {
            device,
            chord: Chord::default(),
        },
        can_escape,
    )))
}

pub(super) struct NativeDevice {
    device: EvDevice,
    chord: Chord,
}
impl Device for NativeDevice {
    fn escape(&mut self) -> io::Result<bool> {
        match self.device.fetch_events() {
            Ok(events) => {
                let mut escape = false;
                for event in events.take(512) {
                    if event.event_type() == EventType::KEY {
                        escape |= self.chord.update(KeyCode::new(event.code()), event.value());
                    }
                }
                Ok(escape)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    }
}
impl Drop for NativeDevice {
    fn drop(&mut self) {
        let _ = self.device.ungrab();
    }
}

#[derive(Default)]
struct Chord {
    down: u8,
}
impl Chord {
    fn update(&mut self, key: KeyCode, value: i32) -> bool {
        let bit = match key {
            KeyCode::KEY_LEFTCTRL => 1,
            KeyCode::KEY_RIGHTCTRL => 2,
            KeyCode::KEY_LEFTALT => 4,
            KeyCode::KEY_RIGHTALT => 8,
            KeyCode::KEY_L => 16,
            _ => return false,
        };
        match value {
            0 => self.down &= !bit,
            1 => self.down |= bit,
            _ => return false,
        }
        self.down & 3 != 0 && self.down & 12 != 0 && self.down & 16 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_denial_is_distinct_from_absent_or_unsuitable_devices() {
        let mut failures = OpenFailures::default();
        for _ in 0..5 {
            failures.record(&io::Error::from_raw_os_error(libc::EACCES));
        }
        let report = BlockReport {
            failed: 5,
            ..Default::default()
        };
        let error = require_escape(&report, false, &failures).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("permission_denied=5"));
        let absent =
            require_escape(&BlockReport::default(), false, &OpenFailures::default()).unwrap_err();
        assert_eq!(absent.kind(), io::ErrorKind::Other);
        assert!(absent.to_string().contains("permission_denied=0"));
    }

    #[test]
    fn other_failure_is_bounded_and_partial_coverage_still_requires_escape() {
        let mut failures = OpenFailures::default();
        failures.record(&io::Error::other("Release held keys and buttons"));
        failures.record(&io::Error::other("x".repeat(10_000)));
        let report = BlockReport {
            grabbed: 1,
            failed: 2,
            skipped: 0,
        };
        let error = require_escape(&report, false, &failures).unwrap_err();
        assert!(error.to_string().contains("Release held keys and buttons"));
        assert!(error.to_string().len() < 512);
        assert!(require_escape(&report, true, &failures).is_ok());
        let mut long = OpenFailures::default();
        long.record(&io::Error::other("x".repeat(10_000)));
        assert_eq!(long.first_other.as_ref().unwrap().len(), 160);
        let mut controls = OpenFailures::default();
        controls.record(&io::Error::other("\u{0001}".repeat(10_000)));
        assert_eq!(
            controls.first_other.as_deref(),
            Some(" ".repeat(160).as_str())
        );
    }
    #[test]
    fn escape_tracks_both_modifier_sides_and_releases() {
        let mut chord = Chord::default();
        assert!(!chord.update(KeyCode::KEY_LEFTCTRL, 1));
        assert!(!chord.update(KeyCode::KEY_RIGHTCTRL, 1));
        assert!(!chord.update(KeyCode::KEY_LEFTCTRL, 0));
        assert!(!chord.update(KeyCode::KEY_RIGHTALT, 1));
        assert!(chord.update(KeyCode::KEY_L, 1));
        assert!(!chord.update(KeyCode::KEY_RIGHTALT, 0));
        assert!(!chord.update(KeyCode::KEY_A, 1));
    }
    #[test]
    fn device_name_requires_an_event_number() {
        assert!(event_name("event12"));
        for name in ["event", "myevent1", "event1.old", "event-1"] {
            assert!(!event_name(name));
        }
    }
}
