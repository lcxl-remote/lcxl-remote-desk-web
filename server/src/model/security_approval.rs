use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use utoipa::ToSchema;

use crate::model::settings::SharedSettings;

/// Type of security permission being requested
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub enum SecurityPermissionType {
    RemoteControl,
    ClipboardSync,
    PrivateScreen,
    Whiteboard,
    Terminal,
    FileBrowse,
    FileTransfer,
}

impl SecurityPermissionType {
    /// Returns the i18n key for this permission type
    pub fn i18n_key(&self) -> &'static str {
        match self {
            Self::RemoteControl => "security.permission.remoteControl",
            Self::ClipboardSync => "security.permission.clipboardSync",
            Self::PrivateScreen => "security.permission.privateScreen",
            Self::Whiteboard => "security.permission.whiteboard",
            Self::Terminal => "security.permission.terminal",
            Self::FileBrowse => "security.permission.fileBrowse",
            Self::FileTransfer => "security.permission.fileTransfer",
        }
    }
}

/// The user's response to a security approval request
#[derive(Debug, Clone)]
pub struct SecurityApprovalResponse {
    /// Whether the user approved the request
    pub approved: bool,
    /// Whether to remember this choice (persist to SecuritySettings)
    pub remember: bool,
}

/// A security approval request sent from the server to Tauri
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SecurityApprovalRequest {
    /// Unique identifier for this request
    pub req_id: String,
    /// The type of permission being requested
    pub permission_type: SecurityPermissionType,
    /// The session ID of the controller requesting access
    pub from_session_id: Option<String>,
}

/// Used by Tauri to send security approval requests to the frontend
#[derive(Clone, Serialize, ToSchema)]
pub struct SecurityApprovalEventPayload {
    pub req_id: String,
    pub permission_type: String,
    pub from_session_id: Option<String>,
    pub i18n_key: String,
}

/// Command sent to Tauri to manage security approval dialog
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum SecurityApprovalCommand {
    /// Show a new approval request
    Request(SecurityApprovalRequest),
    /// Finish all current approvals (unset always_on_top)
    Finish,
}

pub static PENDING_APPROVALS: Lazy<
    Mutex<HashMap<String, tokio::sync::oneshot::Sender<SecurityApprovalResponse>>>,
> = Lazy::new(|| Mutex::new(HashMap::new()));

/// The channel sender type used to send approval commands to Tauri
pub type SecurityApprovalSender = std::sync::mpsc::Sender<SecurityApprovalCommand>;
pub type SecurityApprovalReceiver = std::sync::mpsc::Receiver<SecurityApprovalCommand>;

/// Check a security permission from settings.
/// - `Some(true)` → allow
/// - `Some(false)` → deny
/// - `None` → if Tauri present, prompt user via dialog; else deny
///
/// If `remember` is checked by the user, updates SecuritySettings in config.
pub async fn check_security_permission(
    settings: &SharedSettings,
    security_approval_sender: Option<&SecurityApprovalSender>,
    permission: Option<bool>,
    permission_type: SecurityPermissionType,
    from_session_id: Option<String>,
) -> bool {
    match permission {
        Some(true) => true,
        Some(false) => false,
        None => {
            if let Some(sender) = security_approval_sender {
                let (response_tx, response_rx) = tokio::sync::oneshot::channel();
                let req_id = uuid::Uuid::new_v4().to_string();
                let request = SecurityApprovalRequest {
                    req_id: req_id.clone(),
                    permission_type: permission_type.clone(),
                    from_session_id,
                };
                PENDING_APPROVALS
                    .lock()
                    .unwrap()
                    .insert(req_id.clone(), response_tx);

                if sender
                    .send(SecurityApprovalCommand::Request(request))
                    .is_ok()
                {
                    // Wait for user response
                    match response_rx.await {
                        Ok(response) => {
                            if response.remember {
                                let mut settings_write = settings.write().await;
                                match permission_type {
                                    SecurityPermissionType::RemoteControl => {
                                        settings_write.security.allow_remote_control =
                                            Some(response.approved);
                                    }
                                    SecurityPermissionType::ClipboardSync => {
                                        settings_write.security.allow_clipboard_sync =
                                            Some(response.approved);
                                    }
                                    SecurityPermissionType::PrivateScreen => {
                                        settings_write.security.allow_private_screen =
                                            Some(response.approved);
                                    }
                                    SecurityPermissionType::Whiteboard => {
                                        settings_write.security.allow_whiteboard =
                                            Some(response.approved);
                                    }
                                    SecurityPermissionType::Terminal => {
                                        settings_write.security.allow_terminal =
                                            Some(response.approved);
                                    }
                                    SecurityPermissionType::FileBrowse => {
                                        settings_write.security.allow_file_browse =
                                            Some(response.approved);
                                    }
                                    SecurityPermissionType::FileTransfer => {
                                        settings_write.security.allow_file_transfer =
                                            Some(response.approved);
                                    }
                                }
                                if let Err(e) = settings_write.save() {
                                    log::error!("Failed to save security settings: {}", e);
                                }
                            }
                            return response.approved;
                        }
                        Err(_) => {
                            log::warn!("Security approval response channel dropped, denying");
                            let mut approvals = PENDING_APPROVALS.lock().unwrap();
                            approvals.remove(&req_id);
                            if approvals.is_empty() {
                                let _ = sender.send(SecurityApprovalCommand::Finish);
                            }
                            return false;
                        }
                    }
                }
            }
            log::info!(
                "No GUI available, defaulting to deny for {:?}",
                permission_type
            );
            false
        }
    }
}
