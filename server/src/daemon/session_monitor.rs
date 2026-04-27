use super::worker_manager::WorkerManager;
use log::{error, info};
use std::time::Duration;

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

            let browser_ids = worker_mgr.notify_desktop_switch().await;
            tokio::time::sleep(Duration::from_millis(500)).await;

            let session_id = get_active_session_id();
            match worker_mgr
                .start_worker(session_id, Some(current_desktop.clone()), browser_ids)
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

#[cfg(target_os = "windows")]
pub fn get_active_session_id() -> u32 {
    use windows::Win32::System::RemoteDesktop::{
        WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTS_CURRENT_SERVER_HANDLE,
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
                // Session 0 是系统服务 Session，跳过
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
