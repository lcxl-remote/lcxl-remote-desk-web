use lcxl_remote_desk_server::model::security_approval::{
    SecurityApprovalEventPayload, SecurityApprovalReceiver,
};
use tauri::{AppHandle, Emitter, Manager};

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
            while let Ok(req) = receiver.recv() {
                let payload = SecurityApprovalEventPayload {
                    req_id: req.req_id,
                    permission_type: format!("{:?}", req.permission_type),
                    from_session_id: req.from_session_id,
                    i18n_key: req.permission_type.i18n_key().to_string(),
                };

                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    // 破解现代系统防止后台抢焦点的限制 (Windows/macOS)
                    let _ = window.set_always_on_top(true);
                    let _ = window.set_focus();
                    let _ = window.set_always_on_top(false);
                    // 提示用户托盘有消息（任务栏闪烁）
                    let _ = window.request_user_attention(Some(tauri::UserAttentionType::Critical));

                    // Send payload via dispatchEvent for external url scenarios without tauri injection
                    let safe_json =
                        serde_json::to_string(&payload).unwrap_or_else(|_| "\"\"".to_string());
                    let script = format!(
                        "window.dispatchEvent(new CustomEvent('security-approval-request', {{ detail: {} }}));",
                        safe_json
                    );
                    if let Err(e) = window.eval(&script) {
                        log::error!("Failed to eval security approval request: {}", e);
                    }
                }

                // emit to frontend (for scenarios where tauri injection exists)
                if let Err(e) = app_handle.emit("security-approval-request", payload) {
                    log::error!("Failed to emit security approval request: {}", e);
                }
            }
            log::info!("Security approval receiver thread exiting");
        });
    }
}
