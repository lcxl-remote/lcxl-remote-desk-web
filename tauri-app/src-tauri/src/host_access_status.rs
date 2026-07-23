use lcxl_remote_desk_server::{
    daemon::{
        local_access_control::HostAccessControlAction,
        local_access_control_transport::{endpoint_for_config, execute_native},
    },
    host_control::{HostAccessSnapshot, HostRemoteAccessMode},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use tauri::{
    AppHandle, Listener, LogicalSize, Manager, Monitor, PhysicalPosition, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;
use url::Url;

const STATUS_WINDOW_WIDTH: f64 = 460.0;
const STATUS_WINDOW_COLLAPSED_HEIGHT: f64 = 250.0;
const STATUS_WINDOW_LOCKED_HEIGHT: f64 = 390.0;
const STATUS_WINDOW_EXPANDED_HEIGHT: f64 = 620.0;
const STATUS_WINDOW_MARGIN: i32 = 16;
static ACTIVE_SESSION_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
const MONITOR_RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
pub(crate) const STATUS_LABEL_PREFIX: &str = "host-access-status-";

#[derive(Debug, Clone)]
pub enum HostAccessStatusCommand {
    Snapshot(HostAccessSnapshot),
    Reset,
}

pub struct HostAccessStatusManager {
    app_handle: AppHandle,
    frontend_url: String,
    control_endpoint: PathBuf,
    control_config: String,
}

const CONTROL_EVENT: &str = "lcxl-host-access-control";
const CONTROL_RESULT_EVENT: &str = "lcxl-host-access-control-result";

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum UiControlAction {
    Lock,
    Unlock { expected_version: u64 },
    Disconnect { connection_id: String },
}

#[derive(Debug, Deserialize)]
struct UiControlRequest {
    request_id: String,
    #[serde(flatten)]
    action: UiControlAction,
}

#[derive(Debug, Serialize)]
struct UiControlResult {
    request_id: String,
    ok: bool,
    error: Option<String>,
}

#[derive(Default)]
struct StatusState {
    epoch: Option<String>,
    revision: u64,
    labels: BTreeSet<String>,
    active: bool,
    snapshot: Option<HostAccessSnapshot>,
}

impl HostAccessStatusManager {
    pub fn new(app_handle: AppHandle, frontend_url: String, config_file_path: &str) -> Self {
        Self {
            app_handle,
            frontend_url,
            control_endpoint: endpoint_for_config(config_file_path),
            control_config: config_file_path.to_string(),
        }
    }

    pub fn start(&self, receiver: std::sync::mpsc::Receiver<HostAccessStatusCommand>) {
        install_control_listener(
            &self.app_handle,
            self.control_endpoint.clone(),
            self.control_config.clone(),
        );
        let app_handle = self.app_handle.clone();
        let frontend_url = self.frontend_url.clone();
        std::thread::spawn(move || {
            let mut state = StatusState::default();
            loop {
                match receiver.recv_timeout(MONITOR_RECONCILE_INTERVAL) {
                    Ok(command) => match command {
                        HostAccessStatusCommand::Snapshot(snapshot) => {
                            if !accept_snapshot(&state, &snapshot) {
                                continue;
                            }
                            let was_active = state.active;
                            state.epoch = Some(snapshot.epoch.clone());
                            state.revision = snapshot.revision;
                            state.active = snapshot_should_display(&snapshot);
                            state.snapshot = Some(snapshot.clone());
                            crate::HOST_ACCESS_BLOCKS_EXIT
                                .store(state.active, std::sync::atomic::Ordering::SeqCst);
                            if !state.active {
                                destroy_all(&app_handle, &mut state.labels);
                                update_tray(&app_handle, false, 0);
                                continue;
                            }

                            let monitors = resolve_physical_monitors(&app_handle);
                            reconcile_windows(
                                &app_handle,
                                &frontend_url,
                                &snapshot,
                                &monitors,
                                &mut state.labels,
                            );
                            update_tray(&app_handle, true, snapshot.total_session_count as usize);
                            if !was_active
                                && snapshot.remote_access.mode == HostRemoteAccessMode::Unlocked
                            {
                                show_started_notification(&app_handle);
                            }
                        }
                        HostAccessStatusCommand::Reset => {
                            crate::HOST_ACCESS_BLOCKS_EXIT
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            destroy_all(&app_handle, &mut state.labels);
                            update_tray(&app_handle, false, 0);
                            state = StatusState::default();
                        }
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if state.active
                            && let Some(snapshot) = state.snapshot.as_ref()
                        {
                            let monitors = resolve_physical_monitors(&app_handle);
                            reconcile_windows(
                                &app_handle,
                                &frontend_url,
                                snapshot,
                                &monitors,
                                &mut state.labels,
                            );
                            update_tray(&app_handle, true, snapshot.total_session_count as usize);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        destroy_all(&app_handle, &mut state.labels);
                        update_tray(&app_handle, false, 0);
                        crate::HOST_ACCESS_BLOCKS_EXIT
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }
}

fn install_control_listener(app_handle: &AppHandle, endpoint: PathBuf, config_file_path: String) {
    let handle = app_handle.clone();
    app_handle.listen(CONTROL_EVENT, move |event| {
        let request = match serde_json::from_str::<UiControlRequest>(event.payload()) {
            Ok(request) => request,
            Err(error) => {
                log::warn!("Rejected malformed host-access control event: {error}");
                return;
            }
        };
        if request.request_id.len() > 128
            || request.request_id.is_empty()
            || matches!(
                &request.action,
                UiControlAction::Disconnect { connection_id } if connection_id.is_empty() || connection_id.len() > 256
            )
        {
            dispatch_control_result(
                &handle,
                &UiControlResult {
                    request_id: request.request_id,
                    ok: false,
                    error: Some("invalid local control request".to_string()),
                },
            );
            return;
        }

        let endpoint = endpoint.clone();
        let config_file_path = config_file_path.clone();
        let handle = handle.clone();
        std::thread::spawn(move || {
            let request_id = request.request_id.clone();
            let result = execute_ui_control(&endpoint, &config_file_path, request);
            dispatch_control_result(
                &handle,
                &UiControlResult {
                    request_id,
                    ok: result.is_ok(),
                    error: result.err().map(|error| format!("{error:#}")),
                },
            );
        });
    });
}

fn execute_ui_control(
    endpoint: &std::path::Path,
    config_file_path: &str,
    request: UiControlRequest,
) -> anyhow::Result<()> {
    let action = match request.action {
        UiControlAction::Lock => {
            if !confirm_safety_action(&rust_i18n::t!("host_access_lock_confirm")) {
                anyhow::bail!("lock cancelled");
            }
            HostAccessControlAction::LockAll
        }
        UiControlAction::Disconnect { connection_id } => {
            let suffix = connection_id
                .chars()
                .rev()
                .take(8)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            let message = format!(
                "{}\n\n{}: …{suffix}",
                rust_i18n::t!("host_access_disconnect_confirm"),
                rust_i18n::t!("host_access_connection")
            );
            if !confirm_safety_action(&message) {
                anyhow::bail!("disconnect cancelled");
            }
            HostAccessControlAction::DisconnectConnection { connection_id }
        }
        UiControlAction::Unlock { expected_version } => {
            if !desk_utils::permission::is_admin() {
                return execute_elevated_unlock(config_file_path, expected_version);
            }
            HostAccessControlAction::Unlock { expected_version }
        }
    };
    actix_rt::System::new().block_on(async move {
        execute_native(endpoint, request.request_id, action)
            .await
            .map(|_| ())
    })
}

#[cfg(target_os = "windows")]
fn confirm_safety_action(message: &str) -> bool {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO, MessageBoxW,
    };
    use windows::core::PCWSTR;

    let text: Vec<u16> = std::ffi::OsStr::new(message)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let title_text = rust_i18n::t!("host_access_dialog_title");
    let title: Vec<u16> = std::ffi::OsStr::new(title_text.as_ref())
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND,
        ) == IDYES
    }
}

#[cfg(target_os = "linux")]
fn confirm_safety_action(message: &str) -> bool {
    std::process::Command::new("zenity")
        .arg("--question")
        .arg(format!(
            "--title={}",
            rust_i18n::t!("host_access_dialog_title")
        ))
        .arg(format!("--text={message}"))
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "macos")]
fn confirm_safety_action(message: &str) -> bool {
    let cancel = serde_json::to_string(rust_i18n::t!("button_cancel").as_ref()).unwrap();
    let continue_text = serde_json::to_string(rust_i18n::t!("button_continue").as_ref()).unwrap();
    let script = format!(
        "display dialog {} with title {} buttons {{{cancel}, {continue_text}}} default button {cancel} with icon caution",
        serde_json::to_string(message).unwrap_or_else(|_| "\"Confirm action?\"".into()),
        serde_json::to_string(rust_i18n::t!("host_access_dialog_title").as_ref()).unwrap()
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .is_ok_and(|status| status.success())
}

fn dispatch_control_result(app_handle: &AppHandle, result: &UiControlResult) {
    let Ok(json) = serde_json::to_string(result) else {
        return;
    };
    let script = format!(
        "window.dispatchEvent(new CustomEvent('{CONTROL_RESULT_EVENT}', {{ detail: {json} }}));"
    );
    for (label, window) in app_handle.webview_windows() {
        if label.starts_with(STATUS_LABEL_PREFIX)
            && let Err(error) = window.eval(&script)
        {
            log::warn!("Failed to deliver host-access control result: {error}");
        }
    }
}

fn sidecar_path() -> PathBuf {
    let name = if cfg!(target_os = "windows") {
        "lcxl-remote-desk-server.exe"
    } else {
        "lcxl-remote-desk-server"
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn absolute_config_path(config_file_path: &str) -> PathBuf {
    std::path::absolute(config_file_path).unwrap_or_else(|_| PathBuf::from(config_file_path))
}

#[cfg(target_os = "windows")]
fn execute_elevated_unlock(config_file_path: &str, expected_version: u64) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
        UI::{
            Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_HIDE,
        },
    };
    use windows::core::PCWSTR;

    let sidecar = sidecar_path();
    let config = crate::quote_cmd_arg(&absolute_config_path(config_file_path).to_string_lossy());
    let params =
        format!("access --config-file-path {config} unlock --expected-version {expected_version}");
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = sidecar.as_os_str().encode_wide().chain(Some(0)).collect();
    let parameters: Vec<u16> = params.encode_utf16().chain(Some(0)).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info)? };
    if info.hProcess.is_invalid() {
        anyhow::bail!("elevated unlock helper did not start");
    }
    let wait = unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
    if wait != WAIT_OBJECT_0 {
        unsafe {
            let _ = CloseHandle(info.hProcess);
        }
        anyhow::bail!("failed waiting for elevated unlock helper");
    }
    let mut exit_code = 1u32;
    unsafe {
        GetExitCodeProcess(info.hProcess, &mut exit_code)?;
        let _ = CloseHandle(info.hProcess);
    }
    if exit_code != 0 {
        anyhow::bail!("elevated unlock was cancelled or failed (exit code {exit_code})");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn execute_elevated_unlock(config_file_path: &str, expected_version: u64) -> anyhow::Result<()> {
    let status = std::process::Command::new("pkexec")
        .arg(sidecar_path())
        .arg("access")
        .arg("--config-file-path")
        .arg(absolute_config_path(config_file_path))
        .arg("unlock")
        .arg("--expected-version")
        .arg(expected_version.to_string())
        .status()?;
    if !status.success() {
        anyhow::bail!("elevated unlock was cancelled or failed");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn execute_elevated_unlock(config_file_path: &str, expected_version: u64) -> anyhow::Result<()> {
    // `quoted form of` is implemented here without invoking a shell before the
    // OS authentication dialog. The resulting shell command contains only
    // single-quoted literal arguments.
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
    let command = format!(
        "{} access --config-file-path {} unlock --expected-version {}",
        shell_quote(&sidecar_path().to_string_lossy()),
        shell_quote(&absolute_config_path(config_file_path).to_string_lossy()),
        expected_version
    );
    let script = format!(
        "do shell script {} with administrator privileges",
        serde_json::to_string(&command)?
    );
    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()?;
    if !status.success() {
        anyhow::bail!("elevated unlock was cancelled or failed");
    }
    Ok(())
}

fn accept_snapshot(state: &StatusState, snapshot: &HostAccessSnapshot) -> bool {
    match state.epoch.as_deref() {
        Some(epoch) if epoch == snapshot.epoch => snapshot.revision >= state.revision,
        _ => true,
    }
}

fn snapshot_should_display(snapshot: &HostAccessSnapshot) -> bool {
    snapshot.remote_access.mode != HostRemoteAccessMode::Unlocked
        || (snapshot.indicator_enabled && snapshot.total_session_count > 0)
}

fn resolve_physical_monitors(app_handle: &AppHandle) -> Vec<Monitor> {
    let virtual_name = crate::platform::virtual_display_name();
    let monitors = app_handle.available_monitors().unwrap_or_default();
    monitors
        .into_iter()
        .filter(|monitor| {
            !virtual_name.as_ref().is_some_and(|virtual_name| {
                monitor
                    .name()
                    .is_some_and(|name| name.eq_ignore_ascii_case(virtual_name))
            })
        })
        .collect()
}

fn reconcile_windows(
    app_handle: &AppHandle,
    frontend_url: &str,
    snapshot: &HostAccessSnapshot,
    monitors: &[Monitor],
    labels: &mut BTreeSet<String>,
) {
    let desired: BTreeSet<String> = (0..monitors.len())
        .map(|index| format!("{STATUS_LABEL_PREFIX}{index}"))
        .collect();

    for stale in labels.difference(&desired).cloned().collect::<Vec<_>>() {
        destroy_window(app_handle, &stale);
        labels.remove(&stale);
    }

    for index in 0..monitors.len() {
        let label = format!("{STATUS_LABEL_PREFIX}{index}");
        if let Some(window) = app_handle.get_webview_window(&label) {
            dispatch_snapshot(&window, snapshot);
            constrain_window_to_monitor(&window, monitors.get(index));
            let _ = window.show();
            continue;
        }
        match build_status_window(
            app_handle,
            frontend_url,
            &label,
            snapshot,
            monitors.get(index),
        ) {
            Ok(()) => {
                labels.insert(label);
            }
            Err(error) => log::error!("Failed to build host-access window {label}: {error}"),
        }
    }
}

fn build_status_window(
    app_handle: &AppHandle,
    frontend_url: &str,
    label: &str,
    snapshot: &HostAccessSnapshot,
    monitor: Option<&Monitor>,
) -> Result<(), String> {
    let mut url = Url::parse(frontend_url)
        .and_then(|base| base.join("host-access-status"))
        .map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("tauri", "1");
    let initial = snapshot.clone();
    let window = WebviewWindowBuilder::new(app_handle, label, WebviewUrl::External(url))
        .title(rust_i18n::t!("remote_access_status_title"))
        .inner_size(STATUS_WINDOW_WIDTH, STATUS_WINDOW_COLLAPSED_HEIGHT)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .minimizable(false)
        .focused(false)
        .visible(false)
        .on_document_title_changed(|window, title| {
            let height = match title.as_str() {
                "lcxl-host-access:expanded" => STATUS_WINDOW_EXPANDED_HEIGHT,
                "lcxl-host-access:collapsed" => STATUS_WINDOW_COLLAPSED_HEIGHT,
                "lcxl-host-access:locked" => STATUS_WINDOW_LOCKED_HEIGHT,
                _ => return,
            };
            let _ = window.set_size(LogicalSize::new(STATUS_WINDOW_WIDTH, height));
        })
        .on_page_load(move |window, event| {
            if let tauri::webview::PageLoadEvent::Finished = event.event() {
                crate::inject_native_bridge_state(&window);
                dispatch_snapshot(&window, &initial);
                let _ = window.show();
            }
        })
        .build()
        .map_err(|error| error.to_string())?;

    window.on_window_event(|event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
        }
    });

    position_window(&window, monitor);
    let _ = window.set_always_on_top(true);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhysicalRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl PhysicalRect {
    fn from_origin_size(x: i32, y: i32, width: u32, height: u32) -> Self {
        let width = i32::try_from(width).unwrap_or(i32::MAX);
        let height = i32::try_from(height).unwrap_or(i32::MAX);
        Self {
            left: x,
            top: y,
            right: x.saturating_add(width),
            bottom: y.saturating_add(height),
        }
    }

    fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }
}

fn constrain_origin_to_monitor(window: PhysicalRect, monitor: PhysicalRect) -> (i32, i32) {
    let max_x = monitor
        .right
        .saturating_sub(window.width())
        .max(monitor.left);
    let max_y = monitor
        .bottom
        .saturating_sub(window.height())
        .max(monitor.top);
    (
        window.left.clamp(monitor.left, max_x),
        window.top.clamp(monitor.top, max_y),
    )
}

fn constrain_window_to_monitor(window: &WebviewWindow, monitor: Option<&Monitor>) {
    let Some(monitor) = monitor else {
        return;
    };
    let (Ok(position), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        position_window(window, Some(monitor));
        return;
    };
    let window_rect =
        PhysicalRect::from_origin_size(position.x, position.y, size.width, size.height);
    let monitor = PhysicalRect::from_origin_size(
        monitor.position().x,
        monitor.position().y,
        monitor.size().width,
        monitor.size().height,
    );
    let (x, y) = constrain_origin_to_monitor(window_rect, monitor);
    if (x, y) != (position.x, position.y) {
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
}

fn position_window(window: &WebviewWindow, monitor: Option<&Monitor>) {
    if let Some(monitor) = monitor {
        let scale = monitor.scale_factor();
        let position = monitor.position();
        let size = monitor.size();
        let width = (STATUS_WINDOW_WIDTH * scale) as i32;
        let x = position.x + size.width as i32 - width - STATUS_WINDOW_MARGIN;
        let y = position.y + STATUS_WINDOW_MARGIN;
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
}

fn dispatch_snapshot(window: &WebviewWindow, snapshot: &HostAccessSnapshot) {
    let Ok(json) = serde_json::to_string(snapshot) else {
        return;
    };
    let Ok(encoded) = serde_json::to_string(&json) else {
        return;
    };
    let script = format!(
        "window.__lcxlHostAccessSnapshot = JSON.parse({encoded}); \
         window.dispatchEvent(new CustomEvent('lcxl-host-access-snapshot', \
         {{ detail: window.__lcxlHostAccessSnapshot }}));"
    );
    if let Err(error) = window.eval(&script) {
        log::warn!("Failed to update host-access window: {error}");
    }
}

fn destroy_all(app_handle: &AppHandle, labels: &mut BTreeSet<String>) {
    for label in std::mem::take(labels) {
        destroy_window(app_handle, &label);
    }
}

fn destroy_window(app_handle: &AppHandle, label: &str) {
    if let Some(window) = app_handle.get_webview_window(label) {
        let _ = window.destroy();
    }
}

fn update_tray(app_handle: &AppHandle, active: bool, session_count: usize) {
    ACTIVE_SESSION_COUNT.store(
        if active { session_count } else { 0 },
        std::sync::atomic::Ordering::SeqCst,
    );
    if let Some(tray) = app_handle.tray_by_id(crate::MAIN_TRAY_ID) {
        let tooltip = if active {
            rust_i18n::t!("tray_active_sessions", count = session_count).to_string()
        } else {
            rust_i18n::t!("app_title").to_string()
        };
        let _ = tray.set_tooltip(Some(tooltip));
        if let Some(base) = app_handle.default_window_icon() {
            if active {
                let icon = add_activity_badge(base);
                let _ = tray.set_icon(Some(icon));
            } else {
                let _ = tray.set_icon(Some(base.clone()));
            }
        }
    }
}

pub(crate) fn refresh_tray_locale(app_handle: &AppHandle) {
    let count = ACTIVE_SESSION_COUNT.load(std::sync::atomic::Ordering::SeqCst);
    update_tray(app_handle, count > 0, count);
}

fn add_activity_badge(base: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let width = base.width();
    let height = base.height();
    let mut rgba = base.rgba().to_vec();
    let radius = (width.min(height) / 5).max(2) as i32;
    let center_x = width as i32 - radius - 1;
    let center_y = height as i32 - radius - 1;
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let dx = x - center_x;
            let dy = y - center_y;
            if dx * dx + dy * dy <= radius * radius {
                let offset = ((y as u32 * width + x as u32) * 4) as usize;
                rgba[offset..offset + 4].copy_from_slice(&[245, 158, 11, 255]);
            }
        }
    }
    tauri::image::Image::new_owned(rgba, width, height)
}

fn show_started_notification(app_handle: &AppHandle) {
    let _ = app_handle
        .notification()
        .builder()
        .title(rust_i18n::t!("host_access_title"))
        .body(rust_i18n::t!("host_access_started"))
        .show();
}

pub(crate) fn show_status_windows(app_handle: &AppHandle) {
    for (label, window) in app_handle.webview_windows() {
        if label.starts_with(STATUS_LABEL_PREFIX) {
            let _ = window.show();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(epoch: &str, revision: u64) -> HostAccessSnapshot {
        HostAccessSnapshot {
            epoch: epoch.to_string(),
            revision,
            indicator_enabled: true,
            total_session_count: 0,
            sessions: Vec::new(),
            remote_access: lcxl_remote_desk_server::host_control::HostRemoteAccessStatus::default(),
        }
    }

    #[test]
    fn locked_status_is_visible_even_when_indicator_is_disabled() {
        let mut value = snapshot("epoch", 1);
        value.indicator_enabled = false;
        value.remote_access.mode = HostRemoteAccessMode::Locked;

        assert!(snapshot_should_display(&value));
    }

    #[test]
    fn same_epoch_rejects_old_revision_and_accepts_equal_replay() {
        let state = StatusState {
            epoch: Some("e1".to_string()),
            revision: 4,
            ..Default::default()
        };
        assert!(!accept_snapshot(&state, &snapshot("e1", 3)));
        assert!(accept_snapshot(&state, &snapshot("e1", 4)));
    }

    #[test]
    fn new_epoch_accepts_low_revision() {
        let state = StatusState {
            epoch: Some("old".to_string()),
            revision: 99,
            ..Default::default()
        };
        assert!(accept_snapshot(&state, &snapshot("new", 0)));
    }

    #[test]
    fn remote_capability_only_grants_dragging_to_loopback_status_windows() {
        let capability: serde_json::Value = serde_json::from_str(include_str!(
            "../capabilities/host-access-status-remote.json"
        ))
        .expect("host access status capability must be valid JSON");

        assert_eq!(
            capability["windows"],
            serde_json::json!(["host-access-status-*"])
        );
        assert_eq!(
            capability["remote"]["urls"],
            serde_json::json!(["http://127.0.0.1:*", "http://[\\:\\:1]:*"])
        );
        assert_eq!(
            capability["permissions"],
            serde_json::json!(["core:window:allow-start-dragging", "core:event:allow-emit"])
        );
    }

    #[test]
    fn activity_badge_changes_lower_right_pixels() {
        let base = tauri::image::Image::new_owned(vec![0; 16 * 16 * 4], 16, 16);
        let badged = add_activity_badge(&base);
        assert_ne!(badged.rgba(), base.rgba());
        assert!(
            badged
                .rgba()
                .chunks_exact(4)
                .any(|pixel| pixel == [245, 158, 11, 255])
        );
    }

    #[test]
    fn position_constraint_preserves_a_position_inside_the_assigned_monitor() {
        let monitor = PhysicalRect::from_origin_size(0, 0, 1920, 1080);
        let moved_window = PhysicalRect::from_origin_size(500, 400, 460, 154);

        assert_eq!(
            constrain_origin_to_monitor(moved_window, monitor),
            (500, 400)
        );
    }

    #[test]
    fn position_constraint_keeps_each_window_on_its_assigned_monitor() {
        let left_monitor = PhysicalRect::from_origin_size(0, 0, 1920, 1080);
        let dragged_to_right_monitor = PhysicalRect::from_origin_size(2200, 400, 460, 154);
        let dragged_above_monitor = PhysicalRect::from_origin_size(300, -140, 460, 154);

        assert_eq!(
            constrain_origin_to_monitor(dragged_to_right_monitor, left_monitor),
            (1460, 400)
        );
        assert_eq!(
            constrain_origin_to_monitor(dragged_above_monitor, left_monitor),
            (300, 0)
        );
    }
}
