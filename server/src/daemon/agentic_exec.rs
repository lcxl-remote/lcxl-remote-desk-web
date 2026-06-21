//! Await-based coordination for the Direct agentic exec path.
//!
//! The single-machine confirm-exec flow ([`exec_approval`](super::exec_approval))
//! is request/response: a `ConfirmExec` parks a draft and a later `ResolveExec`
//! consumes it and dispatches. The **agentic** path inverts control — the model
//! initiates `exec_command` *inside the running loop*, so the seam must **block**
//! on the operator's decision and then on the worker's result before the loop can
//! continue (the result is fed back to the model).
//!
//! This coordinator bridges those two synchronous inbound frames
//! (`ResolveExec`, the worker's `ExecResult`) to the awaiting seam via oneshots,
//! keyed by `exec_request_id`:
//!
//! - the approver registers an approval channel, pushes an `ExecPreview`, and
//!   awaits the decision; the `ResolveExec` inbound handler fires it;
//! - the runner registers a result channel, dispatches the plan, and awaits the
//!   outcome; the worker's `ExecResult` (intercepted in the signaling proxy)
//!   fires it instead of being forwarded to the browser.
//!
//! State is process-local and short-lived (one in-flight agentic exec at a time
//! per turn, bound to the live control connection); the awaiting side owns the
//! timeout, and a closed connection drains its pending entries. There is no
//! cross-instance concern here — the Direct runtime is a single daemon process.

use std::collections::HashMap;
use std::sync::Mutex;

use desk_agent_protocol::AgentOutcome;
use tokio::sync::oneshot;

/// Bridges the inbound `ResolveExec` / worker `ExecResult` frames to the awaiting
/// agentic seam. Two maps keyed by `exec_request_id`: the operator's approval
/// decision and the worker's execution outcome.
#[derive(Default)]
pub struct AgenticExecCoordinator {
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    results: Mutex<HashMap<String, oneshot::Sender<AgentOutcome>>>,
}

impl AgenticExecCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an awaiting approval for `exec_request_id`, returning the receiver
    /// the approver awaits. A duplicate id replaces the prior sender (its receiver
    /// then resolves to a channel-closed error — treated as cancelled).
    pub fn register_approval(&self, exec_request_id: String) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.approvals
            .lock()
            .expect("agentic approvals lock")
            .insert(exec_request_id, tx);
        rx
    }

    /// Deliver the operator's decision for `exec_request_id` to the awaiting
    /// approver. Returns `true` if an agentic approval was waiting (so the inbound
    /// `ResolveExec` handler knows it was consumed here and must not fall through to
    /// the single-machine park/consume flow).
    pub fn resolve_approval(&self, exec_request_id: &str, approved: bool) -> bool {
        let Some(tx) = self
            .approvals
            .lock()
            .expect("agentic approvals lock")
            .remove(exec_request_id)
        else {
            return false;
        };
        // A failed send means the approver already gave up (timeout); still report
        // that this id was an agentic one so the caller does not double-handle it.
        let _ = tx.send(approved);
        true
    }

    /// Drop an awaiting approval without delivering (the approver timed out / the
    /// control connection closed). Idempotent.
    pub fn cancel_approval(&self, exec_request_id: &str) {
        self.approvals
            .lock()
            .expect("agentic approvals lock")
            .remove(exec_request_id);
    }

    /// Register an awaiting result for `exec_request_id`, returning the receiver the
    /// runner awaits.
    pub fn register_result(&self, exec_request_id: String) -> oneshot::Receiver<AgentOutcome> {
        let (tx, rx) = oneshot::channel();
        self.results
            .lock()
            .expect("agentic results lock")
            .insert(exec_request_id, tx);
        rx
    }

    /// Deliver the worker's outcome for `exec_request_id` to the awaiting runner.
    /// Returns `true` if an agentic runner was waiting (so the proxy suppresses the
    /// browser-bound `ExecResult` frame and lets the loop consume the result).
    pub fn deliver_result(&self, exec_request_id: &str, outcome: AgentOutcome) -> bool {
        let Some(tx) = self
            .results
            .lock()
            .expect("agentic results lock")
            .remove(exec_request_id)
        else {
            return false;
        };
        let _ = tx.send(outcome);
        true
    }

    /// Drop an awaiting result without delivering (the runner timed out). Idempotent.
    pub fn cancel_result(&self, exec_request_id: &str) {
        self.results
            .lock()
            .expect("agentic results lock")
            .remove(exec_request_id);
    }

    #[cfg(test)]
    pub fn pending_counts(&self) -> (usize, usize) {
        (
            self.approvals.lock().unwrap().len(),
            self.results.lock().unwrap().len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{ExecOutput, OperationOutput};

    fn exec_outcome() -> AgentOutcome {
        AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: 1,
            redactions: vec![],
        }))
    }

    /// An approval registered then resolved delivers the decision and reports the
    /// id as agentic; the entry is removed.
    #[tokio::test]
    async fn approval_round_trip() {
        let c = AgenticExecCoordinator::new();
        let rx = c.register_approval("e1".into());
        assert_eq!(c.pending_counts().0, 1);
        assert!(c.resolve_approval("e1", true), "agentic id matched");
        assert!(rx.await.unwrap());
        assert_eq!(c.pending_counts().0, 0);
    }

    /// Resolving an unknown id reports it as not-agentic (the caller falls through
    /// to the single-machine flow).
    #[test]
    fn resolve_unknown_is_not_agentic() {
        let c = AgenticExecCoordinator::new();
        assert!(!c.resolve_approval("ghost", true));
    }

    /// A result registered then delivered hands the outcome to the runner and
    /// reports it as agentic (so the proxy suppresses the browser frame).
    #[tokio::test]
    async fn result_round_trip() {
        let c = AgenticExecCoordinator::new();
        let rx = c.register_result("e1".into());
        assert!(c.deliver_result("e1", exec_outcome()), "agentic id matched");
        let outcome = rx.await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Ok(_)));
        assert_eq!(c.pending_counts().1, 0);
    }

    /// Delivering an unknown result reports not-agentic (a non-agentic / browser
    /// exec result, which the proxy forwards as usual).
    #[test]
    fn deliver_unknown_is_not_agentic() {
        let c = AgenticExecCoordinator::new();
        assert!(!c.deliver_result("ghost", exec_outcome()));
    }

    /// Cancelling removes a pending entry so a later resolve/deliver is a no-op.
    #[test]
    fn cancel_removes_pending() {
        let c = AgenticExecCoordinator::new();
        let _rx = c.register_approval("e1".into());
        c.cancel_approval("e1");
        assert!(!c.resolve_approval("e1", true));
        let _rx2 = c.register_result("e2".into());
        c.cancel_result("e2");
        assert!(!c.deliver_result("e2", exec_outcome()));
    }
}
