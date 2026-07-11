use desk_signal_facade::model::security_settings::SecuritySettings;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::host_control::{ApprovalRequest, HostControlHub};
use crate::model::settings::SharedSettings;

/// The three-state capability ordering `Some(false) < None < Some(true)` used to
/// combine a per-connection capability ceiling with the host global. `meet` picks
/// the **more restrictive** (smaller) of the two — `Some(false)` (deny) dominates,
/// then `None` (prompt), then `Some(true)` (allow).
fn tri_state_rank(v: Option<bool>) -> u8 {
    match v {
        Some(false) => 0,
        None => 1,
        Some(true) => 2,
    }
}

/// The more restrictive of two tri-state permissions under
/// `Some(false) < None < Some(true)` (i.e. the min). Feeding the result into
/// [`check_security_permission`] means a redeemed-grant session can only ever be
/// *tightened* relative to the host global, never widened: `meet(Some(false), _)`
/// hard-denies, `meet(None, _)` downgrades an allow to a prompt, and
/// `meet(Some(true), g) == g` leaves the global untouched.
pub fn meet_permission(ceiling: Option<bool>, global: Option<bool>) -> Option<bool> {
    if tri_state_rank(ceiling) <= tri_state_rank(global) {
        ceiling
    } else {
        global
    }
}

/// The effective permission for one capability dimension given the connection's
/// optional ceiling and the host global. An owner / unrestricted connection
/// (`ceiling == None`, no `SecuritySettings` at all) uses the global verbatim; a
/// redeemed-grant connection meets its per-dimension ceiling with the global via
/// [`meet_permission`]. `dim` selects the capability field from both.
pub fn effective_permission(
    ceiling: Option<&SecuritySettings>,
    global: Option<bool>,
    dim: impl Fn(&SecuritySettings) -> Option<bool>,
) -> Option<bool> {
    match ceiling {
        None => global,
        Some(c) => meet_permission(dim(c), global),
    }
}

/// Type of security permission being requested
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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
    /// The connection ID of the controller requesting access
    pub from_connection_id: Option<String>,
}

/// Used by Tauri to send security approval requests to the frontend
#[derive(Clone, Serialize, ToSchema)]
pub struct SecurityApprovalEventPayload {
    pub req_id: String,
    pub permission_type: String,
    pub from_connection_id: Option<String>,
    pub i18n_key: String,
}

/// Command sent to Tauri to manage security approval dialog
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum SecurityApprovalCommand {
    /// Show a new approval request
    Request(SecurityApprovalRequest),
    /// One previously-shown approval has finished. The Tauri side keeps a set
    /// of in-flight req_ids and only releases UI affordances (e.g. unsetting
    /// always-on-top) when the set becomes empty, so concurrent dialogs do not
    /// release the focus boost prematurely.
    Finish { req_id: String },
    /// Drop every locally-tracked pending request and release UI affordances.
    /// Sent by the Tauri-side IPC client when the ws link to the server breaks,
    /// so a server-side restart does not leave Tauri pinned with a HashSet that
    /// no Finish will ever match.
    Reset,
}

/// Legacy mpsc Tauri-bridge channel types. Retained while the daemon's tauri_ipc
/// bridge is still in place; Step 6 of the host-control-hub unification removes
/// them entirely. New code must drive approval flow through `HostControlHub`.
pub type SecurityApprovalSender = std::sync::mpsc::Sender<SecurityApprovalCommand>;
pub type SecurityApprovalReceiver = std::sync::mpsc::Receiver<SecurityApprovalCommand>;

/// Check a security permission from settings.
/// - `Some(true)` → allow
/// - `Some(false)` → deny
/// - `None` → ask the user via the Host Control Hub; deny if no UI is available.
///
/// If the user checks "remember", the corresponding `settings.security.allow_*`
/// field is updated and persisted (kept here, not in the hub, per plan review #1).
pub async fn check_security_permission(
    settings: &SharedSettings,
    hub: &Arc<HostControlHub>,
    permission: Option<bool>,
    permission_type: SecurityPermissionType,
    from_connection_id: Option<String>,
) -> bool {
    match permission {
        Some(true) => true,
        Some(false) => false,
        None => {
            let req = ApprovalRequest {
                req_id: crate::host_control::new_req_id(),
                permission_type: permission_type.clone(),
                from_connection_id,
            };
            let response = hub.request_approval(req).await;

            if response.remember {
                let mut settings_write = settings.write().await;
                match permission_type {
                    SecurityPermissionType::RemoteControl => {
                        settings_write.security.allow_remote_control = Some(response.approved);
                    }
                    SecurityPermissionType::ClipboardSync => {
                        settings_write.security.allow_clipboard_sync = Some(response.approved);
                    }
                    SecurityPermissionType::PrivateScreen => {
                        settings_write.security.allow_private_screen = Some(response.approved);
                    }
                    SecurityPermissionType::Whiteboard => {
                        settings_write.security.allow_whiteboard = Some(response.approved);
                    }
                    SecurityPermissionType::Terminal => {
                        settings_write.security.allow_terminal = Some(response.approved);
                    }
                    SecurityPermissionType::FileBrowse => {
                        settings_write.security.allow_file_browse = Some(response.approved);
                    }
                    SecurityPermissionType::FileTransfer => {
                        settings_write.security.allow_file_transfer = Some(response.approved);
                    }
                }
                // Any path that persists security settings normalizes an unset
                // approval timeout to the finite default, so a save never drops
                // it to a value that reloads as the 30s default by omission.
                settings_write.security.normalize();
                if let Err(e) = settings_write.save() {
                    log::error!("Failed to save security settings: {}", e);
                }
            }
            response.approved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_control::{ApprovalResponse, HostControlHub, HostControlMessage};
    use crate::model::settings::Settings;
    use std::time::Duration;

    /// Full 9-cell truth table of `meet_permission` under
    /// `Some(false) < None < Some(true)` — the result is always the more
    /// restrictive (smaller) of the two operands.
    #[test]
    fn meet_permission_picks_the_more_restrictive() {
        use std::option::Option::None as N;
        let f = Some(false);
        let t = Some(true);
        // ceiling \ global:      false   None    true
        assert_eq!(meet_permission(f, f), f);
        assert_eq!(meet_permission(f, N), f);
        assert_eq!(meet_permission(f, t), f);
        assert_eq!(meet_permission(N, f), f);
        assert_eq!(meet_permission(N, N), N);
        assert_eq!(meet_permission(N, t), N);
        assert_eq!(meet_permission(t, f), f);
        assert_eq!(meet_permission(t, N), N);
        assert_eq!(meet_permission(t, t), t);
    }

    /// `effective_permission`: an owner connection (no ceiling) uses the global
    /// verbatim; a grant connection meets its per-dimension ceiling with the
    /// global. A grant can only tighten, never widen.
    #[test]
    fn effective_permission_owner_uses_global_grant_meets() {
        // Owner (no ceiling) → global verbatim, even for a "prompt" global.
        assert_eq!(
            effective_permission(None, Some(true), |c| c.allow_file_transfer),
            Some(true)
        );
        assert_eq!(
            effective_permission(None, None, |c| c.allow_file_transfer),
            None
        );

        // Grant with a deny ceiling hard-denies even when the global allows.
        let deny_ft = SecuritySettings {
            allow_file_transfer: Some(false),
            ..Default::default()
        };
        assert_eq!(
            effective_permission(Some(&deny_ft), Some(true), |c| c.allow_file_transfer),
            Some(false)
        );

        // Grant with an unset (None) ceiling dimension downgrades an allow to a
        // prompt (cannot silently widen).
        let unset = SecuritySettings::default();
        assert_eq!(
            effective_permission(Some(&unset), Some(true), |c| c.allow_terminal),
            None
        );

        // Grant that allows a dimension defers to the global (min == global).
        let allow_term = SecuritySettings {
            allow_terminal: Some(true),
            ..Default::default()
        };
        assert_eq!(
            effective_permission(Some(&allow_term), Some(false), |c| c.allow_terminal),
            Some(false)
        );
    }

    fn shared_settings_for_test() -> SharedSettings {
        let mut s = Settings::default();
        // Point persistence at a unique scratch path so the in-test `save()` does not
        // collide between parallel cargo-test threads. The save call itself logs and
        // ignores errors, so even if the path is unwritable the assertions still hold.
        let dir = std::env::temp_dir().join("lcxl-rd-test-settings");
        let _ = std::fs::create_dir_all(&dir);
        s.args.config_file_path = dir
            .join(format!("settings-{}.toml", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        SharedSettings::from(s)
    }

    /// Spawn a helper that subscribes to outbound commands from the hub and
    /// resolves the first SecurityApprovalRequest it sees with `response`.
    /// Returns immediately; the helper task lives until it resolves once.
    fn spawn_responder(hub: &Arc<HostControlHub>, response: ApprovalResponse) {
        let mut rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let hub_clone = Arc::clone(hub);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(HostControlMessage::SecurityApprovalRequest { req_id, .. }) => {
                        hub_clone.submit_approval(&req_id, response);
                        return;
                    }
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
        });
    }

    // U-18: explicit allow short-circuits — no hub call.
    #[tokio::test]
    async fn u18_check_with_some_true_returns_true() {
        let settings = shared_settings_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        // No subscriber/responder — would block forever if the hub were consulted.
        let approved = tokio::time::timeout(
            Duration::from_millis(200),
            check_security_permission(
                &settings,
                &hub,
                Some(true),
                SecurityPermissionType::RemoteControl,
                None,
            ),
        )
        .await
        .expect("must short-circuit");
        assert!(approved);
    }

    // U-19: explicit deny short-circuits — no hub call.
    #[tokio::test]
    async fn u19_check_with_some_false_returns_false() {
        let settings = shared_settings_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        let approved = tokio::time::timeout(
            Duration::from_millis(200),
            check_security_permission(
                &settings,
                &hub,
                Some(false),
                SecurityPermissionType::RemoteControl,
                None,
            ),
        )
        .await
        .expect("must short-circuit");
        assert!(!approved);
    }

    // U-20: None + hub returns {approved=true, remember=true} → settings updated.
    #[tokio::test]
    async fn u20_check_with_remember_writes_settings() {
        let settings = shared_settings_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );

        let approved = check_security_permission(
            &settings,
            &hub,
            None,
            SecurityPermissionType::Terminal,
            None,
        )
        .await;
        assert!(approved);
        let s = settings.read().await;
        assert_eq!(s.security.allow_terminal, Some(true));
    }

    // U-21: None + hub returns deny without remember → settings unchanged.
    #[tokio::test]
    async fn u21_check_without_remember_does_not_write_settings() {
        let settings = shared_settings_for_test();
        let before = settings.read().await.security.allow_file_browse;
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: false,
                remember: false,
            },
        );

        let approved = check_security_permission(
            &settings,
            &hub,
            None,
            SecurityPermissionType::FileBrowse,
            None,
        )
        .await;
        assert!(!approved);
        let after = settings.read().await.security.allow_file_browse;
        assert_eq!(before, after);
    }

    // U-6: parametric test that all 7 permission types route to the correct settings field.
    #[tokio::test]
    async fn u6_remember_writes_correct_field_per_type() {
        type Getter = fn(&Settings) -> Option<bool>;
        let cases: [(SecurityPermissionType, Getter); 7] = [
            (SecurityPermissionType::RemoteControl, |s| {
                s.security.allow_remote_control
            }),
            (SecurityPermissionType::ClipboardSync, |s| {
                s.security.allow_clipboard_sync
            }),
            (SecurityPermissionType::PrivateScreen, |s| {
                s.security.allow_private_screen
            }),
            (SecurityPermissionType::Whiteboard, |s| {
                s.security.allow_whiteboard
            }),
            (SecurityPermissionType::Terminal, |s| {
                s.security.allow_terminal
            }),
            (SecurityPermissionType::FileBrowse, |s| {
                s.security.allow_file_browse
            }),
            (SecurityPermissionType::FileTransfer, |s| {
                s.security.allow_file_transfer
            }),
        ];
        for (perm, getter) in cases {
            let settings = shared_settings_for_test();
            let hub = Arc::new(HostControlHub::new_local());
            spawn_responder(
                &hub,
                ApprovalResponse {
                    approved: true,
                    remember: true,
                },
            );
            let _ = check_security_permission(&settings, &hub, None, perm.clone(), None).await;
            let s = settings.read().await;
            assert_eq!(getter(&s), Some(true), "field mismatch for {:?}", perm);
        }
    }
}
