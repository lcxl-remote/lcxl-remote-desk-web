//! Single-writer lease state for Computer Use mutation.
//!
//! This state machine is intentionally independent from any adapter. UIA or
//! Office execution may only start after acquiring this lease; browser input,
//! unclassified local input, cancellation, expiry, or a generation mismatch
//! makes further steps fail closed. Invalidating a lease does not prove the
//! outstanding adapter operation stopped; only its release frees the writer.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputPreemptionSource {
    Browser,
    LocalExternal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterLeaseRequest {
    pub work_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
    pub approved_actor_id: String,
    pub interactive_session_incarnation: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriterLeaseStatus {
    Active,
    Preempted(InputPreemptionSource),
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterLeaseState {
    pub request: WriterLeaseRequest,
    pub input_epoch_at_acquire: u64,
    pub status: WriterLeaseStatus,
}

#[derive(Default)]
pub struct WriterLeaseCoordinator {
    state: Mutex<Option<WriterLeaseState>>,
}

impl WriterLeaseCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the worker's only AI writer slot. An exact duplicate dispatch is
    /// idempotent; every unreleased generation continues to occupy capacity.
    pub fn acquire(
        &self,
        request: WriterLeaseRequest,
        current_input_epoch: u64,
    ) -> Result<WriterLeaseState, AgentError> {
        validate_request(&request)?;
        let now = Utc::now();
        if request.expires_at <= now {
            return Err(error(
                AgentErrorKind::Timeout,
                "Computer Use writer lease request has expired",
                false,
            ));
        }
        let mut slot = self.state.lock().map_err(|_| unavailable())?;
        if let Some(existing) = slot.as_ref() {
            if existing.status == WriterLeaseStatus::Active
                && existing.request == request
                && existing.input_epoch_at_acquire == current_input_epoch
            {
                return Ok(existing.clone());
            }
            return Err(error(
                AgentErrorKind::HostAtCapacity,
                "another Computer Use writer lease is active for this interactive session",
                true,
            ));
        }
        let state = WriterLeaseState {
            request,
            input_epoch_at_acquire: current_input_epoch,
            status: WriterLeaseStatus::Active,
        };
        *slot = Some(state.clone());
        Ok(state)
    }

    /// Fence every adapter step immediately before it touches the OS object.
    pub fn require_active(
        &self,
        execution_generation: &str,
        current_input_epoch: u64,
    ) -> Result<WriterLeaseState, AgentError> {
        let slot = self.state.lock().map_err(|_| unavailable())?;
        let Some(state) = slot.as_ref() else {
            return Err(error(
                AgentErrorKind::Cancelled,
                "Computer Use writer lease is not active",
                false,
            ));
        };
        if state.request.execution_generation != execution_generation {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Computer Use execution generation does not own the writer lease",
                false,
            ));
        }
        if state.request.expires_at <= Utc::now() {
            return Err(error(
                AgentErrorKind::Timeout,
                "Computer Use writer lease expired before the next step",
                false,
            ));
        }
        if state.input_epoch_at_acquire != current_input_epoch {
            return Err(error(
                AgentErrorKind::Cancelled,
                "Computer Use writer lease was preempted by user input",
                false,
            ));
        }
        match state.status {
            WriterLeaseStatus::Active => Ok(state.clone()),
            WriterLeaseStatus::Preempted(_) => Err(error(
                AgentErrorKind::Cancelled,
                "Computer Use writer lease was preempted by user input",
                false,
            )),
            WriterLeaseStatus::Cancelled => Err(error(
                AgentErrorKind::Cancelled,
                "Computer Use writer lease was cancelled",
                false,
            )),
        }
    }

    pub fn preempt(&self, source: InputPreemptionSource) {
        let mut slot = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = slot.as_mut()
            && state.status == WriterLeaseStatus::Active
        {
            state.status = WriterLeaseStatus::Preempted(source);
        }
    }

    /// Cancellation targets the original actor and complete action identity.
    /// It forbids subsequent steps but does not assert that an OS call stopped.
    pub fn cancel(
        &self,
        cancel: &desk_agent_protocol::computer_use::ComputerActionCancel,
        approved_actor_id: &str,
    ) -> bool {
        let mut slot = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = slot.as_mut() else {
            return false;
        };
        if state.request.execution_generation != cancel.execution_generation
            || state.request.work_id != cancel.work_id
            || state.request.action_request_id != cancel.action_request_id
            || state.request.approved_actor_id != approved_actor_id
        {
            return false;
        }
        state.status = WriterLeaseStatus::Cancelled;
        true
    }

    /// Release only the matching generation. Late completion from an old
    /// execution cannot clear a newer writer lease.
    pub fn release(&self, execution_generation: &str) -> bool {
        let mut slot = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|state| state.request.execution_generation == execution_generation)
        {
            *slot = None;
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<WriterLeaseState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn validate_request(request: &WriterLeaseRequest) -> Result<(), AgentError> {
    for (field, value) in [
        ("work_id", request.work_id.as_str()),
        ("action_request_id", request.action_request_id.as_str()),
        ("approved_actor_id", request.approved_actor_id.as_str()),
        (
            "execution_generation",
            request.execution_generation.as_str(),
        ),
        (
            "interactive_session_incarnation",
            request.interactive_session_incarnation.as_str(),
        ),
    ] {
        if value.trim().is_empty() {
            return Err(error(
                AgentErrorKind::InvalidInput,
                &format!("Computer Use writer lease field `{field}` is empty"),
                false,
            ));
        }
    }
    Ok(())
}

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::Internal,
        "Computer Use writer lease state is unavailable",
        true,
    )
}

fn error(kind: AgentErrorKind, message: &str, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.to_string(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn request(generation: &str) -> WriterLeaseRequest {
        WriterLeaseRequest {
            work_id: "work-1".into(),
            action_request_id: "action-1".into(),
            execution_generation: generation.into(),
            approved_actor_id: "7".into(),
            interactive_session_incarnation: "session-1:worker-1".into(),
            expires_at: Utc::now() + Duration::seconds(30),
        }
    }

    #[test]
    fn exact_duplicate_is_idempotent_but_second_generation_is_rejected() {
        let leases = WriterLeaseCoordinator::new();
        let duplicate = request("generation-1");
        let first = leases.acquire(duplicate.clone(), 7).unwrap();
        assert_eq!(leases.acquire(duplicate, 7).unwrap(), first);
        let error = leases.acquire(request("generation-2"), 7).unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::HostAtCapacity);
    }

    #[test]
    fn browser_and_local_input_preempt_the_active_writer() {
        for source in [
            InputPreemptionSource::Browser,
            InputPreemptionSource::LocalExternal,
        ] {
            let leases = WriterLeaseCoordinator::new();
            leases.acquire(request("generation-1"), 0).unwrap();
            leases.preempt(source);
            assert_eq!(
                leases.snapshot().unwrap().status,
                WriterLeaseStatus::Preempted(source)
            );
            assert_eq!(
                leases.require_active("generation-1", 1).unwrap_err().kind,
                AgentErrorKind::Cancelled
            );
        }
    }

    #[test]
    fn stale_cancel_and_release_cannot_touch_a_newer_generation() {
        let leases = WriterLeaseCoordinator::new();
        leases.acquire(request("generation-2"), 0).unwrap();
        let cancel = |generation: &str| desk_agent_protocol::computer_use::ComputerActionCancel {
            work_id: "work-1".into(),
            action_request_id: "action-1".into(),
            execution_generation: generation.into(),
            reason: "owner stopped".into(),
        };
        assert!(!leases.cancel(&cancel("generation-1"), "7"));
        assert!(!leases.release("generation-1"));
        assert!(leases.require_active("generation-2", 0).is_ok());
        assert!(leases.cancel(&cancel("generation-2"), "7"));
        assert_eq!(
            leases.require_active("generation-2", 0).unwrap_err().kind,
            AgentErrorKind::Cancelled
        );
        assert!(leases.release("generation-2"));
        assert!(leases.snapshot().is_none());
    }

    #[test]
    fn expired_requests_and_epoch_changes_fail_closed() {
        let leases = WriterLeaseCoordinator::new();
        let mut expired = request("generation-expired");
        expired.expires_at = Utc::now() - Duration::seconds(1);
        assert_eq!(
            leases.acquire(expired, 0).unwrap_err().kind,
            AgentErrorKind::Timeout
        );

        leases.acquire(request("generation-1"), 4).unwrap();
        assert_eq!(
            leases.require_active("generation-1", 5).unwrap_err().kind,
            AgentErrorKind::Cancelled
        );
        assert_eq!(
            leases
                .require_active("generation-other", 4)
                .unwrap_err()
                .kind,
            AgentErrorKind::InvalidInput
        );
    }

    #[test]
    fn cancellation_requires_the_complete_original_identity() {
        use desk_agent_protocol::computer_use::ComputerActionCancel;
        let leases = WriterLeaseCoordinator::new();
        leases.acquire(request("generation-1"), 0).unwrap();
        let original = ComputerActionCancel {
            work_id: "work-1".into(),
            action_request_id: "action-1".into(),
            execution_generation: "generation-1".into(),
            reason: "owner stopped".into(),
        };
        for field in ["work", "action", "generation", "actor"] {
            let mut cancel = original.clone();
            let mut actor = "7";
            match field {
                "work" => cancel.work_id.push_str("-other"),
                "action" => cancel.action_request_id.push_str("-other"),
                "generation" => cancel.execution_generation.push_str("-other"),
                "actor" => actor = "8",
                _ => unreachable!(),
            }
            assert!(!leases.cancel(&cancel, actor), "{field}");
            assert!(leases.require_active("generation-1", 0).is_ok());
        }
        assert!(leases.cancel(&original, "7"));
        assert!(leases.cancel(&original, "7"));
        assert_eq!(
            leases.snapshot().unwrap().status,
            WriterLeaseStatus::Cancelled
        );
        assert!(leases.release("generation-1"));
        assert!(!leases.cancel(&original, "7"));
    }

    #[test]
    fn invalidation_does_not_release_an_outstanding_adapter_writer() {
        for case in ["cancel", "preempt", "expiry", "epoch"] {
            let leases = WriterLeaseCoordinator::new();
            let original = request("generation-1");
            leases.acquire(original.clone(), 0).unwrap();
            let mut epoch = 0;
            match case {
                "cancel" => {
                    assert!(leases.cancel(
                        &desk_agent_protocol::computer_use::ComputerActionCancel {
                            work_id: original.work_id.clone(),
                            action_request_id: original.action_request_id.clone(),
                            execution_generation: original.execution_generation.clone(),
                            reason: String::new(),
                        },
                        "7"
                    ));
                }
                "preempt" => leases.preempt(InputPreemptionSource::LocalExternal),
                "expiry" => {
                    leases
                        .state
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .request
                        .expires_at = Utc::now() - Duration::seconds(1);
                }
                "epoch" => epoch = 1,
                _ => unreachable!(),
            }
            assert!(
                leases.require_active("generation-1", epoch).is_err(),
                "{case}"
            );
            for next in [original, request("generation-2")] {
                assert_eq!(
                    leases.acquire(next, epoch).unwrap_err().kind,
                    AgentErrorKind::HostAtCapacity,
                    "{case}"
                );
            }
            assert!(leases.release("generation-1"));
            leases.acquire(request("generation-2"), epoch).unwrap();
            assert!(!leases.release("generation-1"));
            assert!(leases.require_active("generation-2", epoch).is_ok());
        }
    }
}
