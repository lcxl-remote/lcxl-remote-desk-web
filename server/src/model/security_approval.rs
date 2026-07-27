use desk_signal_facade::model::security_settings::SecuritySettings;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::host_control::{ApprovalRequest, HostControlHub};
use crate::model::policy_access::PolicyAccess;

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

/// A settled permission request, and whether the answer outlives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPermission {
    /// Whether this one request goes ahead.
    pub approved: bool,
    /// The capability stamp to cache this answer under, when it is safe to
    /// cache at all.
    ///
    /// `None` means the operator changed this capability while the user was
    /// answering. The user's answer still settles the request in front of them
    /// — they did just consent to it — but it must not become the standing
    /// answer for the next one, which belongs to the newer policy. Re-asking
    /// instead is worse than it sounds: the newer policy may well be "prompt"
    /// again, and a run of unrelated changes would then produce a run of
    /// dialogs, with a host configured to wait forever never getting out of it.
    pub cacheable_at: Option<u64>,
}

/// Decide a security permission from the policy, without committing anything.
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
    policy: &PolicyAccess,
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
            let approval_timeout =
                crate::host_control::server_approval_timeout(policy.approval_timeout());
            let response = hub.request_approval_shared(req, approval_timeout).await;

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

/// Settle one permission request against the host policy.
///
/// The single shape every gate uses: decide, commit a remembered answer
/// wherever this role commits it, and report whether the answer may be cached.
/// A gate that caches under the returned stamp gets invalidation for free — the
/// next read compares stamps and treats a change as a miss.
///
/// `permission` and `decided_at` must come from one [`PolicyAccess::capability`]
/// read, `permission` narrowed by the connection's ceiling if it has one. The
/// stamp is what the remembered answer is offered to the host under, so a stamp
/// belonging to a different reading of the policy would let an answer taken
/// before a change be committed as though it were taken after.
pub async fn resolve_permission(
    policy: &PolicyAccess,
    hub: &Arc<HostControlHub>,
    permission: Option<bool>,
    decided_at: u64,
    permission_type: SecurityPermissionType,
    from_connection_id: Option<String>,
    suppress_remember: bool,
) -> ResolvedPermission {
    let generation = decided_at;
    let decision = decide_security_permission(
        policy,
        hub,
        permission,
        permission_type,
        from_connection_id,
        suppress_remember,
    )
    .await;
    if decision.remember {
        policy
            .remember(permission_type, decision.approved, generation)
            .await;
    }
    let moved = policy.capability(permission_type).generation != generation;
    if moved {
        log::info!(
            "[security] {permission_type:?} changed while the user was answering; honoring the \
             answer for this request only"
        );
    }
    ResolvedPermission {
        approved: decision.approved,
        cacheable_at: (!moved).then_some(generation),
    }
}

/// Settle one permission request and report only the verdict.
///
/// For the gates that hold no cache — there is nothing for them to do with the
/// stamp.
pub async fn check_security_permission(
    policy: &PolicyAccess,
    hub: &Arc<HostControlHub>,
    permission: Option<bool>,
    decided_at: u64,
    permission_type: SecurityPermissionType,
    from_connection_id: Option<String>,
    suppress_remember: bool,
) -> bool {
    resolve_permission(
        policy,
        hub,
        permission,
        decided_at,
        permission_type,
        from_connection_id,
        suppress_remember,
    )
    .await
    .approved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_control::{ApprovalResponse, HostControlHub, HostControlMessage};
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

    /// A worker-role policy handle plus the upstream a remembered answer would
    /// travel on, so a test can assert on both the verdict and what was sent
    /// back to the host.
    fn policy_for_test() -> (
        Arc<PolicyAccess>,
        Arc<crate::worker::policy_mirror::PolicyMirror>,
        tokio::sync::mpsc::UnboundedReceiver<desk_ipc_protocol::message::WorkerToService>,
    ) {
        PolicyAccess::for_test(SecuritySettings::default())
    }

    /// The capability a remembered answer names, or `None` when nothing was
    /// sent back to the host.
    fn remembered(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<desk_ipc_protocol::message::WorkerToService>,
    ) -> Option<(SecurityPermissionType, bool, u64)> {
        match rx.try_recv().ok()? {
            desk_ipc_protocol::message::WorkerToService::RememberSecurityDecision(payload) => {
                Some((
                    payload.capability,
                    payload.approved,
                    payload.expected_generation,
                ))
            }
            other => panic!("unexpected upstream message: {other:?}"),
        }
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
        let (policy, _mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        // No subscriber/responder — would block forever if the hub were consulted.
        let approved = tokio::time::timeout(
            Duration::from_millis(200),
            check_security_permission(
                &policy,
                &hub,
                Some(true),
                policy
                    .capability(SecurityPermissionType::RemoteControl)
                    .generation,
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
        let (policy, _mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        let approved = tokio::time::timeout(
            Duration::from_millis(200),
            check_security_permission(
                &policy,
                &hub,
                Some(false),
                policy
                    .capability(SecurityPermissionType::RemoteControl)
                    .generation,
                SecurityPermissionType::RemoteControl,
                None,
                false,
            ),
        )
        .await
        .expect("must short-circuit");
        assert!(!approved);
    }

    /// A worker cannot store the policy, so a remembered answer has to leave the
    /// process — carrying the stamp the host judges it against.
    #[tokio::test]
    async fn u20_check_with_remember_sends_the_answer_to_the_host() {
        let (policy, _mirror, mut rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );

        let generation = policy
            .capability(SecurityPermissionType::Terminal)
            .generation;
        let approved = check_security_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::Terminal)
                .generation,
            SecurityPermissionType::Terminal,
            None,
            false,
        )
        .await;

        assert!(approved);
        assert_eq!(
            remembered(&mut rx),
            Some((SecurityPermissionType::Terminal, true, generation))
        );
    }

    /// A capped grant / code-session (`suppress_remember = true`) that the local
    /// user approves *with* remember is honored for this request but must NOT widen
    /// the host global — the owner's per-code ceiling stays the only authority.
    #[tokio::test]
    async fn capped_session_remember_does_not_reach_the_host() {
        let (policy, _mirror, mut rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );

        let approved = check_security_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::Terminal)
                .generation,
            SecurityPermissionType::Terminal,
            None,
            true,
        )
        .await;

        // Honored for this one request...
        assert!(approved);
        // ...but nothing is proposed as the host's standing policy.
        assert_eq!(remembered(&mut rx), None);
    }

    /// Deciding is the half a gate can run without committing anything: nothing
    /// leaves the process until the caller acts on `remember`.
    #[tokio::test]
    async fn deciding_a_permission_commits_nothing() {
        let (policy, _mirror, mut rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );

        let decision = decide_security_permission(
            &policy,
            &hub,
            None,
            SecurityPermissionType::Terminal,
            None,
            false,
        )
        .await;

        assert!(decision.approved);
        assert!(decision.remember, "the user asked to remember this");
        assert_eq!(remembered(&mut rx), None, "deciding must not commit");
    }

    /// A capped session's "remember" must never reach a caller as something to
    /// commit — the suppression has to be decided here, not left to each gate.
    #[tokio::test]
    async fn a_capped_session_never_reports_a_remembered_answer() {
        let (policy, _mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );

        let decision = decide_security_permission(
            &policy,
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
        let (policy, _mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());

        for (configured, expected) in [(Some(true), true), (Some(false), false)] {
            let decision = decide_security_permission(
                &policy,
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

    // U-21: None + hub returns deny without remember → nothing committed.
    #[tokio::test]
    async fn u21_check_without_remember_commits_nothing() {
        let (policy, _mirror, mut rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: false,
                remember: false,
            },
        );

        let approved = check_security_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::FileBrowse)
                .generation,
            SecurityPermissionType::FileBrowse,
            None,
            false,
        )
        .await;
        assert!(!approved);
        assert_eq!(remembered(&mut rx), None);
    }

    /// Every capability must travel back under its own name. A gate that
    /// reported the wrong one would silently change a different setting.
    #[tokio::test]
    async fn u6_remember_names_the_capability_that_was_asked_about() {
        for capability in SecurityPermissionType::ALL {
            let (policy, _mirror, mut rx) = policy_for_test();
            let hub = Arc::new(HostControlHub::new_local());
            spawn_responder(
                &hub,
                ApprovalResponse {
                    approved: true,
                    remember: true,
                },
            );
            let stamp = policy.capability(*capability).generation;
            let _ = check_security_permission(&policy, &hub, None, stamp, *capability, None, false)
                .await;
            assert_eq!(
                remembered(&mut rx),
                Some((*capability, true, 0)),
                "wrong capability reported for {capability:?}"
            );
        }
    }

    /// The case the stamp exists for: the operator changed this capability while
    /// the user was answering. The answer settles the request in front of the
    /// user but must not be cached, or the next command would be decided by a
    /// policy the operator has already replaced.
    #[tokio::test]
    async fn an_answer_given_under_a_replaced_policy_is_not_cacheable() {
        let (policy, mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        {
            let hub = Arc::clone(&hub);
            let mirror = Arc::clone(&mirror);
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(HostControlMessage::SecurityApprovalRequest { req_id, .. }) => {
                            // The operator revokes the capability while the
                            // dialog is still up.
                            let mut published = mirror.snapshot();
                            published
                                .set_capability(SecurityPermissionType::FileTransfer, Some(false));
                            mirror.apply(published);
                            hub.submit_approval(
                                &req_id,
                                ApprovalResponse {
                                    approved: true,
                                    remember: false,
                                },
                            );
                            return;
                        }
                        Ok(_) => continue,
                        Err(_) => return,
                    }
                }
            });
        }

        let resolved = resolve_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::FileTransfer)
                .generation,
            SecurityPermissionType::FileTransfer,
            Some("conn-1".to_string()),
            false,
        )
        .await;

        assert!(resolved.approved, "the user did consent to this request");
        assert_eq!(
            resolved.cacheable_at, None,
            "the answer belongs to a policy that no longer exists"
        );
    }

    /// The ordinary case: nothing moved, so the answer is cacheable under the
    /// stamp it was decided at.
    #[tokio::test]
    async fn an_answer_given_under_a_stable_policy_is_cacheable() {
        let (policy, _mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        spawn_responder(
            &hub,
            ApprovalResponse {
                approved: true,
                remember: false,
            },
        );

        let resolved = resolve_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::Whiteboard)
                .generation,
            SecurityPermissionType::Whiteboard,
            Some("conn-1".to_string()),
            false,
        )
        .await;

        assert!(resolved.approved);
        assert_eq!(
            resolved.cacheable_at,
            Some(
                policy
                    .capability(SecurityPermissionType::Whiteboard)
                    .generation
            )
        );
    }

    /// Two gates hitting the same capability on the same connection are one
    /// question. Answering it once has to settle both, or the user is made to
    /// dismiss a dialog per command.
    #[tokio::test]
    async fn concurrent_gates_share_one_prompt() {
        let (policy, _mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let mut rx = hub.subscribe_outbound();
            hub.mark_tauri_connected();
            let hub = Arc::clone(&hub);
            let prompts = Arc::clone(&prompts);
            tokio::spawn(async move {
                while let Ok(message) = rx.recv().await {
                    if let HostControlMessage::SecurityApprovalRequest { req_id, .. } = message {
                        prompts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // Give the second caller time to arrive before the
                        // answer lands, so it must queue rather than race ahead.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        hub.submit_approval(
                            &req_id,
                            ApprovalResponse {
                                approved: true,
                                remember: false,
                            },
                        );
                    }
                }
            });
        }

        let first = check_security_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::FileBrowse)
                .generation,
            SecurityPermissionType::FileBrowse,
            Some("conn-1".to_string()),
            false,
        );
        let second = check_security_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::FileBrowse)
                .generation,
            SecurityPermissionType::FileBrowse,
            Some("conn-1".to_string()),
            false,
        );
        let (a, b) = tokio::join!(first, second);

        assert!(a && b, "both callers get the one answer");
        assert_eq!(
            prompts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the user must be asked once"
        );
    }

    /// Different connections are different questions even for the same
    /// capability — sharing them would let one controller's answer admit
    /// another.
    #[tokio::test]
    async fn different_connections_are_asked_separately() {
        let (policy, _mirror, _rx) = policy_for_test();
        let hub = Arc::new(HostControlHub::new_local());
        let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let mut rx = hub.subscribe_outbound();
            hub.mark_tauri_connected();
            let hub = Arc::clone(&hub);
            let prompts = Arc::clone(&prompts);
            tokio::spawn(async move {
                while let Ok(message) = rx.recv().await {
                    if let HostControlMessage::SecurityApprovalRequest { req_id, .. } = message {
                        prompts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        hub.submit_approval(
                            &req_id,
                            ApprovalResponse {
                                approved: true,
                                remember: false,
                            },
                        );
                    }
                }
            });
        }

        let first = check_security_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::FileBrowse)
                .generation,
            SecurityPermissionType::FileBrowse,
            Some("conn-1".to_string()),
            false,
        );
        let second = check_security_permission(
            &policy,
            &hub,
            None,
            policy
                .capability(SecurityPermissionType::FileBrowse)
                .generation,
            SecurityPermissionType::FileBrowse,
            Some("conn-2".to_string()),
            false,
        );
        let (a, b) = tokio::join!(first, second);

        assert!(a && b);
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
