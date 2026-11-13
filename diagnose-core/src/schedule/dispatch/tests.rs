use super::*;
use desk_agent_protocol::AgentErrorKind;
use std::cell::RefCell;

const LEASE: LeaseIds = LeaseIds {
    run_epoch: 3,
    session_token: 9,
};

#[derive(Clone, Copy)]
enum Presence {
    Online,
    Offline,
    Unknown,
}

/// Scripted host recording each call; one instance serves one dispatch.
struct Fake {
    admits: bool,
    session: bool,
    presence: Presence,
    claim: bool,
    turn: RefCell<Option<TurnRun>>,
    writes: bool,
    settled: Option<FreshSettled>,
    continuation_error: Option<AgentError>,
    fresh_error: RefCell<Option<PersistedError>>,
    calls: RefCell<Vec<&'static str>>,
}

impl Fake {
    fn new(turn: TurnRun) -> Self {
        Self {
            admits: true,
            session: true,
            presence: Presence::Online,
            claim: true,
            turn: RefCell::new(Some(turn)),
            writes: true,
            settled: Some(FreshSettled::Other),
            continuation_error: None,
            fresh_error: RefCell::new(Some(PersistedError::Clean)),
            calls: RefCell::default(),
        }
    }
    fn call(&self, name: &'static str) {
        self.calls.borrow_mut().push(name);
    }
    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }
    fn presence(&self) -> DevicePresence<&'static str> {
        match self.presence {
            Presence::Online => DevicePresence::Online("route"),
            Presence::Offline => DevicePresence::Offline,
            Presence::Unknown => DevicePresence::Unknown,
        }
    }
}

fn error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::ModelUnavailable,
        message: "provider stopped".into(),
        retryable: true,
        safe_for_model: true,
        error_code: None,
    }
}

fn answered() -> TurnRun {
    TurnRun::Finished(Ok(LoopOutcome::Answered("done".into())))
}

fn candidate(status: &str) -> DispatchCandidate {
    DispatchCandidate {
        id: 7,
        owner: 1,
        schedule_id: "task".into(),
        run_id: "run".into(),
        conversation_id: "conversation".into(),
        status: status.into(),
        device_id: "device".into(),
    }
}

#[async_trait(?Send)]
impl ContinuationDispatchHost for Fake {
    type Target = &'static str;
    type Session = ();
    type Claimed = &'static str;
    fn admits(&self, _: &DispatchCandidate) -> bool {
        self.admits
    }
    async fn load_session(&self, _: &DispatchCandidate) -> Option<()> {
        self.call("load_session");
        self.session.then_some(())
    }
    async fn device_presence(&self, device_id: &str) -> DevicePresence<&'static str> {
        assert_eq!(device_id, "device");
        self.call("device_presence");
        self.presence()
    }
    async fn wait_for_device(&self, run_id: &str) {
        assert_eq!(run_id, "run");
        self.call("wait_for_device");
    }
    async fn claim_initial(
        &self,
        _: &DispatchCandidate,
        _: (),
        target: &'static str,
    ) -> Option<(LeaseIds, &'static str)> {
        assert_eq!(target, "route");
        self.call("claim_initial");
        self.claim.then_some((LEASE, "claimed"))
    }
    async fn claim_permission(&self, _: &DispatchCandidate) -> Option<(LeaseIds, &'static str)> {
        self.call("claim_permission");
        self.claim.then_some((LEASE, "claimed"))
    }
    async fn run_turn(&self, claimed: &'static str) -> TurnRun {
        assert_eq!(claimed, "claimed");
        self.call("run_turn");
        self.turn.borrow_mut().take().unwrap()
    }
    async fn finish_answered(&self, _: &DispatchCandidate, lease: LeaseIds, answer: &str) -> bool {
        assert_eq!((lease, answer), (LEASE, "done"));
        self.call("finish_answered");
        self.writes
    }
    async fn await_permission(
        &self,
        _: &DispatchCandidate,
        lease: LeaseIds,
        request_id: &str,
    ) -> bool {
        assert_eq!((lease, request_id), (LEASE, "request"));
        self.call("await_permission");
        self.writes
    }
    async fn persisted_error(&self, _: &DispatchCandidate) -> Option<AgentError> {
        self.call("persisted_error");
        self.continuation_error.clone()
    }
    async fn finish_failed(
        &self,
        _: &DispatchCandidate,
        lease: LeaseIds,
        error: &AgentError,
    ) -> bool {
        assert_eq!(lease, LEASE);
        assert_eq!(error.message, "provider stopped");
        self.call("finish_failed");
        self.writes
    }
}

#[async_trait(?Send)]
impl FreshDispatchHost for Fake {
    type Target = &'static str;
    type Claimed = &'static str;
    async fn device_presence(&self, device_id: &str) -> DevicePresence<&'static str> {
        assert_eq!(device_id, "device");
        self.call("device_presence");
        self.presence()
    }
    async fn wait_for_device(&self, _: &str) {
        self.call("wait_for_device");
    }
    async fn claim(
        &self,
        candidate: &DispatchCandidate,
        target: &'static str,
    ) -> Option<(LeaseIds, &'static str)> {
        assert_eq!((candidate.id, target), (7, "route"));
        self.call("claim");
        self.claim.then_some((LEASE, "claimed"))
    }
    async fn run_turn(&self, _: &'static str) -> TurnRun {
        self.call("run_turn");
        self.turn.borrow_mut().take().unwrap()
    }
    async fn finish_answered(
        &self,
        _: &DispatchCandidate,
        lease: LeaseIds,
        _: &str,
    ) -> Option<FreshSettled> {
        assert_eq!(lease, LEASE);
        self.call("finish_answered");
        self.settled
    }
    async fn await_children(&self, _: &DispatchCandidate, lease: LeaseIds, wait_id: &str) -> bool {
        assert_eq!((lease, wait_id), (LEASE, "children"));
        self.call("await_children");
        self.writes
    }
    async fn await_permission(&self, _: &DispatchCandidate, _: LeaseIds, _: &str) -> bool {
        self.call("await_permission");
        self.writes
    }
    async fn persisted_error(&self, _: &DispatchCandidate) -> PersistedError {
        self.call("persisted_error");
        self.fresh_error.borrow_mut().take().unwrap()
    }
    async fn finish_failed(
        &self,
        _: &DispatchCandidate,
        _: LeaseIds,
        _: &AgentError,
    ) -> Option<FreshSettled> {
        self.call("finish_failed");
        self.settled
    }
}

async fn continuation(host: &Fake, status: &str) -> DispatchResult {
    dispatch_continuation(host, &candidate(status)).await.result
}

async fn fresh(host: &Fake, status: &str) -> DispatchResult {
    dispatch_fresh(host, &candidate(status)).await.result
}

#[tokio::test]
async fn continuation_defers_before_any_claim() {
    let mut host = Fake::new(answered());
    host.admits = false;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Deferred
    );
    assert!(host.calls().is_empty());

    // A missing session is checked first and never records a device wait.
    let mut host = Fake::new(answered());
    host.session = false;
    host.presence = Presence::Offline;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Deferred
    );
    assert_eq!(host.calls(), ["load_session"]);

    let mut host = Fake::new(answered());
    host.presence = Presence::Offline;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Deferred
    );
    assert_eq!(
        host.calls(),
        ["load_session", "device_presence", "wait_for_device"]
    );

    let mut host = Fake::new(answered());
    host.presence = Presence::Unknown;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Deferred
    );
    assert_eq!(host.calls(), ["load_session", "device_presence"]);

    let mut host = Fake::new(answered());
    host.claim = false;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Deferred
    );
    assert_eq!(
        host.calls(),
        ["load_session", "device_presence", "claim_initial"]
    );
}

#[tokio::test]
async fn permission_resume_claims_without_a_device_check() {
    let mut host = Fake::new(answered());
    host.presence = Presence::Offline;
    assert_eq!(
        continuation(&host, "awaiting_permission").await,
        DispatchResult::Settled
    );
    assert_eq!(
        host.calls(),
        ["claim_permission", "run_turn", "finish_answered"]
    );
}

#[tokio::test]
async fn continuation_settles_by_turn_outcome() {
    let host = Fake::new(TurnRun::Unaccounted);
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Reconcile
    );

    let host = Fake::new(answered());
    assert_eq!(continuation(&host, "queued").await, DispatchResult::Settled);
    let mut host = Fake::new(answered());
    host.writes = false;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Reconcile
    );

    let requested = || {
        TurnRun::Finished(Ok(LoopOutcome::PermissionRequested {
            request_id: "request".into(),
        }))
    };
    let host = Fake::new(requested());
    assert_eq!(continuation(&host, "queued").await, DispatchResult::Waiting);
    let mut host = Fake::new(requested());
    host.writes = false;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Reconcile
    );

    // Other outcomes settle only through a persisted error.
    let host = Fake::new(TurnRun::Finished(Ok(LoopOutcome::Truncated)));
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Reconcile
    );
    assert!(!host.calls().contains(&"finish_failed"));
    let mut host = Fake::new(TurnRun::Finished(Err(error())));
    host.continuation_error = Some(error());
    let report = dispatch_continuation(&host, &candidate("queued")).await;
    assert_eq!(report.result, DispatchResult::Settled);
    assert_eq!(report.stopped, Some(error()));
    let mut host = Fake::new(TurnRun::Finished(Err(error())));
    host.continuation_error = Some(error());
    host.writes = false;
    assert_eq!(
        continuation(&host, "queued").await,
        DispatchResult::Reconcile
    );
}

#[tokio::test]
async fn fresh_defers_before_any_claim() {
    let mut host = Fake::new(answered());
    host.presence = Presence::Offline;
    assert_eq!(fresh(&host, "queued").await, DispatchResult::Deferred);
    assert_eq!(host.calls(), ["device_presence", "wait_for_device"]);
    // Started work waiting on a decision keeps its state while offline.
    for status in ["awaiting_permission", "awaiting_children"] {
        let mut host = Fake::new(answered());
        host.presence = Presence::Offline;
        assert_eq!(fresh(&host, status).await, DispatchResult::Deferred);
        assert_eq!(host.calls(), ["device_presence"]);
    }
    let mut host = Fake::new(answered());
    host.presence = Presence::Unknown;
    assert_eq!(fresh(&host, "queued").await, DispatchResult::Deferred);
    assert_eq!(host.calls(), ["device_presence"]);
    let mut host = Fake::new(answered());
    host.claim = false;
    assert_eq!(fresh(&host, "queued").await, DispatchResult::Deferred);
    assert_eq!(host.calls(), ["device_presence", "claim"]);
}

#[tokio::test]
async fn fresh_settlement_maps_the_status_left_behind() {
    let host = Fake::new(TurnRun::Unaccounted);
    assert_eq!(fresh(&host, "queued").await, DispatchResult::Reconcile);
    for (settled, expected) in [
        (
            Some(FreshSettled::AwaitingChildren),
            DispatchResult::Waiting,
        ),
        (
            Some(FreshSettled::OutcomeUnknown),
            DispatchResult::Reconcile,
        ),
        (Some(FreshSettled::Other), DispatchResult::Settled),
        (None, DispatchResult::Reconcile),
    ] {
        let mut host = Fake::new(answered());
        host.settled = settled;
        assert_eq!(fresh(&host, "queued").await, expected);
        let mut host = Fake::new(TurnRun::Finished(Err(error())));
        *host.fresh_error.borrow_mut() = Some(PersistedError::Failed(error()));
        host.settled = settled;
        assert_eq!(fresh(&host, "queued").await, expected);
        assert_eq!(
            host.calls(),
            [
                "device_presence",
                "claim",
                "run_turn",
                "persisted_error",
                "finish_failed"
            ]
        );
    }
}

#[tokio::test]
async fn fresh_waits_and_unproven_errors() {
    for (writes, expected) in [
        (true, DispatchResult::Waiting),
        (false, DispatchResult::Reconcile),
    ] {
        let mut host = Fake::new(TurnRun::Finished(Ok(LoopOutcome::SubAgentsWaiting {
            wait_id: "children".into(),
        })));
        host.writes = writes;
        assert_eq!(fresh(&host, "queued").await, expected);
        let mut host = Fake::new(TurnRun::Finished(Ok(LoopOutcome::PermissionRequested {
            request_id: "request".into(),
        })));
        host.writes = writes;
        assert_eq!(fresh(&host, "queued").await, expected);
    }
    let host = Fake::new(TurnRun::Finished(Ok(LoopOutcome::Truncated)));
    *host.fresh_error.borrow_mut() = Some(PersistedError::Unreadable);
    assert_eq!(fresh(&host, "queued").await, DispatchResult::Deferred);
    let host = Fake::new(TurnRun::Finished(Ok(LoopOutcome::Truncated)));
    assert_eq!(fresh(&host, "queued").await, DispatchResult::Reconcile);
    assert!(!host.calls().contains(&"finish_failed"));
}
