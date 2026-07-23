//! Worker-side input-desktop monitor.
//!
//! The daemon's `session_monitor` calls `OpenInputDesktop` from inside the
//! SYSTEM service window station (`Service-0x0-3e7$`), so it cannot observe a
//! user-session desktop switch — UAC's transition Default → Winlogon is
//! invisible from session 0. The worker, however, runs in the user session's
//! `WinSta0` and can observe the switch via the same API.
//!
//! `OpenInputDesktop` from a user-token process succeeds for unrestricted
//! desktops (`Default`, `Screen-saver`) but returns `ERROR_ACCESS_DENIED`
//! when the input desktop is `Winlogon` (the secure desktop UAC switches
//! to). The watcher treats that error as "input desktop is restricted"
//! and reports the drift as if the name were `Winlogon` — that's the
//! shape the daemon uses to decide what to do next.
//!
//! Lifetime: the watcher polls forever for the lifetime of the worker. It
//! emits one `WorkerToService::DesktopChanged` per *transition* — repeated
//! observations of the same drifted state are suppressed, and a return to
//! the bound desktop clears that suppression so a subsequent UAC trip is
//! reported again.

use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Poll interval for the input-desktop watcher.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Conventional name we use to surface "input desktop is restricted from
/// this process" up to the daemon. Every modern Windows installation
/// names UAC's secure desktop "Winlogon" so this is the right inference
/// 99 % of the time; if the OS ever uses a different name the daemon-
/// side switch handler will simply log + skip.
pub const RESTRICTED_DESKTOP_NAME: &str = "Winlogon";

/// Spawn the platform monitor on a dedicated thread.
///
/// `bound_desktop` is the desktop name the daemon launched this worker on
/// (delivered via `WorkerInitPayload.desktop_name`).
pub fn spawn(bound_desktop: Option<String>, tx: UnboundedSender<String>) {
    let bound = bound_desktop.unwrap_or_else(|| "Default".to_string());
    if std::thread::Builder::new()
        .name("worker-desktop-monitor".to_string())
        .spawn(move || run_loop(&bound, tx))
        .is_err()
    {
        log::warn!("[DesktopMonitor] failed to spawn watcher thread");
    }
}

/// Outcome of one `OpenInputDesktop` poll. Distinguishing `Restricted`
/// from `OtherError` matters because UAC produces the former
/// deterministically and we want to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDesktopProbe {
    /// We could open the input desktop and read its name.
    Name(String),
    /// `OpenInputDesktop` failed with ERROR_ACCESS_DENIED — the input
    /// desktop has switched to one this process cannot open (typically
    /// `Winlogon` during UAC).
    Restricted,
    /// Any other failure (rare). Surfaces the formatted error so a future
    /// occurrence shows up in the worker log.
    OtherError(String),
}

fn run_loop(bound: &str, tx: UnboundedSender<String>) {
    log::info!("[DesktopMonitor] watching for input-desktop drift from '{bound}'");
    let mut state = DesktopMonitorState::default();

    loop {
        std::thread::sleep(POLL_INTERVAL);
        if tx.is_closed() {
            return;
        }

        if let Some(observed) = state.observe(bound, probe_input_desktop()) {
            if tx.send(observed).is_err() {
                return;
            }
        }
    }
}

#[derive(Default)]
struct DesktopMonitorState {
    last_reported: Option<String>,
    last_logged_other_err: Option<String>,
}

impl DesktopMonitorState {
    fn observe(&mut self, bound: &str, probe: InputDesktopProbe) -> Option<String> {
        let observed = match probe {
            InputDesktopProbe::Name(name) => name,
            InputDesktopProbe::Restricted => RESTRICTED_DESKTOP_NAME.to_string(),
            InputDesktopProbe::OtherError(msg) => {
                // Log only on transitions between distinct errors to avoid
                // one warning per poll when the platform API is unavailable.
                if self.last_logged_other_err.as_deref() != Some(msg.as_str()) {
                    log::warn!("[DesktopMonitor] OpenInputDesktop error: {msg}");
                    self.last_logged_other_err = Some(msg);
                }
                return None;
            }
        };
        self.last_logged_other_err = None;

        if observed == bound {
            if let Some(previous) = self.last_reported.take() {
                log::info!(
                    "[DesktopMonitor] input desktop returned to bound '{bound}' from '{previous}'"
                );
            }
            return None;
        }

        if self.last_reported.as_deref() == Some(observed.as_str()) {
            return None;
        }

        log::info!("[DesktopMonitor] desktop drift detected: '{bound}' -> '{observed}'");
        self.last_reported = Some(observed.clone());
        Some(observed)
    }
}

/// Probe the current input desktop. Returns one of [`InputDesktopProbe`]'s
/// three branches so the caller can react to access-denied (UAC) without
/// pretending nothing happened.
#[cfg(target_os = "windows")]
pub fn probe_input_desktop() -> InputDesktopProbe {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    };

    // Win32 ERROR_ACCESS_DENIED (5) wrapped as an HRESULT facility-Win32
    // value: 0x80070005.
    const E_ACCESSDENIED_HRESULT: i32 = 0x8007_0005u32 as i32;

    unsafe {
        let desktop = match OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x0001), // DESKTOP_READOBJECTS — name only.
        ) {
            Ok(d) => d,
            Err(e) => {
                let hr = e.code().0;
                if hr == E_ACCESSDENIED_HRESULT {
                    return InputDesktopProbe::Restricted;
                }
                return InputDesktopProbe::OtherError(format!("OpenInputDesktop: {e}"));
            }
        };

        let mut name_buf = vec![0u16; 256];
        let mut needed = 0u32;

        let handle = HANDLE(desktop.0);
        let result = GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(name_buf.as_mut_ptr() as *mut _),
            (name_buf.len() * 2) as u32,
            Some(&mut needed),
        );

        let _ = CloseDesktop(desktop);

        match result {
            Ok(()) => {
                let len = (needed as usize / 2).saturating_sub(1);
                InputDesktopProbe::Name(String::from_utf16_lossy(&name_buf[..len]))
            }
            Err(e) => InputDesktopProbe::OtherError(format!("GetUserObjectInformationW: {e}")),
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn probe_input_desktop() -> InputDesktopProbe {
    InputDesktopProbe::OtherError("not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// `RESTRICTED_DESKTOP_NAME` is the wire name shipped to the daemon
    /// for the access-denied case. Locked down with a test so a rename
    /// has to update the daemon-side handler in lock-step.
    #[test]
    fn restricted_name_is_winlogon() {
        assert_eq!(RESTRICTED_DESKTOP_NAME, "Winlogon");
    }

    /// When the receiver is dropped before the watcher detects a change,
    /// `run_loop` must observe `tx.is_closed()` and exit.
    #[test]
    fn run_loop_exits_when_receiver_dropped() {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        drop(rx);
        let handle = std::thread::Builder::new()
            .name("desktop-monitor-test".to_string())
            .spawn(move || run_loop("Default", tx))
            .unwrap();
        let start = std::time::Instant::now();
        while !handle.is_finished() && start.elapsed() < Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            handle.is_finished(),
            "run_loop did not exit after receiver was dropped"
        );
        handle.join().unwrap();
    }

    /// The production state machine emits once per drift transition and
    /// returning to the bound desktop re-arms the same drift for later.
    #[test]
    fn dedupes_drift_state_across_polls() {
        let probes = vec![
            InputDesktopProbe::Name("Default".to_string()), // matches bound
            InputDesktopProbe::Restricted,                  // first drift -> emit
            InputDesktopProbe::Restricted,                  // dedup -> no emit
            InputDesktopProbe::Name("Default".to_string()), // returned
            InputDesktopProbe::Restricted,                  // re-armed -> emit
        ];

        let mut state = DesktopMonitorState::default();
        let observed = probes
            .into_iter()
            .filter_map(|probe| state.observe("Default", probe))
            .collect::<Vec<_>>();

        assert_eq!(
            observed,
            vec!["Winlogon".to_string(), "Winlogon".to_string()]
        );
    }

    #[test]
    fn distinct_errors_are_deduplicated_until_a_successful_probe() {
        let mut state = DesktopMonitorState::default();

        assert_eq!(
            state.observe("Default", InputDesktopProbe::OtherError("failure".into())),
            None
        );
        assert_eq!(
            state.observe("Default", InputDesktopProbe::OtherError("failure".into())),
            None
        );
        assert_eq!(
            state.observe("Default", InputDesktopProbe::Name("Default".to_string())),
            None
        );
        assert_eq!(state.last_logged_other_err, None);
    }
}
