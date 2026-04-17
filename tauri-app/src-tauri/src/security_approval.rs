use lcxl_remote_desk_server::model::security_approval::{
    SecurityApprovalCommand, SecurityApprovalEventPayload, SecurityApprovalReceiver,
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

pub struct SecurityApprovalManager {
    app_handle: AppHandle,
}

impl SecurityApprovalManager {
    pub fn new(app_handle: AppHandle) -> Self {
        Self { app_handle }
    }

    pub fn start(&self, receiver: SecurityApprovalReceiver) {
        let app_handle = self.app_handle.clone();

        std::thread::spawn(move || {
            while let Ok(cmd) = receiver.recv() {
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
                            // Bypass modern system restrictions preventing background focus stealing (Windows/macOS)
                            let _ = window.set_always_on_top(true);
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
                    SecurityApprovalCommand::Finish => {
                        log::info!(
                            "Received SecurityApprovalCommand::Finish, unsetting always_on_top"
                        );
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.set_always_on_top(false);
                        }
                    }
                }
            }
            log::info!("Security approval receiver thread exiting");
        });
    }
}
