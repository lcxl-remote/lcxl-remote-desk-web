use super::worker_manager::WorkerManager;
use log::info;
#[cfg(target_os = "windows")]
use log::{error, warn};
use std::time::Duration;

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsSessionRegistration {
    pub session: desk_ipc_protocol::message::SessionKey,
    pub session_id: u32,
    pub display_name: String,
    pub station_name: String,
    pub foreground: bool,
}

#[cfg(target_os = "windows")]
pub fn windows_resident_pool_enabled() -> bool {
    std::env::var("LRD_EXPERIMENTAL_WINDOWS_RESIDENT_WORKERS")
        .ok()
        .as_deref()
        == Some("1")
}

pub async fn run_session_monitor(
    worker_mgr: WorkerManager,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Session monitor starting");

    #[cfg(target_os = "windows")]
    {
        run_windows_session_monitor(worker_mgr).await
    }

    #[cfg(not(target_os = "windows"))]
    {
        info!("Session monitor: no-op on this platform");
        let _ = worker_mgr;
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

#[cfg(target_os = "windows")]
async fn run_windows_session_monitor(
    worker_mgr: WorkerManager,
) -> Result<(), Box<dyn std::error::Error>> {
    if windows_resident_pool_enabled() {
        return run_windows_resident_session_monitor(worker_mgr).await;
    }
    let mut last_desktop_name = String::new();
    let poll_interval = Duration::from_secs(1);

    loop {
        tokio::time::sleep(poll_interval).await;

        let current_desktop = match get_current_desktop_name() {
            Ok(name) => name,
            Err(e) => {
                log::trace!("Failed to get desktop name: {}", e);
                continue;
            }
        };

        if current_desktop != last_desktop_name && !last_desktop_name.is_empty() {
            info!(
                "Desktop switch detected: {} -> {}",
                last_desktop_name, current_desktop
            );

            worker_mgr.notify_desktop_switch().await;
            tokio::time::sleep(Duration::from_millis(500)).await;

            let session_id = get_active_session_id();
            match worker_mgr
                .start_worker(session_id, Some(current_desktop.clone()))
                .await
            {
                Ok(_) => {
                    info!("New worker started for desktop: {}", current_desktop);
                }
                Err(e) => {
                    error!(
                        "Failed to start worker for desktop {}: {}",
                        current_desktop, e
                    );
                }
            }
        }

        last_desktop_name = current_desktop;
    }
}

#[cfg(target_os = "windows")]
async fn run_windows_resident_session_monitor(
    worker_mgr: WorkerManager,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::HashMap;

    info!("experimental Windows resident-session worker monitor starting");
    let mut tracked: HashMap<u32, (String, u64)> = HashMap::new();
    let mut next_generation = 1u64;
    loop {
        let observed = match enumerate_schedulable_windows_sessions() {
            Ok(observed) => observed,
            Err(error) => {
                warn!("Windows session enumeration failed; retaining current workers: {error}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let mut registrations = Vec::with_capacity(observed.len());
        let mut next_tracked = HashMap::new();
        for observed in observed {
            let generation = match tracked.get(&observed.session_id) {
                Some((user, generation)) if user == &observed.user => *generation,
                _ => {
                    let generation = next_generation;
                    next_generation = next_generation.saturating_add(1);
                    generation
                }
            };
            next_tracked.insert(observed.session_id, (observed.user.clone(), generation));
            registrations.push(WindowsSessionRegistration {
                session: desk_ipc_protocol::message::SessionKey {
                    platform_session_id: observed.session_id.to_string(),
                    session_generation: generation,
                },
                session_id: observed.session_id,
                display_name: format!("{} (session {})", observed.user, observed.session_id),
                station_name: observed.station_name,
                foreground: observed.foreground,
            });
        }
        tracked = next_tracked;
        worker_mgr
            .reconcile_windows_resident_workers(registrations)
            .await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(target_os = "windows")]
struct ObservedWindowsSession {
    session_id: u32,
    user: String,
    station_name: String,
    foreground: bool,
}

#[cfg(target_os = "windows")]
fn enumerate_schedulable_windows_sessions() -> Result<Vec<ObservedWindowsSession>, String> {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSActive, WTSConnected, WTSDomainName, WTSEnumerateSessionsW,
        WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSUserName,
        WTSWinStationName,
    };
    use windows::core::PWSTR;

    unsafe fn query(
        session_id: u32,
        class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
    ) -> Option<String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        if unsafe {
            WTSQuerySessionInformationW(
                Some(WTS_CURRENT_SERVER_HANDLE),
                session_id,
                class,
                &mut buffer,
                &mut bytes,
            )
        }
        .is_err()
            || buffer.is_null()
        {
            return None;
        }
        let len = (bytes as usize / 2).saturating_sub(1);
        let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(buffer.0, len) });
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        Some(value)
    }

    unsafe {
        let mut session_info_ptr = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut session_info_ptr,
            &mut count,
        )
        .is_err()
            || session_info_ptr.is_null()
        {
            return Err("WTSEnumerateSessionsW returned no snapshot".to_string());
        }
        let active_console = WTSGetActiveConsoleSessionId();
        let mut result = Vec::new();
        for session in std::slice::from_raw_parts(session_info_ptr, count as usize) {
            if session.SessionId == 0
                || (session.State != WTSActive && session.State != WTSConnected)
            {
                continue;
            }
            let Some(user_name) =
                query(session.SessionId, WTSUserName).filter(|name| !name.trim().is_empty())
            else {
                continue;
            };
            let domain = query(session.SessionId, WTSDomainName).unwrap_or_default();
            let user = if domain.trim().is_empty() {
                user_name
            } else {
                format!(r"{domain}\{user_name}")
            };
            result.push(ObservedWindowsSession {
                session_id: session.SessionId,
                user,
                station_name: query(session.SessionId, WTSWinStationName)
                    .unwrap_or_else(|| "WinSta0".to_string()),
                foreground: session.SessionId == active_console || session.State == WTSActive,
            });
        }
        WTSFreeMemory(session_info_ptr.cast());
        Ok(result)
    }
}

#[cfg(target_os = "windows")]
pub fn get_current_desktop_name() -> Result<String, String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    };

    unsafe {
        let desktop = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x0001),
        )
        .map_err(|e| format!("OpenInputDesktop failed: {}", e))?;

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

        // Always close the desktop handle before returning.
        let _ = CloseDesktop(desktop);

        if result.is_err() {
            return Err("GetUserObjectInformationW failed".to_string());
        }

        let len = (needed as usize / 2).saturating_sub(1);
        let name = String::from_utf16_lossy(&name_buf[..len]);
        Ok(name)
    }
}

#[cfg(not(target_os = "windows"))]
pub fn get_active_session_id() -> u32 {
    0
}

#[cfg(target_os = "windows")]
pub fn get_active_session_id() -> u32 {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSGetActiveConsoleSessionId,
    };

    unsafe {
        let mut session_info_ptr = std::ptr::null_mut();
        let mut count: u32 = 0;

        let result = WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut session_info_ptr,
            &mut count,
        );

        if result.is_err() || session_info_ptr.is_null() {
            log::warn!("WTSEnumerateSessionsW failed, falling back to console session");
            return WTSGetActiveConsoleSessionId();
        }

        let sessions = std::slice::from_raw_parts(session_info_ptr, count as usize);
        let mut active_session_id = None;

        for session in sessions {
            log::debug!(
                "Session enumeration: id={}, state={:?}",
                session.SessionId,
                session.State
            );
            if session.State == WTSActive && session.SessionId != 0 {
                // Session 0 hosts system services, never a user desktop.
                active_session_id = Some(session.SessionId);
                break;
            }
        }

        WTSFreeMemory(session_info_ptr as *mut _);

        match active_session_id {
            Some(id) => {
                log::info!("Found active session via WTSEnumerateSessionsW: {}", id);
                id
            }
            None => {
                let fallback = WTSGetActiveConsoleSessionId();
                log::warn!(
                    "No active session found via enumeration, falling back to console session: {}",
                    fallback
                );
                fallback
            }
        }
    }
}
