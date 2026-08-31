//! Daemon-owned volatile binding between one approved PTY execution and the
//! exact upstream WebSocket that prepared it.
//!
//! Nothing here is durable. Losing the link removes its entries and the caller
//! cancels their workers; reconnecting cannot recover, replay, or rebind input.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use desk_agent_protocol::exec_pty::{
    PtyCloseReason, PtyOutputFrame, PtyStreamClosed, PtyStreamOpened,
};
use desk_agent_protocol::exec_pty_wire::{self, PtyWireFrame};
use desk_ipc_protocol::message::{ExecPtyStartPayload, ServiceToWorker, WorkerKey};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::daemon::worker_manager::WorkerIncarnation;

pub const CARRIER_OUTPUT_QUEUE_CAP: usize = 16;

#[derive(Clone)]
pub struct ExecPtyLinkContext {
    pub registry: ExecPtyCarrierRegistry,
    pub link_id: PtyCarrierLinkId,
    pub outbound: mpsc::Sender<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PtyCarrierLinkId(Uuid);

impl PtyCarrierLinkId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PtyCarrierLinkId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Default)]
pub struct ExecPtyCarrierRegistry {
    inner: Arc<Mutex<HashMap<String, CarrierEntry>>>,
}

struct CarrierEntry {
    link_id: PtyCarrierLinkId,
    task_id: String,
    execution_generation: String,
    session_target_id: String,
    registration_generation: u64,
    wire_worker_incarnation: u64,
    destination: CarrierDestination,
    outbound: mpsc::Sender<Vec<u8>>,
    opened: bool,
    next_input_sequence: u64,
    next_output_sequence: u64,
}

#[derive(Clone)]
enum CarrierDestination {
    Worker {
        worker_key: Option<WorkerKey>,
        worker_incarnation: WorkerIncarnation,
    },
    Daemon {
        control: mpsc::Sender<PtyWireFrame>,
    },
}

pub enum CarrierDispatch {
    Worker {
        worker_key: Option<WorkerKey>,
        command: ServiceToWorker,
    },
    Daemon {
        control: mpsc::Sender<PtyWireFrame>,
        frame: PtyWireFrame,
    },
}

#[derive(Clone)]
pub struct CarrierCancellation {
    pub stream_id: String,
    pub execution_generation: String,
    pub session_target_id: String,
    pub registration_generation: u64,
    pub worker_incarnation: u64,
    destination: CarrierDestination,
    pub reason: PtyCloseReason,
}

impl CarrierCancellation {
    pub fn into_dispatch(self) -> CarrierDispatch {
        let frame = desk_agent_protocol::exec_pty::PtyCancelFrame {
            stream_id: self.stream_id,
            execution_generation: self.execution_generation,
            session_target_id: self.session_target_id,
            registration_generation: self.registration_generation,
            worker_incarnation: self.worker_incarnation,
            reason: self.reason,
        };
        match self.destination {
            CarrierDestination::Worker { worker_key, .. } => CarrierDispatch::Worker {
                worker_key,
                command: ServiceToWorker::ExecPtyCancel(frame),
            },
            CarrierDestination::Daemon { control } => CarrierDispatch::Daemon {
                control,
                frame: PtyWireFrame::Cancel(frame),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarrierError {
    DuplicateStream,
    MissingStream,
    WrongLink,
    StaleWorker,
    StaleBinding,
    SequenceViolation,
    InvalidDirection,
    EncodeFailed,
    SlowConsumer,
    LinkClosed,
}

impl CarrierError {
    pub fn close_reason(&self) -> PtyCloseReason {
        match self {
            Self::SequenceViolation => PtyCloseReason::SequenceViolation,
            Self::StaleWorker | Self::StaleBinding | Self::WrongLink | Self::MissingStream => {
                PtyCloseReason::SessionStale
            }
            Self::SlowConsumer => PtyCloseReason::SlowConsumer,
            Self::LinkClosed => PtyCloseReason::CarrierDisconnected,
            Self::DuplicateStream | Self::InvalidDirection | Self::EncodeFailed => {
                PtyCloseReason::InternalError
            }
        }
    }
}

impl std::fmt::Display for CarrierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl ExecPtyCarrierRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(
        &self,
        link_id: PtyCarrierLinkId,
        start: &ExecPtyStartPayload,
        worker_key: Option<WorkerKey>,
        worker_incarnation: WorkerIncarnation,
        outbound: mpsc::Sender<Vec<u8>>,
    ) -> Result<(), CarrierError> {
        if start.stream_id.is_empty()
            || start.plan.execution_generation != start.request_id
            || start.registration_generation
                != worker_key
                    .as_ref()
                    .map_or(0, |key| key.session.session_generation)
            || start.worker_incarnation
                != worker_key.as_ref().map_or(0, |_| worker_incarnation.get())
            || worker_key
                .as_ref()
                .is_some_and(|key| key.session.platform_session_id != start.session_target_id)
        {
            return Err(CarrierError::StaleBinding);
        }
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.contains_key(&start.stream_id) {
            return Err(CarrierError::DuplicateStream);
        }
        inner.insert(
            start.stream_id.clone(),
            CarrierEntry {
                link_id,
                task_id: start.plan.exec_request_id.0.clone(),
                execution_generation: start.plan.execution_generation.clone(),
                session_target_id: start.session_target_id.clone(),
                registration_generation: start.registration_generation,
                wire_worker_incarnation: start.worker_incarnation,
                destination: CarrierDestination::Worker {
                    worker_key,
                    worker_incarnation,
                },
                outbound,
                opened: false,
                next_input_sequence: 0,
                next_output_sequence: 0,
            },
        );
        Ok(())
    }

    pub fn bind_daemon(
        &self,
        link_id: PtyCarrierLinkId,
        start: &ExecPtyStartPayload,
        control: mpsc::Sender<PtyWireFrame>,
        outbound: mpsc::Sender<Vec<u8>>,
    ) -> Result<(), CarrierError> {
        if start.stream_id.is_empty()
            || start.plan.execution_generation != start.request_id
            || start.session_target_id.is_empty()
            || start.registration_generation == 0
            || start.worker_incarnation == 0
        {
            return Err(CarrierError::StaleBinding);
        }
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.contains_key(&start.stream_id) {
            return Err(CarrierError::DuplicateStream);
        }
        inner.insert(
            start.stream_id.clone(),
            CarrierEntry {
                link_id,
                task_id: start.plan.exec_request_id.0.clone(),
                execution_generation: start.plan.execution_generation.clone(),
                session_target_id: start.session_target_id.clone(),
                registration_generation: start.registration_generation,
                wire_worker_incarnation: start.worker_incarnation,
                destination: CarrierDestination::Daemon { control },
                outbound,
                opened: false,
                next_input_sequence: 0,
                next_output_sequence: 0,
            },
        );
        Ok(())
    }

    pub fn route_worker_opened(
        &self,
        worker_key: Option<&WorkerKey>,
        worker_incarnation: WorkerIncarnation,
        opened: PtyStreamOpened,
    ) -> Result<(), CarrierError> {
        let stream_id = opened.stream_id.clone();
        self.route_worker_frame(
            worker_key,
            worker_incarnation,
            &stream_id,
            PtyWireFrame::Opened(opened),
        )
    }

    pub fn route_worker_output(
        &self,
        worker_key: Option<&WorkerKey>,
        worker_incarnation: WorkerIncarnation,
        output: PtyOutputFrame,
    ) -> Result<(), CarrierError> {
        let stream_id = output.stream_id.clone();
        self.route_worker_frame(
            worker_key,
            worker_incarnation,
            &stream_id,
            PtyWireFrame::Output(output),
        )
    }

    pub fn route_worker_closed(
        &self,
        worker_key: Option<&WorkerKey>,
        worker_incarnation: WorkerIncarnation,
        closed: PtyStreamClosed,
    ) -> Result<(), CarrierError> {
        let stream_id = closed.stream_id.clone();
        let result = self.route_worker_frame(
            worker_key,
            worker_incarnation,
            &stream_id,
            PtyWireFrame::Closed(closed),
        );
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&stream_id);
        result
    }

    fn route_worker_frame(
        &self,
        worker_key: Option<&WorkerKey>,
        worker_incarnation: WorkerIncarnation,
        stream_id: &str,
        frame: PtyWireFrame,
    ) -> Result<(), CarrierError> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entry = inner
            .get_mut(stream_id)
            .ok_or(CarrierError::MissingStream)?;
        validate_worker(entry, worker_key, worker_incarnation)?;
        route_output_frame(entry, frame)
    }

    /// Atomically prove that the daemon-bound carrier is still live, reserve
    /// room for `Opened`, and cross the caller's irreversible start barrier.
    /// Holding the registry lock across the tiny synchronous `start` closure
    /// makes link removal order unambiguous: either removal wins and `start` is
    /// never called, or start wins and removal immediately delivers a cancel.
    pub fn start_daemon<T, E>(
        &self,
        opened: PtyStreamOpened,
        start: impl FnOnce() -> Result<T, E>,
    ) -> Result<Result<T, E>, CarrierError> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entry = inner
            .get_mut(&opened.stream_id)
            .ok_or(CarrierError::MissingStream)?;
        if !matches!(entry.destination, CarrierDestination::Daemon { .. })
            || entry.opened
            || entry.task_id != opened.task_id
            || !binding_matches_opened(entry, &opened)
        {
            return Err(CarrierError::StaleBinding);
        }
        let encoded = exec_pty_wire::encode(&PtyWireFrame::Opened(opened))
            .map_err(|_| CarrierError::EncodeFailed)?;
        let outbound = entry.outbound.clone();
        let permit = outbound.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => CarrierError::SlowConsumer,
            mpsc::error::TrySendError::Closed(()) => CarrierError::LinkClosed,
        })?;
        let started = start();
        if started.is_ok() {
            entry.opened = true;
            permit.send(encoded);
        }
        Ok(started)
    }

    pub fn route_daemon_output(&self, output: PtyOutputFrame) -> Result<(), CarrierError> {
        let stream_id = output.stream_id.clone();
        self.route_daemon_frame(&stream_id, PtyWireFrame::Output(output))
    }

    pub fn route_daemon_closed(&self, closed: PtyStreamClosed) -> Result<(), CarrierError> {
        let stream_id = closed.stream_id.clone();
        let result = self.route_daemon_frame(&stream_id, PtyWireFrame::Closed(closed));
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&stream_id);
        result
    }

    fn route_daemon_frame(&self, stream_id: &str, frame: PtyWireFrame) -> Result<(), CarrierError> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entry = inner
            .get_mut(stream_id)
            .ok_or(CarrierError::MissingStream)?;
        if !matches!(entry.destination, CarrierDestination::Daemon { .. }) {
            return Err(CarrierError::StaleWorker);
        }
        route_output_frame(entry, frame)
    }

    pub fn accept_upstream_frame(
        &self,
        link_id: PtyCarrierLinkId,
        frame: PtyWireFrame,
    ) -> Result<CarrierDispatch, CarrierError> {
        let (stream_id, generation, binding, sequence) = match &frame {
            PtyWireFrame::Input(input) => (
                &input.stream_id,
                &input.execution_generation,
                (
                    &input.session_target_id,
                    input.registration_generation,
                    input.worker_incarnation,
                ),
                Some(input.sequence),
            ),
            PtyWireFrame::Resize(resize) => (
                &resize.stream_id,
                &resize.execution_generation,
                (
                    &resize.session_target_id,
                    resize.registration_generation,
                    resize.worker_incarnation,
                ),
                Some(resize.sequence),
            ),
            PtyWireFrame::Cancel(cancel) => {
                if matches!(
                    cancel.reason,
                    PtyCloseReason::Exited
                        | PtyCloseReason::TimedOut
                        | PtyCloseReason::OutcomeUnknown
                ) {
                    return Err(CarrierError::InvalidDirection);
                }
                (
                    &cancel.stream_id,
                    &cancel.execution_generation,
                    (
                        &cancel.session_target_id,
                        cancel.registration_generation,
                        cancel.worker_incarnation,
                    ),
                    None,
                )
            }
            _ => return Err(CarrierError::InvalidDirection),
        };
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entry = inner
            .get_mut(stream_id)
            .ok_or(CarrierError::MissingStream)?;
        if entry.link_id != link_id {
            return Err(CarrierError::WrongLink);
        }
        if entry.execution_generation != *generation
            || entry.session_target_id != *binding.0
            || entry.registration_generation != binding.1
            || entry.wire_worker_incarnation != binding.2
        {
            return Err(CarrierError::StaleBinding);
        }
        if let Some(sequence) = sequence {
            if sequence != entry.next_input_sequence {
                return Err(CarrierError::SequenceViolation);
            }
            entry.next_input_sequence = entry
                .next_input_sequence
                .checked_add(1)
                .ok_or(CarrierError::SequenceViolation)?;
        }
        match &entry.destination {
            CarrierDestination::Daemon { control } => Ok(CarrierDispatch::Daemon {
                control: control.clone(),
                frame,
            }),
            CarrierDestination::Worker { worker_key, .. } => {
                let command = match frame {
                    PtyWireFrame::Input(input) => ServiceToWorker::ExecPtyInput(input),
                    PtyWireFrame::Resize(resize) => ServiceToWorker::ExecPtyResize(resize),
                    PtyWireFrame::Cancel(cancel) => ServiceToWorker::ExecPtyCancel(cancel),
                    _ => unreachable!("direction checked above"),
                };
                Ok(CarrierDispatch::Worker {
                    worker_key: worker_key.clone(),
                    command,
                })
            }
        }
    }
}

fn route_output_frame(entry: &mut CarrierEntry, frame: PtyWireFrame) -> Result<(), CarrierError> {
    match &frame {
        PtyWireFrame::Opened(opened) => {
            if entry.opened
                || entry.task_id != opened.task_id
                || !binding_matches_opened(entry, opened)
            {
                return Err(CarrierError::StaleBinding);
            }
            entry.opened = true;
        }
        PtyWireFrame::Output(output) => {
            if !entry.opened || !binding_matches_output(entry, output) {
                return Err(CarrierError::StaleBinding);
            }
            if output.sequence != entry.next_output_sequence {
                return Err(CarrierError::SequenceViolation);
            }
            entry.next_output_sequence = entry
                .next_output_sequence
                .checked_add(1)
                .ok_or(CarrierError::SequenceViolation)?;
        }
        PtyWireFrame::Closed(closed) => {
            if !entry.opened || !binding_matches_closed(entry, closed) {
                return Err(CarrierError::StaleBinding);
            }
        }
        _ => return Err(CarrierError::InvalidDirection),
    }
    let encoded = exec_pty_wire::encode(&frame).map_err(|_| CarrierError::EncodeFailed)?;
    match entry.outbound.try_send(encoded) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err(CarrierError::SlowConsumer),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(CarrierError::LinkClosed),
    }
}

impl ExecPtyCarrierRegistry {
    pub fn remove_link(&self, link_id: PtyCarrierLinkId) -> Vec<CarrierCancellation> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let streams = inner
            .iter()
            .filter(|(_, entry)| entry.link_id == link_id)
            .map(|(stream_id, _)| stream_id.clone())
            .collect::<Vec<_>>();
        streams
            .into_iter()
            .filter_map(|stream_id| {
                inner.remove(&stream_id).map(|entry| {
                    cancellation(stream_id, entry, PtyCloseReason::CarrierDisconnected)
                })
            })
            .collect()
    }

    pub fn remove_stream(
        &self,
        stream_id: &str,
        reason: PtyCloseReason,
    ) -> Option<CarrierCancellation> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(stream_id)
            .map(|entry| cancellation(stream_id.to_string(), entry, reason))
    }

    pub fn contains(&self, stream_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(stream_id)
    }
}

fn cancellation(
    stream_id: String,
    entry: CarrierEntry,
    reason: PtyCloseReason,
) -> CarrierCancellation {
    CarrierCancellation {
        stream_id,
        execution_generation: entry.execution_generation,
        session_target_id: entry.session_target_id,
        registration_generation: entry.registration_generation,
        worker_incarnation: entry.wire_worker_incarnation,
        destination: entry.destination,
        reason,
    }
}

fn validate_worker(
    entry: &CarrierEntry,
    worker_key: Option<&WorkerKey>,
    worker_incarnation: WorkerIncarnation,
) -> Result<(), CarrierError> {
    match &entry.destination {
        CarrierDestination::Worker {
            worker_key: expected_key,
            worker_incarnation: expected_incarnation,
        } if expected_key.as_ref() == worker_key && *expected_incarnation == worker_incarnation => {
            Ok(())
        }
        _ => Err(CarrierError::StaleWorker),
    }
}

fn binding_matches_opened(entry: &CarrierEntry, frame: &PtyStreamOpened) -> bool {
    entry.execution_generation == frame.execution_generation
        && entry.session_target_id == frame.session_target_id
        && entry.registration_generation == frame.registration_generation
        && entry.wire_worker_incarnation == frame.worker_incarnation
}

fn binding_matches_output(entry: &CarrierEntry, frame: &PtyOutputFrame) -> bool {
    entry.execution_generation == frame.execution_generation
        && entry.session_target_id == frame.session_target_id
        && entry.registration_generation == frame.registration_generation
        && entry.wire_worker_incarnation == frame.worker_incarnation
}

fn binding_matches_closed(entry: &CarrierEntry, frame: &PtyStreamClosed) -> bool {
    entry.execution_generation == frame.execution_generation
        && entry.session_target_id == frame.session_target_id
        && entry.registration_generation == frame.registration_generation
        && entry.wire_worker_incarnation == frame.worker_incarnation
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::exec::{
        ApprovalId, ExecContainmentSnapshot, ExecExecutionBasis, ExecIoMode, ExecRequestId,
        ExecShellKind,
    };
    use desk_ipc_protocol::message::{DesktopTarget, SessionKey};

    fn key() -> WorkerKey {
        WorkerKey {
            session: SessionKey {
                platform_session_id: "session-1".into(),
                session_generation: 7,
            },
            desktop: DesktopTarget::LinuxSession,
        }
    }

    fn start() -> ExecPtyStartPayload {
        ExecPtyStartPayload {
            request_id: "generation-1".into(),
            connection_id: None,
            exec_pty: true,
            exec_pty_elevation: false,
            stream_id: "stream-1".into(),
            session_target_id: "session-1".into(),
            registration_generation: 7,
            worker_incarnation: 9,
            plan: desk_agent_protocol::exec::ExecPlan {
                exec_request_id: ExecRequestId("task-1".into()),
                execution_generation: "generation-1".into(),
                program: "printf".into(),
                argv: vec!["hello".into()],
                cwd: None,
                shell: ExecShellKind::Native,
                risk: RiskLevel::High,
                io_mode: ExecIoMode::Pty {
                    initial_rows: 24,
                    initial_cols: 80,
                },
                execution_basis: ExecExecutionBasis::Template,
                template_id: "test".into(),
                approval_id: ApprovalId("approval-1".into()),
                fingerprint: "fp".into(),
                timeout_ms: 5_000,
                max_stdout_bytes: 4096,
                max_stderr_bytes: 4096,
                containment: ExecContainmentSnapshot::default(),
            },
            audit_source_request_id: None,
        }
    }

    #[tokio::test]
    async fn output_is_bound_to_exact_worker_and_link() {
        let registry = ExecPtyCarrierRegistry::new();
        let link = PtyCarrierLinkId::new();
        let (tx, mut rx) = mpsc::channel(CARRIER_OUTPUT_QUEUE_CAP);
        registry
            .bind(
                link,
                &start(),
                Some(key()),
                WorkerIncarnation::for_test(9),
                tx,
            )
            .unwrap();
        registry
            .route_worker_opened(
                Some(&key()),
                WorkerIncarnation::for_test(9),
                PtyStreamOpened {
                    task_id: "task-1".into(),
                    execution_generation: "generation-1".into(),
                    stream_id: "stream-1".into(),
                    session_target_id: "session-1".into(),
                    registration_generation: 7,
                    worker_incarnation: 9,
                },
            )
            .unwrap();
        assert!(rx.recv().await.is_some());
        assert_eq!(
            registry.route_worker_output(
                Some(&key()),
                WorkerIncarnation::for_test(10),
                PtyOutputFrame {
                    stream_id: "stream-1".into(),
                    execution_generation: "generation-1".into(),
                    session_target_id: "session-1".into(),
                    registration_generation: 7,
                    worker_incarnation: 9,
                    sequence: 0,
                    data: b"x".to_vec(),
                },
            ),
            Err(CarrierError::StaleWorker)
        );
    }

    #[tokio::test]
    async fn daemon_destination_keeps_binary_input_on_local_control_lane() {
        let registry = ExecPtyCarrierRegistry::new();
        let link = PtyCarrierLinkId::new();
        let (outbound, mut outbound_rx) = mpsc::channel(CARRIER_OUTPUT_QUEUE_CAP);
        let (control, mut control_rx) = mpsc::channel(4);
        registry
            .bind_daemon(link, &start(), control, outbound)
            .unwrap();
        let crossed_start_barrier = std::cell::Cell::new(false);
        registry
            .start_daemon(
                PtyStreamOpened {
                    task_id: "task-1".into(),
                    execution_generation: "generation-1".into(),
                    stream_id: "stream-1".into(),
                    session_target_id: "session-1".into(),
                    registration_generation: 7,
                    worker_incarnation: 9,
                },
                || {
                    crossed_start_barrier.set(true);
                    Ok::<_, ()>(())
                },
            )
            .expect("carrier start barrier should be live")
            .expect("test start closure should succeed");
        assert!(crossed_start_barrier.get());
        assert!(outbound_rx.recv().await.is_some());

        let input = desk_agent_protocol::exec_pty::PtyInputFrame {
            stream_id: "stream-1".into(),
            execution_generation: "generation-1".into(),
            session_target_id: "session-1".into(),
            registration_generation: 7,
            worker_incarnation: 9,
            sequence: 0,
            data: vec![0, 0xff, b'\n'],
        };
        let CarrierDispatch::Daemon { control, frame } = registry
            .accept_upstream_frame(link, PtyWireFrame::Input(input.clone()))
            .unwrap()
        else {
            panic!("daemon-bound stream was routed to a worker");
        };
        control.send(frame).await.unwrap();
        let PtyWireFrame::Input(received) = control_rx.recv().await.unwrap() else {
            panic!("unexpected control frame");
        };
        assert_eq!(received.data, input.data);

        let invalid_cancel = desk_agent_protocol::exec_pty::PtyCancelFrame {
            stream_id: "stream-1".into(),
            execution_generation: "generation-1".into(),
            session_target_id: "session-1".into(),
            registration_generation: 7,
            worker_incarnation: 9,
            reason: PtyCloseReason::Exited,
        };
        assert!(matches!(
            registry.accept_upstream_frame(link, PtyWireFrame::Cancel(invalid_cancel)),
            Err(CarrierError::InvalidDirection)
        ));
    }

    #[test]
    fn removed_daemon_carrier_cannot_cross_start_barrier() {
        let registry = ExecPtyCarrierRegistry::new();
        let link = PtyCarrierLinkId::new();
        let (outbound, _outbound_rx) = mpsc::channel(CARRIER_OUTPUT_QUEUE_CAP);
        let (control, _control_rx) = mpsc::channel(4);
        registry
            .bind_daemon(link, &start(), control, outbound)
            .unwrap();
        registry.remove_link(link);

        let called = std::cell::Cell::new(false);
        let result = registry.start_daemon(
            PtyStreamOpened {
                task_id: "task-1".into(),
                execution_generation: "generation-1".into(),
                stream_id: "stream-1".into(),
                session_target_id: "session-1".into(),
                registration_generation: 7,
                worker_incarnation: 9,
            },
            || {
                called.set(true);
                Ok::<_, ()>(())
            },
        );
        assert!(matches!(result, Err(CarrierError::MissingStream)));
        assert!(!called.get());
    }
}
