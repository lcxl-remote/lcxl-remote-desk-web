use desk_signal_facade::model::security_settings::SecuritySettings;
use serde::Serialize;
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

/// The capability dimensions live with the settings they select, so that the
/// list, the fields and the policy distribution built on them cannot disagree.
pub use desk_signal_facade::model::security_settings::SecurityPermissionType;

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

/// What the host decided about one permission request.
///
/// Deciding and persisting are separate so that the two roles that ask this
/// question can commit a remembered answer their own way: the daemon owns the
/// settings directly, while a session worker holds only a copy of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityDecision {
    /// Whether this one request goes ahead.
    pub approved: bool,
    /// Whether the answer should become the host's standing policy for this
    /// capability. Already accounts for a capped session, which can never widen
    /// the owner's global — a caller that sees `true` may commit it as-is.
    pub remember: bool,
}

/// Decide a security permission from settings, without persisting anything.
/// - `Some(true)` → allow
/// - `Some(false)` → deny
/// - `None` → ask the user via the Host Control Hub; deny if no UI is available.
///
/// A capped grant / code-session connection passes `suppress_remember = true`:
/// its prompt fires because the meet of the owner's per-code ceiling and the
/// global landed on `None`, and letting the local user's "remember" widen the
/// owner's **global** `allow_*` would leak a per-session decision into the
/// owner's own future sessions and every other code (capabilities are the
/// owner's to configure, not to grant ad-hoc through a borrowed session's
/// prompt). The approval is still honored for this one request; only the
/// standing policy is left alone, which shows up as `remember: false`.
pub async fn decide_security_permission(
    settings: &SharedSettings,
    hub: &Arc<HostControlHub>,
    permission: Option<bool>,
    permission_type: SecurityPermissionType,
    from_connection_id: Option<String>,
    suppress_remember: bool,
) -> SecurityDecision {
    match permission {
        Some(true) => SecurityDecision {
            approved: true,
            remember: false,
        },
        Some(false) => SecurityDecision {
            approved: false,
            remember: false,
        },
        None => {
            let req = ApprovalRequest {
                req_id: crate::host_control::new_req_id(),
                permission_type,
                from_connection_id,
            };
            // Bound the wait by the host's configured approval timeout (0 = never,
            // None = the finite default; never fail open to an unbounded wait).
            let approval_timeout = crate::host_control::server_approval_timeout(
                settings.read().await.security.approval_timeout,
            );
            let response = hub.request_approval(req, approval_timeout).await;

            if response.remember && suppress_remember {
                log::info!(
                    "[security] not persisting a capped session's approval to the host global \
                     (per-code ceiling stays authoritative)"
                );
            }
            SecurityDecision {
                approved: response.approved,
                remember: response.remember && !suppress_remember,
            }
        }
    }
}

/// Make a remembered answer the host's standing policy for one capability.
///
/// Failures are logged rather than returned: the request the user just answered
/// is honored either way, and there is no caller that could do anything useful
/// with a persistence error at this point.
pub async fn persist_remembered_decision(
    settings: &SharedSettings,
    permission_type: &SecurityPermissionType,
    approved: bool,
) {
    let mut settings_write = settings.write().await;
    permission_type.write(&mut settings_write.security, Some(approved));
    // Any path that persists security settings normalizes an unset approval
    // timeout to the finite default, so a save never drops it to a value that
    // reloads as the 30s default by omission.
    settings_write.security.normalize();
    if let Err(e) = settings_write.save() {
        log::error!("Failed to save security settings: {}", e);
    }
}

/// Decide a permission and commit a remembered answer in one step.
///
/// The shape every gate currently uses. It writes the standing policy through
/// whichever `Settings` handle the caller holds, which is correct in the daemon
/// and merely tolerated in a worker, where that handle is a copy.
pub async fn check_security_permission(
    settings: &SharedSettings,
    hub: &Arc<HostControlHub>,
    permission: Option<bool>,
    permission_type: SecurityPermissionType,
    from_connection_id: Option<String>,
    suppress_remember: bool,
) -> bool {
    let decision = decide_security_permission(
        settings,
        hub,
        permission,
        permission_type,
        from_connection_id,
        suppress_remember,
    )
    .await;
    if decision.remember {
        persist_remembered_decision(settings, &permission_type, decision.approved).await;
    }
    decision.approved
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
                false,
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
                false,
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
            false,
        )
        .await;
        assert!(approved);
        let s = settings.read().await;
        assert_eq!(s.security.allow_terminal, Some(true));
    }

    /// A capped grant / code-session (`suppress_remember = true`) that the local
    /// user approves *with* remember is honored for this request but must NOT widen
    /// the host global — the owner's per-code ceiling stays the only authority.
    #[tokio::test]
    async fn capped_session_remember_does_not_write_global() {
        let settings = shared_settings_for_test();
        let before = settings.read().await.security.allow_terminal;
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
            true,
        )
        .await;
        // Honored for this one request...
        assert!(approved);
        // ...but the global is untouched (no leak into the owner's future sessions).
        assert_eq!(settings.read().await.security.allow_terminal, before);
    }

    /// Deciding is the half that a session worker can run: it holds a copy of
    /// the settings, so a write there would push a stale snapshot onto disk.
    /// Asking must therefore leave the stored policy exactly as it was.
    #[tokio::test]
    async fn deciding_a_permission_does_not_touch_the_stored_policy() {
        let settings = shared_settings_for_test();
        let before = settings.read().await.security.allow_terminal;
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );

        let decision = decide_security_permission(
            &settings,
            &hub,
            None,
            SecurityPermissionType::Terminal,
            None,
            false,
        )
        .await;

        assert!(decision.approved);
        assert!(decision.remember, "the user asked to remember this");
        assert_eq!(
            settings.read().await.security.allow_terminal,
            before,
            "deciding must not write the policy"
        );
    }

    /// Committing is the other half, and it is what actually moves the policy.
    #[tokio::test]
    async fn persisting_a_remembered_answer_writes_the_policy() {
        let settings = shared_settings_for_test();

        persist_remembered_decision(&settings, &SecurityPermissionType::FileDelete, false).await;

        assert_eq!(
            settings.read().await.security.allow_file_delete,
            Some(false)
        );
    }

    /// A capped session's "remember" must never reach a caller as something to
    /// commit — the suppression has to be decided here, not left to each gate.
    #[tokio::test]
    async fn a_capped_session_never_reports_a_remembered_answer() {
        let settings = shared_settings_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );

        let decision = decide_security_permission(
            &settings,
            &hub,
            None,
            SecurityPermissionType::Terminal,
            None,
            true,
        )
        .await;

        assert!(decision.approved, "the request itself is still honored");
        assert!(!decision.remember);
    }

    /// An already-configured capability answers without a prompt, so there is
    /// nothing to remember and no policy write to make.
    #[tokio::test]
    async fn a_configured_capability_reports_nothing_to_remember() {
        let settings = shared_settings_for_test();
        let hub = Arc::new(HostControlHub::new_local());

        for (configured, expected) in [(Some(true), true), (Some(false), false)] {
            let decision = decide_security_permission(
                &settings,
                &hub,
                configured,
                SecurityPermissionType::RemoteControl,
                None,
                false,
            )
            .await;
            assert_eq!(decision.approved, expected);
            assert!(!decision.remember);
        }
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
            false,
        )
        .await;
        assert!(!approved);
        let after = settings.read().await.security.allow_file_browse;
        assert_eq!(before, after);
    }

    // U-6: parametric test that every permission type routes to the correct settings field.
    #[tokio::test]
    async fn u6_remember_writes_correct_field_per_type() {
        type Getter = fn(&Settings) -> Option<bool>;
        let cases: [(SecurityPermissionType, Getter); 8] = [
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
            (SecurityPermissionType::FileDelete, |s| {
                s.security.allow_file_delete
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
            let _ = check_security_permission(&settings, &hub, None, perm, None, false).await;
            let s = settings.read().await;
            assert_eq!(getter(&s), Some(true), "field mismatch for {:?}", perm);
        }
    }
}
