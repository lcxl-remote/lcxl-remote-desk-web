//! Locally authorized, bounded Linux input control; never an isolation proof.
use super::{ComputerUseBroker, WriterLeaseRequest};
use desk_agent_protocol::computer_turn::{ComputerActionTurnQuery, ComputerActionTurnState};
use desk_agent_protocol::computer_use::{
    ComputerActionCancel, ComputerActionKind, ComputerActionTurnScope, SealedComputerActionPlan,
};
use desk_input_injection::linux_input_block::{BlockReport, InputBlock};
use desk_ipc_protocol::message::{ComputerTurnQueryPayload, ComputerTurnStatusPayload};
use std::{
    io,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

/// Native UI receipt. Partial coverage must be displayed as partial coverage.
#[derive(Clone, Copy, Debug)]
pub struct LinuxInputControlReceipt {
    pub generation: u64,
    pub report: BlockReport,
}

pub(super) trait Guard {
    fn active(&self) -> bool;
}
impl Guard for InputBlock {
    fn active(&self) -> bool {
        self.is_active()
    }
}

pub(super) struct Control<G = InputBlock> {
    next: u64,
    period: Option<Period<G>>,
    changed: std::sync::Arc<tokio::sync::Notify>,
}
struct Period<G> {
    generation: u64,
    worker: u64,
    guard: G,
    action: Option<WriterLeaseRequest>,
    owner: Option<TurnOwner>,
    probe: Option<(ComputerTurnQueryPayload, Instant)>,
    next_probe: Instant,
}

/// Immutable for a locally accepted period, including gaps between actions.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TurnOwner {
    authority: String,
    actor: String,
    scope: ComputerActionTurnScope,
}
impl<G> Default for Control<G> {
    fn default() -> Self {
        Self::with_changes(std::sync::Arc::default())
    }
}
impl<G> Control<G> {
    pub(super) fn with_changes(changed: std::sync::Arc<tokio::sync::Notify>) -> Self {
        Self {
            next: 0,
            period: None,
            changed,
        }
    }
}
impl<G: Guard> Control<G> {
    fn revoke(&mut self) -> bool {
        let Some(period) = self.period.take() else {
            return false;
        };
        drop(period);
        self.changed.notify_one();
        true
    }

    fn current(&self, worker: u64) -> Option<u64> {
        self.period
            .as_ref()
            .filter(|p| p.worker == worker && p.guard.active())
            .map(|p| p.generation)
    }

    fn install(&mut self, worker: u64, guard: G) -> io::Result<u64> {
        if self.period.as_ref().is_some_and(|p| p.guard.active()) {
            return Err(io::Error::other(
                "An input control period is already active",
            ));
        }
        if !guard.active() {
            return Err(io::Error::other("Input blocking ended before admission"));
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Input control generation exhausted"))?;
        self.period = Some(Period {
            generation: self.next,
            worker,
            guard,
            action: None,
            owner: None,
            probe: None,
            next_probe: Instant::now(),
        });
        self.changed.notify_one();
        Ok(self.next)
    }

    fn probe(&mut self, worker: u64, now: Instant) -> Option<ComputerTurnQueryPayload> {
        let period = self.period.as_mut()?;
        if period.worker != worker
            || !period.guard.active()
            || now < period.next_probe
            || period
                .probe
                .as_ref()
                .is_some_and(|(_, sent)| now.duration_since(*sent) < Duration::from_secs(5))
        {
            return None;
        }
        let owner = period.owner.as_ref()?;
        let request = ComputerTurnQueryPayload {
            request_id: uuid::Uuid::new_v4().to_string(),
            authority: owner.authority.clone(),
            control_generation: period.generation,
            query: ComputerActionTurnQuery {
                actor_id: owner.actor.clone(),
                scope: owner.scope.clone(),
            },
        };
        period.probe = Some((request.clone(), now));
        period.next_probe = now + Duration::from_secs(1);
        Some(request)
    }

    fn apply_status(
        &mut self,
        worker: u64,
        reply: &ComputerTurnStatusPayload,
        now: Instant,
    ) -> bool {
        let Some(period) = self.period.as_mut() else {
            return false;
        };
        if period.worker != worker
            || !period.guard.active()
            || !period.probe.as_ref().is_some_and(|(request, sent)| {
                request == &reply.request && now.duration_since(*sent) < Duration::from_secs(5)
            })
        {
            return false;
        }
        period.probe = None;
        if reply.state == ComputerActionTurnState::Revoked {
            return self.revoke();
        }
        // Current/Unavailable can neither extend the guard nor reopen a period.
        false
    }

    fn bind_action(&mut self, owner: TurnOwner, action: WriterLeaseRequest) -> bool {
        let Some(period) = self.period.as_mut() else {
            return false;
        };
        if !period.guard.active()
            || period.owner.as_ref().is_some_and(|bound| bound != &owner)
            || owner.actor != action.approved_actor_id
        {
            return false;
        }
        period.owner = Some(owner);
        period.action = Some(action);
        true
    }

    fn clear_action(&mut self, execution: &str) {
        if let Some(period) = self.period.as_mut()
            && period
                .action
                .as_ref()
                .is_some_and(|a| a.execution_generation == execution)
        {
            period.action = None;
        }
    }
    fn cancel_action(&mut self, cancel: &ComputerActionCancel, actor: &str) -> bool {
        let matches = self
            .period
            .as_ref()
            .and_then(|p| p.action.as_ref())
            .is_some_and(|a| {
                a.execution_generation == cancel.execution_generation
                    && a.work_id == cancel.work_id
                    && a.action_request_id == cancel.action_request_id
                    && a.approved_actor_id == actor
            });
        if matches {
            self.revoke();
        }
        matches
    }

    fn end(&mut self, generation: u64) -> bool {
        if self
            .period
            .as_ref()
            .is_some_and(|p| p.generation == generation)
        {
            self.revoke()
        } else {
            false
        }
    }
}

impl ComputerUseBroker {
    pub(crate) fn linux_input_turn_probe(&self) -> Option<ComputerTurnQueryPayload> {
        self.linux_input_control.lock().ok()?.probe(
            self.worker_generation.load(Ordering::SeqCst),
            Instant::now(),
        )
    }

    pub(crate) fn apply_linux_input_turn_status(&self, reply: &ComputerTurnStatusPayload) -> bool {
        self.linux_input_control.lock().is_ok_and(|mut control| {
            control.apply_status(
                self.worker_generation.load(Ordering::SeqCst),
                reply,
                Instant::now(),
            )
        })
    }
    /// Native local-consent entry only: do not expose this method as a remote
    /// tool or derive consent from an owner login. Call on a blocking thread.
    /// The caller must explain the duration, Ctrl+Alt+L escape and partial
    /// coverage policy before calling. Device permissions are never changed.
    pub fn begin_linux_input_control(
        &self,
        duration: Duration,
        accept_partial: bool,
    ) -> io::Result<LinuxInputControlReceipt> {
        // Hold the same identity fence as reset/revocation while acquiring and
        // installing. The backend callback must never acquire broker locks.
        let identity = self
            .linux_desktop_identity
            .lock()
            .map_err(|_| io::Error::other("Desktop identity is unavailable"))?;
        if identity.is_none() {
            return Err(io::Error::other(
                "An unlocked trusted Wayland desktop is required",
            ));
        }
        let mut control = self
            .linux_input_control
            .lock()
            .map_err(|_| io::Error::other("Input control is unavailable"))?;
        if control.period.as_ref().is_some_and(|p| p.guard.active()) {
            return Err(io::Error::other(
                "An input control period is already active",
            ));
        }
        // The backend thread may notify after expiry or local escape without
        // reentering broker locks; Notify retains a permit during collection.
        let changed = control.changed.clone();
        let guard = InputBlock::acquire(duration, move |_| changed.notify_one())?;
        let report = guard.report();
        if report.failed != 0 && !accept_partial {
            return Err(io::Error::other(format!(
                "Partial input blocking was not accepted: {} acquired, {} failed",
                report.grabbed, report.failed
            )));
        }
        let generation = control.install(self.worker_generation.load(Ordering::SeqCst), guard)?;
        Ok(LinuxInputControlReceipt { generation, report })
    }

    /// An old local UI's completion cannot release a newer control period.
    pub fn end_linux_input_control(&self, generation: u64) -> bool {
        self.linux_input_control
            .lock()
            .is_ok_and(|mut c| c.end(generation))
    }

    pub(crate) fn linux_input_control_generation(&self) -> Option<u64> {
        self.linux_input_control
            .lock()
            .ok()?
            .current(self.worker_generation.load(Ordering::SeqCst))
    }

    pub(crate) fn require_linux_input_control(
        &self,
        generation: Option<u64>,
    ) -> Result<(), desk_agent_protocol::AgentError> {
        if generation.is_none() || self.linux_input_control_generation() != generation {
            return Err(super::error(
                desk_agent_protocol::AgentErrorKind::Cancelled,
                "The locally authorized best-effort input control period is no longer active",
                false,
            ));
        }
        Ok(())
    }

    /// Bind only native desktop writes, after admission. Browser/file writers
    /// never own the local device guard. The control lock fences cancellation
    /// against the final writer-status check and assignment.
    pub(crate) fn bind_linux_input_action(
        &self,
        generation: Option<u64>,
        plan: &SealedComputerActionPlan,
        authority: Option<&str>,
    ) -> Result<(), desk_agent_protocol::AgentError> {
        if !plan.actions.iter().any(|step| {
            matches!(
                step.action,
                ComputerActionKind::UiInApplication { .. }
                    | ComputerActionKind::WaylandOutputInput(_)
            )
        }) {
            return Ok(());
        }
        let unavailable = || {
            super::error(
                desk_agent_protocol::AgentErrorKind::Cancelled,
                "Desktop input control or its admitted writer is no longer current",
                false,
            )
        };
        let authority = authority
            .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(unavailable)?;
        let scope = plan.turn_scope.as_ref().ok_or_else(unavailable)?;
        scope.validate().map_err(|_| unavailable())?;
        let mut control = self.linux_input_control.lock().map_err(|_| unavailable())?;
        if generation.is_none()
            || control.current(self.worker_generation.load(Ordering::SeqCst)) != generation
        {
            return Err(unavailable());
        }
        let writer = self.require_writer_lease(&plan.execution_generation)?;
        if writer.request.work_id != plan.work_id
            || writer.request.action_request_id != plan.action_request_id
            || writer.request.approved_actor_id != plan.approved_actor_id
        {
            return Err(unavailable());
        }
        if !control.bind_action(
            TurnOwner {
                authority: authority.into(),
                actor: plan.approved_actor_id.clone(),
                scope: scope.clone(),
            },
            writer.request,
        ) {
            return Err(unavailable());
        }
        Ok(())
    }

    pub(super) fn cancel_linux_input_action(&self, cancel: &ComputerActionCancel, actor: &str) {
        if let Ok(mut control) = self.linux_input_control.lock() {
            control.cancel_action(cancel, actor);
        }
    }
    pub(super) fn clear_linux_input_action(&self, execution: &str) {
        if let Ok(mut control) = self.linux_input_control.lock() {
            control.clear_action(execution);
        }
    }

    pub(super) fn revoke_linux_input_control(&self) {
        // No callback reenters the broker, so joining release under this lock
        // cannot wait for an identity/ownership lock held by this caller.
        if let Ok(mut control) = self.linux_input_control.lock() {
            control.revoke();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    };
    struct Fake {
        active: Arc<AtomicBool>,
        released: Arc<AtomicUsize>,
    }
    impl Guard for Fake {
        fn active(&self) -> bool {
            self.active.load(Ordering::SeqCst)
        }
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            self.released.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn fake() -> (Fake, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let active = Arc::new(AtomicBool::new(true));
        let released = Arc::new(AtomicUsize::new(0));
        (
            Fake {
                active: active.clone(),
                released: released.clone(),
            },
            active,
            released,
        )
    }

    fn action(execution: &str) -> WriterLeaseRequest {
        WriterLeaseRequest {
            scope: super::super::WriterLeaseScope::InteractiveSession,
            work_id: "work".into(),
            action_request_id: "action".into(),
            execution_generation: execution.into(),
            approved_actor_id: "owner".into(),
            interactive_session_incarnation: "session".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(60),
        }
    }
    fn cancel(execution: &str) -> ComputerActionCancel {
        ComputerActionCancel {
            work_id: "work".into(),
            action_request_id: "action".into(),
            execution_generation: execution.into(),
            reason: "cancelled".into(),
        }
    }
    fn owner() -> TurnOwner {
        TurnOwner {
            authority: "a".repeat(64),
            actor: "owner".into(),
            scope: ComputerActionTurnScope {
                conversation_id: "conversation".into(),
                turn_id: "turn".into(),
                input_revision: 1,
                lease_token: 2,
            },
        }
    }
    #[test]
    fn local_start_stop_and_cancellation_wake_the_shared_readiness_collector() {
        let broker = ComputerUseBroker::new();
        let mut control = Control::with_changes(broker.readiness_changed.clone());
        let (guard, _, _) = fake();
        let first = control.install(7, guard).unwrap();
        // The event occurred while the collector was not waiting.
        assert!(broker.wait_for_readiness_change().now_or_never().is_some());
        assert!(control.bind_action(owner(), action("execution")));
        control.clear_action("execution");
        assert!(broker.wait_for_readiness_change().now_or_never().is_none());
        assert!(!control.end(first + 1));
        assert!(broker.wait_for_readiness_change().now_or_never().is_none());
        assert!(control.bind_action(owner(), action("next")));
        assert!(control.cancel_action(&cancel("next"), "owner"));
        assert!(broker.wait_for_readiness_change().now_or_never().is_some());
        let (guard, _, _) = fake();
        let second = control.install(7, guard).unwrap();
        assert!(control.end(second));
        // Coalesced start/stop still leaves a refresh pending.
        assert!(broker.wait_for_readiness_change().now_or_never().is_some());
        assert!(broker.wait_for_readiness_change().now_or_never().is_none());
    }

    #[test]
    fn turn_revoke_and_worker_reset_wake_readiness_but_current_does_not() {
        let broker = ComputerUseBroker::new();
        let mut control = Control::with_changes(broker.readiness_changed.clone());
        let (guard, _, _) = fake();
        control.install(7, guard).unwrap();
        assert!(control.bind_action(owner(), action("execution")));
        assert!(broker.wait_for_readiness_change().now_or_never().is_some());
        let now = Instant::now();
        let first = control.probe(7, now).unwrap();
        assert!(!control.apply_status(
            7,
            &ComputerTurnStatusPayload {
                request: first,
                state: ComputerActionTurnState::Current
            },
            now
        ));
        assert!(broker.wait_for_readiness_change().now_or_never().is_none());
        let later = now + Duration::from_secs(1);
        let request = control.probe(7, later).unwrap();
        assert!(control.apply_status(
            7,
            &ComputerTurnStatusPayload {
                request,
                state: ComputerActionTurnState::Revoked
            },
            later
        ));
        assert!(broker.wait_for_readiness_change().now_or_never().is_some());
        let (guard, _, _) = fake();
        control.install(7, guard).unwrap();
        assert!(broker.wait_for_readiness_change().now_or_never().is_some());
        assert!(control.revoke());
        assert!(broker.wait_for_readiness_change().now_or_never().is_some());
        assert!(!control.revoke());
        assert!(broker.wait_for_readiness_change().now_or_never().is_none());
    }
    #[test]
    fn only_the_current_probe_can_release_its_local_period() {
        let mut control = Control::default();
        let (guard, _, released) = fake();
        let id = control.install(7, guard).unwrap();
        let now = Instant::now();
        assert!(control.probe(7, now).is_none());
        assert!(control.bind_action(owner(), action("execution")));
        let first = control.probe(7, now).unwrap();
        assert!(control.probe(7, now).is_none());
        let reply = |request, state| ComputerTurnStatusPayload { request, state };
        assert!(!control.apply_status(
            7,
            &reply(first.clone(), ComputerActionTurnState::Current),
            now
        ));
        assert_eq!(control.current(7), Some(id));
        let next_time = now + Duration::from_secs(1);
        let second = control.probe(7, next_time).unwrap();
        assert_ne!(first.request_id, second.request_id);
        assert!(!control.apply_status(
            7,
            &reply(first, ComputerActionTurnState::Revoked),
            next_time
        ));
        for field in 0..4 {
            let mut wrong = second.clone();
            match field {
                0 => wrong.authority = "b".repeat(64),
                1 => wrong.control_generation += 1,
                2 => wrong.query.actor_id = "other".into(),
                _ => wrong.query.scope.turn_id = "other".into(),
            }
            assert!(!control.apply_status(
                7,
                &reply(wrong, ComputerActionTurnState::Revoked),
                next_time
            ));
        }
        let ended = reply(second, ComputerActionTurnState::Revoked);
        assert!(control.apply_status(7, &ended, next_time));
        assert_eq!(released.load(Ordering::SeqCst), 1);
        let (next_guard, _, next_released) = fake();
        control.install(7, next_guard).unwrap();
        assert!(!control.apply_status(7, &ended, next_time));
        assert_eq!(next_released.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn expired_or_wrong_worker_replies_do_not_release_and_unavailable_never_renews() {
        let mut control = Control::default();
        let (guard, active, released) = fake();
        let id = control.install(7, guard).unwrap();
        assert!(control.bind_action(owner(), action("execution")));
        let now = Instant::now();
        let first = control.probe(7, now).unwrap();
        let expired = now + Duration::from_secs(5);
        assert!(!control.apply_status(
            7,
            &ComputerTurnStatusPayload {
                request: first,
                state: ComputerActionTurnState::Revoked
            },
            expired
        ));
        let next = control.probe(7, expired).unwrap();
        assert!(control.probe(8, expired).is_none());
        assert!(!control.apply_status(
            8,
            &ComputerTurnStatusPayload {
                request: next.clone(),
                state: ComputerActionTurnState::Revoked
            },
            expired
        ));
        assert!(!control.apply_status(
            7,
            &ComputerTurnStatusPayload {
                request: next,
                state: ComputerActionTurnState::Unavailable
            },
            expired
        ));
        assert_eq!(control.current(7), Some(id));
        active.store(false, Ordering::SeqCst);
        assert!(control.probe(7, expired + Duration::from_secs(1)).is_none());
        assert_eq!(control.current(7), None);
        control.end(id);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn a_period_cannot_be_borrowed_by_another_turn_or_upstream_between_actions() {
        let mut control = Control::default();
        let (guard, _, released) = fake();
        let id = control.install(7, guard).unwrap();
        assert!(control.bind_action(owner(), action("first")));
        control.clear_action("first");
        for field in 0..6 {
            let mut other = owner();
            match field {
                0 => other.authority = "b".repeat(64),
                1 => other.actor = "other".into(),
                2 => other.scope.conversation_id.push_str("-other"),
                3 => other.scope.turn_id.push_str("-other"),
                4 => other.scope.input_revision += 1,
                _ => other.scope.lease_token += 1,
            }
            let mut writer = action("other");
            writer.approved_actor_id = other.actor.clone();
            assert!(!control.bind_action(other, writer));
            assert_eq!(control.current(7), Some(id));
            assert!(control.period.as_ref().unwrap().action.is_none());
        }
        assert!(control.bind_action(owner(), action("second")));
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert!(control.end(id));
        assert_eq!(released.load(Ordering::SeqCst), 1);
        let (next, _, _) = fake();
        control.install(7, next).unwrap();
        let mut other = owner();
        other.scope.turn_id = "new-turn".into();
        assert!(control.bind_action(other, action("third")));
    }

    #[test]
    fn expired_guard_or_different_writer_actor_cannot_claim_a_period() {
        let mut control = Control::default();
        let (guard, active, _) = fake();
        control.install(7, guard).unwrap();
        let mut writer = action("first");
        writer.approved_actor_id = "other".into();
        assert!(!control.bind_action(owner(), writer));
        assert!(control.period.as_ref().unwrap().owner.is_none());
        active.store(false, Ordering::SeqCst);
        assert!(!control.bind_action(owner(), action("first")));
    }

    #[test]
    fn cancellation_requires_the_entire_bound_action_and_actor() {
        let mut control = Control::default();
        let (guard, _, released) = fake();
        let id = control.install(7, guard).unwrap();
        control.period.as_mut().unwrap().action = Some(action("execution"));
        let mut wrong_work = cancel("execution");
        wrong_work.work_id = "other".into();
        let mut wrong_action = cancel("execution");
        wrong_action.action_request_id = "other".into();
        for request in [wrong_work, wrong_action, cancel("old-execution")] {
            assert!(!control.cancel_action(&request, "owner"));
        }
        assert!(!control.cancel_action(&cancel("execution"), "another-actor"));
        assert_eq!(control.current(7), Some(id));
        assert!(control.cancel_action(&cancel("execution"), "owner"));
        assert_eq!(control.current(7), None);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn completion_clears_only_its_action_and_does_not_end_the_operation_period() {
        let mut control = Control::default();
        let (guard, _, released) = fake();
        let id = control.install(7, guard).unwrap();
        control.period.as_mut().unwrap().action = Some(action("new-execution"));
        control.clear_action("old-execution");
        assert!(control.period.as_ref().unwrap().action.is_some());
        assert!(!control.cancel_action(&cancel("old-execution"), "owner"));
        control.clear_action("new-execution");
        assert!(!control.cancel_action(&cancel("new-execution"), "owner"));
        assert_eq!(control.current(7), Some(id));
        assert_eq!(released.load(Ordering::SeqCst), 0);
        control.end(id);
        let (next, _, _) = fake();
        let next_id = control.install(7, next).unwrap();
        assert!(!control.cancel_action(&cancel("new-execution"), "owner"));
        assert_eq!(control.current(7), Some(next_id));
    }

    #[test]
    fn legacy_monitor_flag_cannot_admit_linux_desktop_input() {
        let broker = ComputerUseBroker::new();
        broker.set_input_ownership_ready(true);
        assert!(!broker.input_ownership_is_ready());
        assert!(broker.require_linux_input_control(None).is_err());
        assert!(broker.require_linux_input_control(Some(1)).is_err());
        // Reject before touching devices when there is no trusted desktop.
        assert!(
            broker
                .begin_linux_input_control(Duration::from_secs(1), true)
                .is_err()
        );
        assert!(!broker.end_linux_input_control(1));
    }

    #[test]
    fn escape_or_expiry_revokes_without_waiting_for_a_callback() {
        let mut c = Control::default();
        let (guard, active, released) = fake();
        let id = c.install(7, guard).unwrap();
        assert_eq!(c.current(7), Some(id));
        assert_eq!(c.current(8), None);
        active.store(false, Ordering::SeqCst);
        assert_eq!(c.current(7), None);
        assert!(c.end(id));
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn old_completion_cannot_release_replacement_and_drop_releases_current() {
        let mut c = Control::default();
        let (first, active, first_released) = fake();
        let first_id = c.install(7, first).unwrap();
        active.store(false, Ordering::SeqCst);
        let (second, _, second_released) = fake();
        let second_id = c.install(7, second).unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(first_released.load(Ordering::SeqCst), 1);
        assert!(!c.end(first_id));
        assert_eq!(c.current(7), Some(second_id));
        drop(c);
        assert_eq!(second_released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn overlapping_or_already_ended_admission_releases_rejected_guard() {
        let mut c = Control::default();
        let (first, _, _) = fake();
        let first_id = c.install(7, first).unwrap();
        let (second, _, released) = fake();
        assert!(c.install(7, second).is_err());
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert_eq!(c.current(7), Some(first_id));
        c.end(first_id);
        let (third, active, released) = fake();
        active.store(false, Ordering::SeqCst);
        assert!(c.install(7, third).is_err());
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert_eq!(c.current(7), None);
    }
}
