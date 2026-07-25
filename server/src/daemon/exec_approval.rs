//! Pending-approval store for the confirm-execution flow.
//!
//! When `ConfirmExec` classifies a command as executable, the daemon mints an
//! `exec_request_id`, renders an **immutable** [`ExecPlanDraft`], and parks it
//! here keyed by that id. `ResolveExec` later looks the id up, consumes it
//! (removing it so it can never be approved twice), and — on approve — seals the
//! stored draft into an `ExecPlan` with a freshly minted `approval_id`. The
//! original input and admission policy are retained so approval can rebuild the
//! draft against the latest local policy snapshot. Only an exactly equal rebuild
//! seals the draft that was shown at preview time.
//!
//! State is in-memory and short-lived: a daemon restart simply drops pending
//! approvals (the control end re-previews). Entries expire after [`TTL`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use desk_agent_protocol::ExecInput;
use desk_agent_protocol::ExecutionMode;
use desk_agent_protocol::authz::ExecAdmissionPolicy;
use desk_agent_protocol::exec::{
    ApprovalId, CommandClassification, ExecPlan, ExecPlanDraft, ExecRequestId,
};

/// How long a pending approval stays valid before it is treated as expired.
pub const TTL: Duration = Duration::from_secs(120);

/// One parked, executable preview awaiting the user's decision.
struct PendingApproval {
    input: ExecInput,
    admission_policy: ExecAdmissionPolicy,
    execution_mode: ExecutionMode,
    draft: ExecPlanDraft,
    classification: CommandClassification,
    created_at: Instant,
    /// The control-end connection that requested it (where the result is sent).
    connection_id: Option<String>,
    /// `Some(template_id)` when the active mode is `SessionApproved` and the
    /// command matched a template: approving this preview grants that template
    /// for the rest of the connection's session (subsequent matching commands
    /// skip confirmation). `None` for one-shot confirmation (e.g.
    /// `ConfirmEachAction`), which never widens beyond the single command.
    session_grant_template: Option<String>,
    /// Originating ConfirmExec frame `request_id` — the manager's authorization
    /// ledger key. Threaded through `ResolveExec` and the worker round-trip so
    /// every exec lifecycle audit event can be attributed to the real operator.
    /// `None` on the single-machine / non-manager path.
    source_request_id: Option<String>,
}

/// Outcome of consuming a pending approval.
pub struct ConsumedApproval {
    pub input: ExecInput,
    pub admission_policy: ExecAdmissionPolicy,
    pub execution_mode: ExecutionMode,
    pub draft: ExecPlanDraft,
    pub classification: CommandClassification,
    pub connection_id: Option<String>,
    /// See [`PendingApproval::session_grant_template`].
    pub session_grant_template: Option<String>,
    /// See [`PendingApproval::source_request_id`].
    pub source_request_id: Option<String>,
}

/// Result of attempting to consume a pending approval.
pub enum TakeOutcome {
    /// Consumed and removed; ready to seal/execute or audit a rejection.
    Consumed(ConsumedApproval),
    /// Unknown, already consumed, or expired.
    NotFound,
    /// Exists but belongs to a different connection — left in place.
    Forbidden,
}

/// In-memory map of `exec_request_id` → pending approval. Cheap, brief locks
/// (no `.await` is held across the mutex).
#[derive(Default)]
pub struct PendingApprovalStore {
    inner: Mutex<HashMap<String, PendingApproval>>,
}

impl PendingApprovalStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park an executable draft and return its freshly minted
    /// [`ExecRequestId`]. Opportunistically evicts expired entries.
    pub fn insert(
        &self,
        input: ExecInput,
        admission_policy: ExecAdmissionPolicy,
        execution_mode: ExecutionMode,
        draft: ExecPlanDraft,
        classification: CommandClassification,
        connection_id: Option<String>,
        session_grant_template: Option<String>,
        source_request_id: Option<String>,
    ) -> ExecRequestId {
        let id = mint_exec_request_id();
        let mut map = self.inner.lock().expect("pending approvals lock");
        evict_expired(&mut map);
        map.insert(
            id.0.clone(),
            PendingApproval {
                input,
                admission_policy,
                execution_mode,
                draft,
                classification,
                created_at: Instant::now(),
                connection_id,
                session_grant_template,
                source_request_id,
            },
        );
        id
    }

    /// Look up and **remove** a pending approval (consume-once), bound to the
    /// connection that requested the preview. Removing on every successful take —
    /// approve *or* expired — closes replay and concurrent double-approve.
    ///
    /// `connection_id` is the resolving control end's connection; it must match
    /// the one that created the pending. On a **mismatch the entry is left in
    /// place** ([`TakeOutcome::Forbidden`]) so a control end that learned a
    /// stray `exec_request_id` can neither act on nor evict another connection's
    /// pending command.
    pub fn take(&self, id: &ExecRequestId, connection_id: Option<&str>) -> TakeOutcome {
        let mut map = self.inner.lock().expect("pending approvals lock");
        let Some(pending) = map.get(&id.0) else {
            return TakeOutcome::NotFound;
        };
        if pending.connection_id.as_deref() != connection_id {
            return TakeOutcome::Forbidden;
        }
        // Connection matches — consume it.
        let pending = map.remove(&id.0).expect("present");
        if pending.created_at.elapsed() > TTL {
            return TakeOutcome::NotFound;
        }
        TakeOutcome::Consumed(ConsumedApproval {
            input: pending.input,
            admission_policy: pending.admission_policy,
            execution_mode: pending.execution_mode,
            draft: pending.draft,
            classification: pending.classification,
            connection_id: pending.connection_id,
            session_grant_template: pending.session_grant_template,
            source_request_id: pending.source_request_id,
        })
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("pending approvals lock").len()
    }
}

/// Mint a fresh `exec_request_id`. Used both when parking a pending approval
/// and on the session-approved auto-execute path, which seals a plan directly
/// without ever parking it.
pub fn mint_exec_request_id() -> ExecRequestId {
    ExecRequestId(format!("exec_{}", uuid::Uuid::new_v4().simple()))
}

/// Seal a consumed draft into an approved [`ExecPlan`], minting the
/// `approval_id`. The one place an approval id is created — a control-end /
/// model-supplied value can never reach here.
///
/// On this path the two identity axes are already present and just need naming:
/// `exec_request_id` is the daemon-minted task id, stable from the classifying
/// `ConfirmExec` through to the `ResolveExec` that approves it, while
/// `execution_generation` is the id of the frame that actually triggers this
/// dispatch. Only one frame ever triggers execution — a preview that is merely
/// classified is not a generation of anything.
pub fn seal_plan(
    exec_request_id: ExecRequestId,
    execution_generation: &str,
    draft: ExecPlanDraft,
) -> (ApprovalId, ExecPlan) {
    let approval_id = ApprovalId(format!("appr_{}", uuid::Uuid::new_v4().simple()));
    let plan = ExecPlan::from_draft(
        exec_request_id,
        execution_generation,
        approval_id.clone(),
        draft,
    );
    (approval_id, plan)
}

fn evict_expired(map: &mut HashMap<String, PendingApproval>) {
    map.retain(|_, p| p.created_at.elapsed() <= TTL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::authz::ExecAdmissionPolicy;
    use desk_agent_protocol::exec::{ExecDecision, ExecEffect, ExecExecutionBasis, ExecShellKind};

    fn input() -> ExecInput {
        ExecInput {
            target: desk_agent_protocol::ExecTarget::Shell {
                shell: "powershell".into(),
            },
            command: "docker restart web1".into(),
            cwd: None,
            timeout_ms: 30_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        }
    }

    fn draft() -> ExecPlanDraft {
        ExecPlanDraft {
            program: "docker".into(),
            argv: vec!["restart".into(), "web1".into()],
            cwd: None,
            shell: ExecShellKind::Native,
            risk: RiskLevel::High,
            execution_basis: ExecExecutionBasis::Template,
            template_id: "docker_restart".into(),
            fingerprint: "fp".into(),
            timeout_ms: 30_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
            containment: Default::default(),
        }
    }

    fn classification() -> CommandClassification {
        CommandClassification {
            risk: RiskLevel::High,
            matched_template: Some("docker_restart".into()),
            impact: "Restart a Docker container".into(),
            decision: ExecDecision::ConfirmRequired,
            effect: Some(ExecEffect::Mutating),
        }
    }

    fn is_consumed(o: TakeOutcome) -> Option<ConsumedApproval> {
        match o {
            TakeOutcome::Consumed(c) => Some(c),
            _ => None,
        }
    }

    #[test]
    fn insert_then_take_returns_the_draft_once() {
        let store = PendingApprovalStore::new();
        let id = store.insert(
            input(),
            ExecAdmissionPolicy::TemplateOnly,
            ExecutionMode::ConfirmEachAction,
            draft(),
            classification(),
            Some("conn1".into()),
            None,
            Some("frame_req_1".into()),
        );
        assert_eq!(store.len(), 1);

        let consumed = is_consumed(store.take(&id, Some("conn1"))).expect("first take");
        assert_eq!(consumed.draft.template_id, "docker_restart");
        assert_eq!(consumed.connection_id.as_deref(), Some("conn1"));
        assert_eq!(consumed.input, input());
        assert_eq!(consumed.admission_policy, ExecAdmissionPolicy::TemplateOnly);
        assert_eq!(consumed.execution_mode, ExecutionMode::ConfirmEachAction);
        // The source frame request_id (ledger key) survives the round-trip.
        assert_eq!(consumed.source_request_id.as_deref(), Some("frame_req_1"));
        assert_eq!(store.len(), 0);

        // Second take (replay / concurrent double-approve) finds nothing.
        assert!(matches!(
            store.take(&id, Some("conn1")),
            TakeOutcome::NotFound
        ));
    }

    #[test]
    fn session_grant_template_round_trips_through_take() {
        let store = PendingApprovalStore::new();
        let id = store.insert(
            input(),
            ExecAdmissionPolicy::TemplateOnly,
            ExecutionMode::SessionApproved,
            draft(),
            classification(),
            Some("conn1".into()),
            Some("docker_restart".into()),
            None,
        );
        let consumed = is_consumed(store.take(&id, Some("conn1"))).expect("take");
        assert_eq!(
            consumed.session_grant_template.as_deref(),
            Some("docker_restart")
        );
    }

    #[test]
    fn unknown_id_is_not_found() {
        let store = PendingApprovalStore::new();
        assert!(matches!(
            store.take(&ExecRequestId("nope".into()), Some("conn1")),
            TakeOutcome::NotFound
        ));
    }

    #[test]
    fn take_from_other_connection_is_forbidden_and_keeps_pending() {
        let store = PendingApprovalStore::new();
        let id = store.insert(
            input(),
            ExecAdmissionPolicy::TemplateOnly,
            ExecutionMode::ConfirmEachAction,
            draft(),
            classification(),
            Some("owner".into()),
            None,
            None,
        );

        // A different connection cannot consume — and the pending is preserved.
        assert!(matches!(
            store.take(&id, Some("attacker")),
            TakeOutcome::Forbidden
        ));
        assert_eq!(store.len(), 1, "forbidden take must not evict the pending");

        // The owning connection still can.
        assert!(is_consumed(store.take(&id, Some("owner"))).is_some());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn seal_plan_mints_approval_and_preserves_draft() {
        let id = ExecRequestId("exec_1".into());
        let d = draft();
        let (approval_id, plan) = seal_plan(id.clone(), "gen-1", d.clone());
        assert!(approval_id.0.starts_with("appr_"));
        assert_eq!(plan.exec_request_id, id);
        assert_eq!(plan.approval_id, approval_id);
        assert_eq!(plan.argv, d.argv);
        assert_eq!(plan.fingerprint, d.fingerprint);
    }

    #[test]
    fn minted_ids_are_unique() {
        let store = PendingApprovalStore::new();
        let a = store.insert(
            input(),
            ExecAdmissionPolicy::TemplateOnly,
            ExecutionMode::ConfirmEachAction,
            draft(),
            classification(),
            None,
            None,
            None,
        );
        let b = store.insert(
            input(),
            ExecAdmissionPolicy::TemplateOnly,
            ExecutionMode::ConfirmEachAction,
            draft(),
            classification(),
            None,
            None,
            None,
        );
        assert_ne!(a, b);
    }
}
