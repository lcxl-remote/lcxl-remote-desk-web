//! Pending-approval store for the confirm-execution flow.
//!
//! When `ConfirmExec` classifies a command as executable, the daemon mints an
//! `exec_request_id`, renders an **immutable** [`ExecPlanDraft`], and parks it
//! here keyed by that id. `ResolveExec` later looks the id up, consumes it
//! (removing it so it can never be approved twice), and — on approve — seals the
//! stored draft into an `ExecPlan` with a freshly minted `approval_id`. The
//! draft is sealed at preview time and never re-rendered, so the previewed plan
//! is exactly the executed plan.
//!
//! State is in-memory and short-lived: a daemon restart simply drops pending
//! approvals (the control end re-previews). Entries expire after [`TTL`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use desk_agent_protocol::exec::{
    ApprovalId, CommandClassification, ExecPlan, ExecPlanDraft, ExecRequestId,
};

/// How long a pending approval stays valid before it is treated as expired.
pub const TTL: Duration = Duration::from_secs(120);

/// One parked, executable preview awaiting the user's decision.
struct PendingApproval {
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
}

/// Outcome of consuming a pending approval.
pub struct ConsumedApproval {
    pub draft: ExecPlanDraft,
    pub classification: CommandClassification,
    pub connection_id: Option<String>,
    /// See [`PendingApproval::session_grant_template`].
    pub session_grant_template: Option<String>,
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
        draft: ExecPlanDraft,
        classification: CommandClassification,
        connection_id: Option<String>,
        session_grant_template: Option<String>,
    ) -> ExecRequestId {
        let id = mint_exec_request_id();
        let mut map = self.inner.lock().expect("pending approvals lock");
        evict_expired(&mut map);
        map.insert(
            id.0.clone(),
            PendingApproval {
                draft,
                classification,
                created_at: Instant::now(),
                connection_id,
                session_grant_template,
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
            draft: pending.draft,
            classification: pending.classification,
            connection_id: pending.connection_id,
            session_grant_template: pending.session_grant_template,
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
pub fn seal_plan(exec_request_id: ExecRequestId, draft: ExecPlanDraft) -> (ApprovalId, ExecPlan) {
    let approval_id = ApprovalId(format!("appr_{}", uuid::Uuid::new_v4().simple()));
    let plan = ExecPlan::from_draft(exec_request_id, approval_id.clone(), draft);
    (approval_id, plan)
}

fn evict_expired(map: &mut HashMap<String, PendingApproval>) {
    map.retain(|_, p| p.created_at.elapsed() <= TTL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::exec::{ExecDecision, ExecEffect, ExecShellKind};

    fn draft() -> ExecPlanDraft {
        ExecPlanDraft {
            program: "docker".into(),
            argv: vec!["restart".into(), "web1".into()],
            cwd: None,
            shell: ExecShellKind::Native,
            risk: RiskLevel::High,
            template_id: "docker_restart".into(),
            fingerprint: "fp".into(),
            timeout_ms: 30_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
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
        let id = store.insert(draft(), classification(), Some("conn1".into()), None);
        assert_eq!(store.len(), 1);

        let consumed = is_consumed(store.take(&id, Some("conn1"))).expect("first take");
        assert_eq!(consumed.draft.template_id, "docker_restart");
        assert_eq!(consumed.connection_id.as_deref(), Some("conn1"));
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
            draft(),
            classification(),
            Some("conn1".into()),
            Some("docker_restart".into()),
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
        let id = store.insert(draft(), classification(), Some("owner".into()), None);

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
        let (approval_id, plan) = seal_plan(id.clone(), d.clone());
        assert!(approval_id.0.starts_with("appr_"));
        assert_eq!(plan.exec_request_id, id);
        assert_eq!(plan.approval_id, approval_id);
        assert_eq!(plan.argv, d.argv);
        assert_eq!(plan.fingerprint, d.fingerprint);
    }

    #[test]
    fn minted_ids_are_unique() {
        let store = PendingApprovalStore::new();
        let a = store.insert(draft(), classification(), None, None);
        let b = store.insert(draft(), classification(), None, None);
        assert_ne!(a, b);
    }
}
