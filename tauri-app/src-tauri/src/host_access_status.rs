use lcxl_remote_desk_server::host_control::HostAccessSnapshot;
use std::collections::BTreeSet;
use tauri::{
    AppHandle, LogicalSize, Manager, Monitor, PhysicalPosition, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;
use url::Url;

const STATUS_WINDOW_WIDTH: f64 = 460.0;
const STATUS_WINDOW_COLLAPSED_HEIGHT: f64 = 154.0;
const STATUS_WINDOW_EXPANDED_HEIGHT: f64 = 420.0;
const STATUS_WINDOW_MARGIN: i32 = 16;
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
    pub fn new(app_handle: AppHandle, frontend_url: String) -> Self {
        Self {
            app_handle,
            frontend_url,
        }
    }

    pub fn start(&self, receiver: std::sync::mpsc::Receiver<HostAccessStatusCommand>) {
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
                            state.active =
                                snapshot.indicator_enabled && snapshot.total_session_count > 0;
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
                            if !was_active {
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

fn accept_snapshot(state: &StatusState, snapshot: &HostAccessSnapshot) -> bool {
    match state.epoch.as_deref() {
        Some(epoch) if epoch == snapshot.epoch => snapshot.revision >= state.revision,
        _ => true,
    }
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
    let url = Url::parse(frontend_url)
        .and_then(|base| base.join("host-access-status"))
        .map_err(|error| error.to_string())?;
    let initial = snapshot.clone();
    let window = WebviewWindowBuilder::new(app_handle, label, WebviewUrl::External(url))
        .title("Remote Access Status")
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
                _ => return,
            };
            let _ = window.set_size(LogicalSize::new(STATUS_WINDOW_WIDTH, height));
        })
        .on_page_load(move |window, event| {
            if let tauri::webview::PageLoadEvent::Finished = event.event() {
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
    if let Some(tray) = app_handle.tray_by_id(crate::MAIN_TRAY_ID) {
        let tooltip = if active {
            format!("LCXL Remote Desktop — {session_count} remote session(s) active")
        } else {
            "LCXL Remote Desktop".to_string()
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
        }
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
