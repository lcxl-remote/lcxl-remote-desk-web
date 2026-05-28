use lcxl_remote_desk_server::model::security_approval::{
    SecurityApprovalCommand, SecurityApprovalReceiver, SecurityApprovalRequest,
    SecurityPermissionType,
};
use std::collections::HashMap;
use tauri::{
    AppHandle, Manager, Monitor, PhysicalPosition, UserAttentionType, WebviewUrl,
    WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;
use url::Url;

const APPROVAL_WINDOW_INNER_W: f64 = 520.0;
const APPROVAL_WINDOW_INNER_H: f64 = 320.0;

pub struct SecurityApprovalManager {
    app_handle: AppHandle,
    /// Base URL the approval page is served from (daemon URL in service-shell
    /// mode, the embedded frontend URL in default mode). The page is loaded via
    /// an external URL so it shares the session cookie with the main window.
    frontend_url: String,
}

/// Side effects the windowing state machine should apply for a command. Pure:
/// [`compute_effect`] never mutates state nor creates windows, so it can be unit
/// tested without an `AppHandle`. The manager loop owns all mutation and I/O.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WindowingEffect {
    /// `create`: per-monitor windows that must be built (label + monitor index).
    /// `refocus`: windows already present that should be raised again (replay /
    /// reconnect, or a partial previous build).
    Open {
        create: Vec<(String, usize)>,
        refocus: Vec<String>,
    },
    /// Windows to destroy (Finish for one req, Reset for all).
    Close {
        labels: Vec<String>,
    },
    NoChange,
}

/// Replace any character outside `[0-9a-zA-Z-]` with `_` so the value is safe as
/// a Tauri window label. req_ids are UUIDs (hex + dashes) so this is the
/// identity for real input; the sanitizer only guards against malformed ids.
pub(crate) fn sanitize_label_token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Deterministic window label for a given request and monitor index.
pub(crate) fn build_window_label(req_id: &str, mon_idx: usize) -> String {
    format!(
        "security-approval-{}-mon-{}",
        sanitize_label_token(req_id),
        mon_idx
    )
}

/// Pure windowing decision. `state` maps req_id → labels successfully built so
/// far. `monitor_count` is the number of physical monitors (always treated as
/// at least one so there is always a visible dialog).
pub(crate) fn compute_effect(
    state: &HashMap<String, Vec<String>>,
    cmd: &SecurityApprovalCommand,
    monitor_count: usize,
) -> WindowingEffect {
    match cmd {
        SecurityApprovalCommand::Request(req) => {
            let count = monitor_count.max(1);
            let existing = state.get(&req.req_id);
            let mut create = Vec::new();
            let mut refocus = Vec::new();
            for idx in 0..count {
                let label = build_window_label(&req.req_id, idx);
                let already = existing.map(|v| v.contains(&label)).unwrap_or(false);
                if already {
                    refocus.push(label);
                } else {
                    create.push((label, idx));
                }
            }
            WindowingEffect::Open { create, refocus }
        }
        SecurityApprovalCommand::Finish { req_id } => match state.get(req_id) {
            Some(labels) if !labels.is_empty() => WindowingEffect::Close {
                labels: labels.clone(),
            },
            _ => WindowingEffect::NoChange,
        },
        SecurityApprovalCommand::Reset => {
            if state.is_empty() {
                WindowingEffect::NoChange
            } else {
                let labels = state.values().flatten().cloned().collect();
                WindowingEffect::Close { labels }
            }
        }
    }
}

/// Build the external URL for the approval page, carrying the request context as
/// query parameters (percent-encoded by `url`). The page reads these via
/// `useSearchParams` — no Tauri IPC injection is required.
fn build_approval_url(frontend_url: &str, req: &SecurityApprovalRequest) -> Result<Url, String> {
    let mut url = Url::parse(frontend_url)
        .and_then(|u| u.join("security-approval"))
        .map_err(|e| e.to_string())?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("req_id", &req.req_id);
        q.append_pair("permission_type", &format!("{:?}", req.permission_type));
        if let Some(ref from) = req.from_connection_id {
            q.append_pair("from_connection_id", from);
        }
        q.append_pair("i18n_key", req.permission_type.i18n_key());
    }
    Ok(url)
}

/// Create a single per-monitor approval window centered on `monitor` (or
/// Tauri-centered when the monitor is unknown).
fn build_approval_window(
    app_handle: &AppHandle,
    frontend_url: &str,
    label: &str,
    req: &SecurityApprovalRequest,
    monitor: Option<&Monitor>,
) -> Result<(), String> {
    let url = build_approval_url(frontend_url, req)?;
    let mut builder = WebviewWindowBuilder::new(app_handle, label, WebviewUrl::External(url))
        .title("Security Approval")
        .inner_size(APPROVAL_WINDOW_INNER_W, APPROVAL_WINDOW_INNER_H)
        .decorations(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .minimizable(false)
        .focused(true);
    // Without a known monitor, let Tauri center the window on the active display.
    if monitor.is_none() {
        builder = builder.center();
    }
    let window = builder.build().map_err(|e| e.to_string())?;

    if let Some(m) = monitor {
        let scale = m.scale_factor();
        let pos = m.position();
        let size = m.size();
        let win_w = (APPROVAL_WINDOW_INNER_W * scale) as i32;
        let win_h = (APPROVAL_WINDOW_INNER_H * scale) as i32;
        let x = pos.x + (size.width as i32 - win_w) / 2;
        let y = pos.y + (size.height as i32 - win_h) / 2;
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }

    // Bypass modern background focus-stealing restrictions (Windows/macOS).
    let _ = window.set_always_on_top(true);
    let _ = window.set_focus();
    let _ = window.request_user_attention(Some(UserAttentionType::Critical));
    Ok(())
}

/// Best-effort raise of an already-present window (replay / reconnect).
fn refocus_window(app_handle: &AppHandle, label: &str) {
    if let Some(window) = app_handle.get_webview_window(label) {
        let _ = window.set_always_on_top(true);
        let _ = window.set_focus();
        let _ = window.request_user_attention(Some(UserAttentionType::Critical));
    }
}

/// Resolve the physical monitors to spread approval windows across. Falls back to
/// the primary monitor, then to an empty list (caller still builds one centered
/// window). Never panics — failures degrade to a single visible dialog.
fn resolve_monitors(app_handle: &AppHandle) -> Vec<Monitor> {
    match app_handle.available_monitors() {
        Ok(monitors) if !monitors.is_empty() => monitors,
        Ok(_) | Err(_) => match app_handle.primary_monitor() {
            Ok(Some(m)) => vec![m],
            _ => Vec::new(),
        },
    }
}

/// Notify the local user via a tray notification (taskbar flash) that a remote
/// peer is requesting a permission.
fn show_tray_notification(app_handle: &AppHandle, permission_type: &SecurityPermissionType) {
    let permission_key = match permission_type {
        SecurityPermissionType::RemoteControl => "permission.remote_control",
        SecurityPermissionType::ClipboardSync => "permission.clipboard_sync",
        SecurityPermissionType::PrivateScreen => "permission.private_screen",
        SecurityPermissionType::Whiteboard => "permission.whiteboard",
        SecurityPermissionType::Terminal => "permission.terminal",
        SecurityPermissionType::FileBrowse => "permission.file_browse",
        SecurityPermissionType::FileTransfer => "permission.file_transfer",
    };
    let permission_name = rust_i18n::t!(permission_key);
    let title = rust_i18n::t!("security_approval_title");
    let msg = rust_i18n::t!("permission_requested", permission = permission_name);

    app_handle
        .notification()
        .builder()
        .title(title)
        .body(msg)
        .show()
        .unwrap_or_else(|e| log::error!("Failed to show notification: {}", e));
}

impl SecurityApprovalManager {
    pub fn new(app_handle: AppHandle, frontend_url: String) -> Self {
        Self {
            app_handle,
            frontend_url,
        }
    }

    pub fn start(&self, receiver: SecurityApprovalReceiver) {
        let app_handle = self.app_handle.clone();
        let frontend_url = self.frontend_url.clone();

        std::thread::spawn(move || {
            // req_id → labels of windows successfully built for that request.
            let mut state: HashMap<String, Vec<String>> = HashMap::new();
            while let Ok(cmd) = receiver.recv() {
                match &cmd {
                    SecurityApprovalCommand::Request(req) => {
                        let monitors = resolve_monitors(&app_handle);
                        let monitor_count = monitors.len().max(1);
                        let effect = compute_effect(&state, &cmd, monitor_count);
                        if let WindowingEffect::Open { create, refocus } = effect {
                            for (label, idx) in create {
                                match build_approval_window(
                                    &app_handle,
                                    &frontend_url,
                                    &label,
                                    req,
                                    monitors.get(idx),
                                ) {
                                    Ok(()) => {
                                        state.entry(req.req_id.clone()).or_default().push(label);
                                    }
                                    Err(e) => {
                                        // Leave state untouched so the next replay
                                        // reconciles the missing monitor.
                                        log::error!(
                                            "Failed to build approval window {}: {}",
                                            label,
                                            e
                                        );
                                    }
                                }
                            }
                            for label in refocus {
                                refocus_window(&app_handle, &label);
                            }
                        }
                        show_tray_notification(&app_handle, &req.permission_type);
                    }
                    SecurityApprovalCommand::Finish { req_id } => {
                        log::info!("SecurityApprovalCommand::Finish req_id={}", req_id);
                        if let WindowingEffect::Close { labels } = compute_effect(&state, &cmd, 0) {
                            for label in labels {
                                destroy_window(&app_handle, &label);
                            }
                        }
                        state.remove(req_id);
                    }
                    SecurityApprovalCommand::Reset => {
                        log::info!("SecurityApprovalCommand::Reset");
                        if let WindowingEffect::Close { labels } = compute_effect(&state, &cmd, 0) {
                            for label in labels {
                                destroy_window(&app_handle, &label);
                            }
                        }
                        state.clear();
                    }
                }
            }
            log::info!("Security approval receiver thread exiting");
        });
    }
}

/// Programmatically close a window. `destroy()` forces close WITHOUT emitting
/// CloseRequested, so the page's close handler never runs — this is the
/// "program-initiated close = NOT a Deny" path (Finish / Reset). The frontend
/// only submits a Deny when the user clicks the window's X (CloseRequested).
fn destroy_window(app_handle: &AppHandle, label: &str) {
    if let Some(window) = app_handle.get_webview_window(label) {
        if let Err(e) = window.destroy() {
            log::error!("Failed to destroy approval window {}: {}", label, e);
        }
    }
}

// TODO(virtual-display): a per-monitor approval window is also created on the
// virtual display (HW id `LcxlVirtualDisplay`), where the remote peer could
// click it. Filter the virtual display out (by device name) once exclusive mode
// settles, so only physically-present screens get an approval dialog.

#[cfg(test)]
mod tests {
    use super::*;

    fn request_cmd(id: &str) -> SecurityApprovalCommand {
        SecurityApprovalCommand::Request(SecurityApprovalRequest {
            req_id: id.to_string(),
            permission_type: SecurityPermissionType::RemoteControl,
            from_connection_id: None,
        })
    }

    fn finish_cmd(id: &str) -> SecurityApprovalCommand {
        SecurityApprovalCommand::Finish {
            req_id: id.to_string(),
        }
    }

    fn labels(id: &str, count: usize) -> Vec<String> {
        (0..count).map(|i| build_window_label(id, i)).collect()
    }

    #[test]
    fn sanitize_keeps_uuid_identity() {
        let uuid = "3f8a1c2d-4b5e-6789-abcd-ef0123456789";
        assert_eq!(sanitize_label_token(uuid), uuid);
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_label_token("a&b#c d%"), "a_b_c_d_");
        // Only [0-9a-zA-Z-] survive.
        let out = sanitize_label_token("x/y\\z:1");
        assert!(
            out.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(out, "x_y_z_1");
    }

    #[test]
    fn build_label_is_stable() {
        assert_eq!(build_window_label("r1", 0), "security-approval-r1-mon-0");
        assert_eq!(build_window_label("r1", 2), "security-approval-r1-mon-2");
    }

    #[test]
    fn first_request_creates_all_monitors() {
        let state = HashMap::new();
        let effect = compute_effect(&state, &request_cmd("a"), 3);
        match effect {
            WindowingEffect::Open { create, refocus } => {
                assert_eq!(
                    create,
                    vec![
                        (build_window_label("a", 0), 0),
                        (build_window_label("a", 1), 1),
                        (build_window_label("a", 2), 2),
                    ]
                );
                assert!(refocus.is_empty());
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn repeat_same_count_all_refocus() {
        let mut state = HashMap::new();
        state.insert("a".to_string(), labels("a", 3));
        let effect = compute_effect(&state, &request_cmd("a"), 3);
        match effect {
            WindowingEffect::Open { create, refocus } => {
                assert!(create.is_empty());
                assert_eq!(refocus, labels("a", 3));
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn grow_two_to_three_creates_missing_only() {
        let mut state = HashMap::new();
        state.insert("a".to_string(), labels("a", 2));
        let effect = compute_effect(&state, &request_cmd("a"), 3);
        match effect {
            WindowingEffect::Open { create, refocus } => {
                assert_eq!(create, vec![(build_window_label("a", 2), 2)]);
                assert_eq!(refocus, labels("a", 2));
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn shrink_three_to_one_does_not_close() {
        let mut state = HashMap::new();
        state.insert("a".to_string(), labels("a", 3));
        let effect = compute_effect(&state, &request_cmd("a"), 1);
        match effect {
            WindowingEffect::Open { create, refocus } => {
                assert!(create.is_empty());
                assert_eq!(refocus, vec![build_window_label("a", 0)]);
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn finish_closes_only_that_request() {
        let mut state = HashMap::new();
        state.insert("a".to_string(), labels("a", 1));
        state.insert("b".to_string(), labels("b", 1));
        let effect = compute_effect(&state, &finish_cmd("a"), 0);
        assert_eq!(
            effect,
            WindowingEffect::Close {
                labels: labels("a", 1)
            }
        );
    }

    #[test]
    fn unknown_finish_is_no_change() {
        let mut state = HashMap::new();
        state.insert("a".to_string(), labels("a", 1));
        assert_eq!(
            compute_effect(&state, &finish_cmd("ghost"), 0),
            WindowingEffect::NoChange
        );
    }

    #[test]
    fn finish_on_empty_state_is_no_change() {
        let state = HashMap::new();
        assert_eq!(
            compute_effect(&state, &finish_cmd("a"), 0),
            WindowingEffect::NoChange
        );
    }

    #[test]
    fn reset_closes_all_windows() {
        let mut state = HashMap::new();
        state.insert("a".to_string(), labels("a", 2));
        state.insert("b".to_string(), labels("b", 1));
        match compute_effect(&state, &SecurityApprovalCommand::Reset, 0) {
            WindowingEffect::Close { mut labels } => {
                labels.sort();
                let mut expected = [labels_vec("a", 2), labels_vec("b", 1)].concat();
                expected.sort();
                assert_eq!(labels, expected);
            }
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn reset_on_empty_state_is_no_change() {
        let state = HashMap::new();
        assert_eq!(
            compute_effect(&state, &SecurityApprovalCommand::Reset, 0),
            WindowingEffect::NoChange
        );
    }

    #[test]
    fn approval_url_carries_encoded_params() {
        let req = SecurityApprovalRequest {
            req_id: "a&b#c d".to_string(),
            permission_type: SecurityPermissionType::RemoteControl,
            from_connection_id: Some("u%".to_string()),
        };
        let url = build_approval_url("http://127.0.0.1:8082", &req).unwrap();
        assert_eq!(url.path(), "/security-approval");
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("req_id").map(String::as_str), Some("a&b#c d"));
        assert_eq!(
            pairs.get("from_connection_id").map(String::as_str),
            Some("u%")
        );
        assert!(pairs.contains_key("permission_type"));
        assert!(pairs.contains_key("i18n_key"));
    }

    fn labels_vec(id: &str, count: usize) -> Vec<String> {
        labels(id, count)
    }
}
