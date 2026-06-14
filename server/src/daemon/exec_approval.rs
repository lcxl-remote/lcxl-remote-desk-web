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
    #[allow(dead_code)] // recorded for the PR4 audit wiring
    classification: CommandClassification,
    created_at: Instant,
    /// The control-end connection that requested it (where the result is sent).
    connection_id: Option<String>,
}

/// Outcome of consuming a pending approval.
pub struct ConsumedApproval {
    pub draft: ExecPlanDraft,
    pub classification: CommandClassification,
    pub connection_id: Option<String>,
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
    ) -> ExecRequestId {
        let id = format!("exec_{}", uuid::Uuid::new_v4().simple());
        let mut map = self.inner.lock().expect("pending approvals lock");
        evict_expired(&mut map);
        map.insert(
            id.clone(),
            PendingApproval {
                draft,
                classification,
                created_at: Instant::now(),
                connection_id,
            },
        );
        ExecRequestId(id)
    }

    /// Look up and **remove** a pending approval (consume-once). Returns `None`
    /// if it is unknown, already consumed, or expired. Removing on every take —
    /// approve *or* expired — closes replay and concurrent double-approve.
    pub fn take(&self, id: &ExecRequestId) -> Option<ConsumedApproval> {
        let mut map = self.inner.lock().expect("pending approvals lock");
        let pending = map.remove(&id.0)?;
        if pending.created_at.elapsed() > TTL {
            return None;
        }
        Some(ConsumedApproval {
            draft: pending.draft,
            classification: pending.classification,
            connection_id: pending.connection_id,
        })
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("pending approvals lock").len()
    }
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

    #[test]
    fn insert_then_take_returns_the_draft_once() {
        let store = PendingApprovalStore::new();
        let id = store.insert(draft(), classification(), Some("conn1".into()));
        assert_eq!(store.len(), 1);

        let consumed = store.take(&id).expect("first take");
        assert_eq!(consumed.draft.template_id, "docker_restart");
        assert_eq!(consumed.connection_id.as_deref(), Some("conn1"));
        assert_eq!(store.len(), 0);

        // Second take (replay / concurrent double-approve) finds nothing.
        assert!(store.take(&id).is_none());
    }

    #[test]
    fn unknown_id_is_none() {
        let store = PendingApprovalStore::new();
        assert!(store.take(&ExecRequestId("nope".into())).is_none());
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
        let a = store.insert(draft(), classification(), None);
        let b = store.insert(draft(), classification(), None);
        assert_ne!(a, b);
    }
}
