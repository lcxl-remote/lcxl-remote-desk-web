//! Per-candidate dispatch of scheduled work shared by both hosts.
//!
//! Hosts own scanning, transactions, device routing, the turn task and
//! logging. This module only orders the admission, claim, turn and settlement
//! steps and classifies the result. A claimed occurrence is never retried
//! here: anything after the claim that cannot be proven is reconciled.
use crate::agent_loop::LoopOutcome;
use async_trait::async_trait;
use desk_agent_protocol::AgentError;

#[cfg(test)]
mod tests;

/// Scan-report bucket for one candidate. Hosts only count and log it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchResult {
    Settled,
    Waiting,
    Deferred,
    Reconcile,
}

/// `stopped` is the turn error the host logs after dispatch returns.
#[derive(Debug)]
pub struct DispatchReport {
    pub result: DispatchResult,
    pub stopped: Option<AgentError>,
}

impl DispatchReport {
    fn of(result: DispatchResult) -> Self {
        Self {
            result,
            stopped: None,
        }
    }
}

/// Host-independent candidate facts from the run row and its frozen snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchCandidate {
    /// Run row primary key, used to re-read the active row after a claim.
    pub id: i64,
    pub owner: i32,
    pub schedule_id: String,
    pub run_id: String,
    pub conversation_id: String,
    pub status: String,
    /// `target_device_id` of the run's frozen task snapshot.
    pub device_id: String,
}

/// A lookup that cannot decide (several live connections, a presence error,
/// an unparsable id) is `Unknown`: defer without recording a device wait.
#[derive(Debug)]
pub enum DevicePresence<T> {
    Online(T),
    Offline,
    Unknown,
}

/// Owned lease identity, taken from the claim before the turn consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseIds {
    pub run_epoch: i64,
    pub session_token: u64,
}

#[derive(Debug)]
pub enum TurnRun {
    /// Cancellation registration failed or the owned task was lost after the
    /// claim. Neither proves completion, so the run needs reconciliation.
    Unaccounted,
    Finished(Result<LoopOutcome, AgentError>),
}

/// Run status a fresh-task settlement left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshSettled {
    AwaitingChildren,
    OutcomeUnknown,
    Other,
}

/// The fresh task's own session after a non-answer outcome.
#[derive(Debug)]
pub enum PersistedError {
    Unreadable,
    Clean,
    Failed(AgentError),
}

#[async_trait(?Send)]
pub trait ContinuationDispatchHost {
    /// Host device route.
    type Target;
    /// Source session handed to the initial claim.
    type Session;
    type Claimed;
    fn admits(&self, candidate: &DispatchCandidate) -> bool;
    async fn load_session(&self, candidate: &DispatchCandidate) -> Option<Self::Session>;
    async fn device_presence(&self, device_id: &str) -> DevicePresence<Self::Target>;
    /// Errors are ignored: the run is deferred either way.
    async fn wait_for_device(&self, run_id: &str);
    async fn claim_initial(
        &self,
        candidate: &DispatchCandidate,
        session: Self::Session,
        target: Self::Target,
    ) -> Option<(LeaseIds, Self::Claimed)>;
    async fn claim_permission(
        &self,
        candidate: &DispatchCandidate,
    ) -> Option<(LeaseIds, Self::Claimed)>;
    async fn run_turn(&self, claimed: Self::Claimed) -> TurnRun;
    async fn finish_answered(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        answer: &str,
    ) -> bool;
    async fn await_permission(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        request_id: &str,
    ) -> bool;
    /// `None` when the session is unreadable or holds no persisted error.
    async fn persisted_error(&self, candidate: &DispatchCandidate) -> Option<AgentError>;
    async fn finish_failed(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        error: &AgentError,
    ) -> bool;
}

#[async_trait(?Send)]
pub trait FreshDispatchHost {
    type Target;
    type Claimed;
    async fn device_presence(&self, device_id: &str) -> DevicePresence<Self::Target>;
    /// Errors are ignored: the run is deferred either way.
    async fn wait_for_device(&self, run_id: &str);
    /// Readiness, tool projection, the claim and the re-read of the active row
    /// for its lease epoch. Any failure, including that re-read, is `None`;
    /// an occurrence claimed but not run is recovered by lease expiry.
    async fn claim(
        &self,
        candidate: &DispatchCandidate,
        target: Self::Target,
    ) -> Option<(LeaseIds, Self::Claimed)>;
    async fn run_turn(&self, claimed: Self::Claimed) -> TurnRun;
    /// `None` on a failed settlement write.
    async fn finish_answered(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        answer: &str,
    ) -> Option<FreshSettled>;
    async fn await_children(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        wait_id: &str,
    ) -> bool;
    async fn await_permission(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        request_id: &str,
    ) -> bool;
    async fn persisted_error(&self, candidate: &DispatchCandidate) -> PersistedError;
    async fn finish_failed(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        error: &AgentError,
    ) -> Option<FreshSettled>;
}

fn waiting_or_reconcile(written: bool) -> DispatchResult {
    if written {
        DispatchResult::Waiting
    } else {
        DispatchResult::Reconcile
    }
}

fn settled_or_reconcile(written: bool) -> DispatchResult {
    if written {
        DispatchResult::Settled
    } else {
        DispatchResult::Reconcile
    }
}

fn fresh_result(settled: Option<FreshSettled>) -> DispatchResult {
    match settled {
        Some(FreshSettled::AwaitingChildren) => DispatchResult::Waiting,
        Some(FreshSettled::OutcomeUnknown) | None => DispatchResult::Reconcile,
        Some(FreshSettled::Other) => DispatchResult::Settled,
    }
}

/// A conversation timer or its permission resume. The session is read before
/// the device so a missing session never records a device wait.
pub async fn dispatch_continuation<H: ContinuationDispatchHost>(
    host: &H,
    candidate: &DispatchCandidate,
) -> DispatchReport {
    if !host.admits(candidate) {
        return DispatchReport::of(DispatchResult::Deferred);
    }
    let claimed = if candidate.status == "awaiting_permission" {
        host.claim_permission(candidate).await
    } else {
        let Some(session) = host.load_session(candidate).await else {
            return DispatchReport::of(DispatchResult::Deferred);
        };
        match host.device_presence(&candidate.device_id).await {
            DevicePresence::Online(target) => host.claim_initial(candidate, session, target).await,
            DevicePresence::Offline => {
                host.wait_for_device(&candidate.run_id).await;
                return DispatchReport::of(DispatchResult::Deferred);
            }
            DevicePresence::Unknown => return DispatchReport::of(DispatchResult::Deferred),
        }
    };
    let Some((lease, claimed)) = claimed else {
        return DispatchReport::of(DispatchResult::Deferred);
    };
    let outcome = match host.run_turn(claimed).await {
        TurnRun::Unaccounted => return DispatchReport::of(DispatchResult::Reconcile),
        TurnRun::Finished(outcome) => outcome,
    };
    let stopped = outcome.as_ref().err().cloned();
    let result = match outcome {
        Ok(LoopOutcome::Answered(answer)) => {
            settled_or_reconcile(host.finish_answered(candidate, lease, &answer).await)
        }
        Ok(LoopOutcome::PermissionRequested { request_id }) => {
            waiting_or_reconcile(host.await_permission(candidate, lease, &request_id).await)
        }
        // A loop outcome or transport error alone is not completion. Settlement
        // verifies the persisted error, the exact turn and both leases.
        _ => match host.persisted_error(candidate).await {
            Some(error) => settled_or_reconcile(host.finish_failed(candidate, lease, &error).await),
            None => DispatchResult::Reconcile,
        },
    };
    DispatchReport { result, stopped }
}

/// An isolated task occurrence in its own session.
pub async fn dispatch_fresh<H: FreshDispatchHost>(
    host: &H,
    candidate: &DispatchCandidate,
) -> DispatchReport {
    let target = match host.device_presence(&candidate.device_id).await {
        DevicePresence::Online(target) => target,
        DevicePresence::Offline => {
            // A started occurrence waiting on approval or children keeps its state.
            if !matches!(
                candidate.status.as_str(),
                "awaiting_permission" | "awaiting_children"
            ) {
                host.wait_for_device(&candidate.run_id).await;
            }
            return DispatchReport::of(DispatchResult::Deferred);
        }
        DevicePresence::Unknown => return DispatchReport::of(DispatchResult::Deferred),
    };
    let Some((lease, claimed)) = host.claim(candidate, target).await else {
        return DispatchReport::of(DispatchResult::Deferred);
    };
    let outcome = match host.run_turn(claimed).await {
        TurnRun::Unaccounted => return DispatchReport::of(DispatchResult::Reconcile),
        TurnRun::Finished(outcome) => outcome,
    };
    let stopped = outcome.as_ref().err().cloned();
    let result = match outcome {
        Ok(LoopOutcome::Answered(answer)) => {
            fresh_result(host.finish_answered(candidate, lease, &answer).await)
        }
        Ok(LoopOutcome::SubAgentsWaiting { wait_id }) => {
            waiting_or_reconcile(host.await_children(candidate, lease, &wait_id).await)
        }
        Ok(LoopOutcome::PermissionRequested { request_id }) => {
            waiting_or_reconcile(host.await_permission(candidate, lease, &request_id).await)
        }
        _ => match host.persisted_error(candidate).await {
            PersistedError::Unreadable => DispatchResult::Deferred,
            PersistedError::Clean => DispatchResult::Reconcile,
            PersistedError::Failed(error) => {
                fresh_result(host.finish_failed(candidate, lease, &error).await)
            }
        },
    };
    DispatchReport { result, stopped }
}
