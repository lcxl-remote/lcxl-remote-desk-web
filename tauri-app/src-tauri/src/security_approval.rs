use lcxl_remote_desk_server::model::security_approval::{
    SecurityApprovalCommand, SecurityApprovalEventPayload, SecurityApprovalReceiver,
};
use std::collections::HashSet;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

pub struct SecurityApprovalManager {
    app_handle: AppHandle,
}

/// Decision returned by [`reduce_pending`] for the Tauri-side state machine that
/// tracks in-flight approval dialogs. Pulled out of `start` so it can be unit
/// tested without spinning up a Tauri AppHandle.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PendingEffect {
    /// First request just arrived — pin the window above other windows.
    SetAlwaysOnTop,
    /// Last request just resolved — release the always-on-top pin.
    UnsetAlwaysOnTop,
    /// Request added/removed but the boolean state did not change (concurrent
    /// dialogs, or unknown finish id).
    NoChange,
}

/// Pure transition: insert/remove a req_id and report whether the boolean
/// always-on-top state should flip.
pub(crate) fn reduce_pending(
    pending: &mut HashSet<String>,
    cmd: &SecurityApprovalCommand,
) -> PendingEffect {
    match cmd {
        SecurityApprovalCommand::Request(req) => {
            let was_empty = pending.is_empty();
            let inserted = pending.insert(req.req_id.clone());
            // First entry → flip on. Re-insertion of an already-present id
            // (server replay on Tauri reconnect) is a no-op.
            if was_empty && inserted {
                PendingEffect::SetAlwaysOnTop
            } else {
                PendingEffect::NoChange
            }
        }
        SecurityApprovalCommand::Finish { req_id } => {
            let removed = pending.remove(req_id);
            if removed && pending.is_empty() {
                PendingEffect::UnsetAlwaysOnTop
            } else {
                PendingEffect::NoChange
            }
        }
        SecurityApprovalCommand::Reset => {
            if pending.is_empty() {
                PendingEffect::NoChange
            } else {
                pending.clear();
                PendingEffect::UnsetAlwaysOnTop
            }
        }
    }
}

impl SecurityApprovalManager {
    pub fn new(app_handle: AppHandle) -> Self {
        Self { app_handle }
    }

    pub fn start(&self, receiver: SecurityApprovalReceiver) {
        let app_handle = self.app_handle.clone();

        std::thread::spawn(move || {
            let mut pending: HashSet<String> = HashSet::new();
            while let Ok(cmd) = receiver.recv() {
                let effect = reduce_pending(&mut pending, &cmd);
                match cmd {
                    SecurityApprovalCommand::Request(req) => {
                        let payload = SecurityApprovalEventPayload {
                            req_id: req.req_id.clone(),
                            permission_type: format!("{:?}", req.permission_type),
                            from_connection_id: req.from_connection_id.clone(),
                            i18n_key: req.permission_type.i18n_key().to_string(),
                        };

                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            // Bypass modern system restrictions preventing background focus stealing (Windows/macOS).
                            // Pin only when the pending set transitioned from empty → non-empty;
                            // re-issuing the call on every Request would still work, but the
                            // explicit guard mirrors the Finish branch and keeps logs tidy.
                            if matches!(effect, PendingEffect::SetAlwaysOnTop) {
                                let _ = window.set_always_on_top(true);
                            }
                            let _ = window.set_focus();
                            // Notify user of tray message (taskbar flashing)
                            let _ = window
                                .request_user_attention(Some(tauri::UserAttentionType::Critical));

                            // Send payload via dispatchEvent for external url scenarios without tauri injection
                            let safe_json = serde_json::to_string(&payload)
                                .unwrap_or_else(|_| "\"\"".to_string());
                            let script = format!(
                                "window.dispatchEvent(new CustomEvent('security-approval-request', {{ detail: {} }}));",
                                safe_json
                            );
                            if let Err(e) = window.eval(&script) {
                                log::error!("Failed to eval security approval request: {}", e);
                            }
                        }

                        // ... show notification logic ...
                        use lcxl_remote_desk_server::model::security_approval::SecurityPermissionType;
                        let permission_key = match req.permission_type {
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
                        let msg =
                            rust_i18n::t!("permission_requested", permission = permission_name);

                        app_handle
                            .notification()
                            .builder()
                            .title(title)
                            .body(msg)
                            .show()
                            .unwrap_or_else(|e| log::error!("Failed to show notification: {}", e));

                        // emit to frontend (for scenarios where tauri injection exists)
                        if let Err(e) = app_handle.emit("security-approval-request", payload) {
                            log::error!("Failed to emit security approval request: {}", e);
                        }
                    }
                    SecurityApprovalCommand::Finish { req_id } => {
                        log::info!(
                            "Received SecurityApprovalCommand::Finish for req_id={} (effect={:?}, pending_left={})",
                            req_id,
                            effect,
                            pending.len()
                        );
                        if matches!(effect, PendingEffect::UnsetAlwaysOnTop)
                            && let Some(window) = app_handle.get_webview_window("main")
                        {
                            let _ = window.set_always_on_top(false);
                        }
                    }
                    SecurityApprovalCommand::Reset => {
                        log::info!(
                            "Received SecurityApprovalCommand::Reset (effect={:?})",
                            effect
                        );
                        if matches!(effect, PendingEffect::UnsetAlwaysOnTop)
                            && let Some(window) = app_handle.get_webview_window("main")
                        {
                            let _ = window.set_always_on_top(false);
                        }
                    }
                }
            }
            log::info!("Security approval receiver thread exiting");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcxl_remote_desk_server::model::security_approval::{
        SecurityApprovalRequest, SecurityPermissionType,
    };

    fn req(id: &str) -> SecurityApprovalCommand {
        SecurityApprovalCommand::Request(SecurityApprovalRequest {
            req_id: id.to_string(),
            permission_type: SecurityPermissionType::RemoteControl,
            from_connection_id: None,
        })
    }

    fn fin(id: &str) -> SecurityApprovalCommand {
        SecurityApprovalCommand::Finish {
            req_id: id.to_string(),
        }
    }

    #[test]
    fn first_request_pins_and_last_finish_releases() {
        let mut p = HashSet::new();
        assert_eq!(
            reduce_pending(&mut p, &req("a")),
            PendingEffect::SetAlwaysOnTop
        );
        assert_eq!(p.len(), 1);
        assert_eq!(
            reduce_pending(&mut p, &fin("a")),
            PendingEffect::UnsetAlwaysOnTop
        );
        assert!(p.is_empty());
    }

    // Concurrent dialogs: pin once on the first request, only release after both
    // have finished. This is the regression case for the bug where the window
    // stayed pinned because no Finish ever arrived.
    #[test]
    fn concurrent_requests_only_release_on_last_finish() {
        let mut p = HashSet::new();
        assert_eq!(
            reduce_pending(&mut p, &req("a")),
            PendingEffect::SetAlwaysOnTop
        );
        assert_eq!(reduce_pending(&mut p, &req("b")), PendingEffect::NoChange);
        assert_eq!(reduce_pending(&mut p, &fin("a")), PendingEffect::NoChange);
        assert_eq!(p.len(), 1);
        assert_eq!(
            reduce_pending(&mut p, &fin("b")),
            PendingEffect::UnsetAlwaysOnTop
        );
        assert!(p.is_empty());
    }

    // Server may replay a still-pending request when Tauri reconnects mid-flight.
    // Re-inserting the same id must not toggle the pin off-and-on.
    #[test]
    fn duplicate_request_is_idempotent() {
        let mut p = HashSet::new();
        assert_eq!(
            reduce_pending(&mut p, &req("a")),
            PendingEffect::SetAlwaysOnTop
        );
        assert_eq!(reduce_pending(&mut p, &req("a")), PendingEffect::NoChange);
        assert_eq!(p.len(), 1);
    }

    // Stray Finish for an unknown id (e.g. delivered after a hub reset) must not
    // unpin while real requests are still in flight.
    #[test]
    fn unknown_finish_does_not_release() {
        let mut p = HashSet::new();
        reduce_pending(&mut p, &req("a"));
        assert_eq!(
            reduce_pending(&mut p, &fin("ghost")),
            PendingEffect::NoChange
        );
        assert_eq!(p.len(), 1);
    }

    // A Finish that arrives before any matching Request (unlikely but possible
    // under a reconnect race) must be a no-op rather than flipping state.
    #[test]
    fn finish_on_empty_set_is_noop() {
        let mut p = HashSet::new();
        assert_eq!(reduce_pending(&mut p, &fin("a")), PendingEffect::NoChange);
        assert!(p.is_empty());
    }

    // Server-side restart edge case: ws drops while requests are still in flight.
    // The IPC client emits Reset to force-clear pending and release the pin so
    // the Tauri shell does not stay always-on-top forever.
    #[test]
    fn reset_clears_pending_and_releases_pin() {
        let mut p = HashSet::new();
        reduce_pending(&mut p, &req("a"));
        reduce_pending(&mut p, &req("b"));
        assert_eq!(p.len(), 2);
        assert_eq!(
            reduce_pending(&mut p, &SecurityApprovalCommand::Reset),
            PendingEffect::UnsetAlwaysOnTop
        );
        assert!(p.is_empty());
    }

    // Reset on an already-empty set must not pretend a transition happened —
    // otherwise the manager would issue a redundant set_always_on_top(false)
    // every reconnect.
    #[test]
    fn reset_on_empty_set_is_noop() {
        let mut p = HashSet::new();
        assert_eq!(
            reduce_pending(&mut p, &SecurityApprovalCommand::Reset),
            PendingEffect::NoChange
        );
    }
}
