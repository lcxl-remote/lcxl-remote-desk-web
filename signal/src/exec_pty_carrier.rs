//! Volatile browser ↔ host exec-PTY carrier for the single-instance OSS signal.
//!
//! The registry deliberately owns no durable state and never stores input bytes.
//! Browser input is decoded only for binding/sequence validation and is then
//! forwarded immediately to the target host's already-live signaling socket.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use desk_agent_protocol::exec_pty::{
    PtyCancelFrame, PtyCarrierPrepare, PtyCloseReason, PtyStreamOpened,
};
use desk_agent_protocol::exec_pty_wire::{self, PtyWireFrame};
use desk_signal_facade::error::DeskSignalFacadeError;
use desk_signal_facade::model::auth_context::AuthKind;
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::RemoteDeskTypeEnum;
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::service::BinaryFrameObserver;
use desk_utils::error::DeskErrorCode;
use tokio::sync::mpsc;

pub const BROWSER_OUTPUT_QUEUE_CAP: usize = 16;
const MAX_LIVE_CARRIERS: usize = 128;

#[derive(Clone, Default)]
pub struct SignalExecPtyCarriers {
    inner: Arc<Mutex<HashMap<String, CarrierEntry>>>,
}

struct CarrierEntry {
    browser_connection_id: String,
    target_connection_id: String,
    exec_request_id: String,
    output_tx: mpsc::Sender<Vec<u8>>,
    consumed: bool,
    execution_generation: Option<String>,
    opened: Option<PtyStreamOpened>,
    next_input_sequence: u64,
    next_output_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierError {
    InvalidPrepare,
    ApprovalNotPending,
    BrowserNotOwner,
    TargetOffline,
    Duplicate,
    Capacity,
    Missing,
    NotConsumed,
    WrongTarget,
    WrongDirection,
    StaleBinding,
    SequenceViolation,
    SlowConsumer,
    Transport,
}

impl CarrierError {
    fn message(self) -> &'static str {
        match self {
            Self::InvalidPrepare => "invalid interactive carrier request",
            Self::ApprovalNotPending => "interactive approval is no longer pending",
            Self::BrowserNotOwner => "interactive execution requires the device owner",
            Self::TargetOffline => "target device is offline",
            Self::Duplicate => "an interactive carrier already exists for this request",
            Self::Capacity => "too many interactive carriers are active",
            Self::Missing => "interactive carrier is no longer live",
            Self::NotConsumed => "interactive carrier has not been approved",
            Self::WrongTarget => "interactive carrier target changed",
            Self::WrongDirection => "invalid interactive carrier frame direction",
            Self::StaleBinding => "interactive carrier binding is stale",
            Self::SequenceViolation => "interactive carrier sequence is invalid",
            Self::SlowConsumer => "interactive carrier cannot keep up with output",
            Self::Transport => "interactive carrier transport closed",
        }
    }
}

impl std::fmt::Display for CarrierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl SignalExecPtyCarriers {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn prepare(
        &self,
        request: &PtyCarrierPrepare,
        connections: &SharedConnectionMap,
        output_tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<String, CarrierError> {
        request
            .validate()
            .map_err(|_| CarrierError::InvalidPrepare)?;
        if !crate::agent_exec::global_agent_exec_pending().can_prepare_carrier(
            &request.browser_connection_id,
            &request.target_connection_id,
            &request.exec_request_id,
        ) {
            return Err(CarrierError::ApprovalNotPending);
        }
        {
            let map = connections.read().await;
            let browser = map
                .get(&request.browser_connection_id)
                .ok_or(CarrierError::BrowserNotOwner)?;
            let target = map
                .get(&request.target_connection_id)
                .ok_or(CarrierError::TargetOffline)?;
            if browser.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Browser
                || browser.auth_context.auth_kind != AuthKind::CookieAuth
                || browser.auth_context.user_id
                    != Some(crate::control_authorizer::SINGLE_ACCOUNT_USER_ID)
            {
                return Err(CarrierError::BrowserNotOwner);
            }
            if target.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Server
                || target.auth_context.auth_kind != AuthKind::TokenAuth
                || !target.model.version_info.exec_pty
            {
                return Err(CarrierError::TargetOffline);
            }
        }

        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.len() >= MAX_LIVE_CARRIERS {
            return Err(CarrierError::Capacity);
        }
        if inner
            .values()
            .any(|entry| entry.exec_request_id == request.exec_request_id)
        {
            return Err(CarrierError::Duplicate);
        }
        let carrier_id = uuid::Uuid::new_v4().to_string();
        inner.insert(
            carrier_id.clone(),
            CarrierEntry {
                browser_connection_id: request.browser_connection_id.clone(),
                target_connection_id: request.target_connection_id.clone(),
                exec_request_id: request.exec_request_id.clone(),
                output_tx,
                consumed: false,
                execution_generation: None,
                opened: None,
                next_input_sequence: 0,
                next_output_sequence: 0,
            },
        );
        Ok(carrier_id)
    }

    pub fn consume_for_approval(
        &self,
        carrier_id: &str,
        browser_connection_id: &str,
        target_connection_id: &str,
        exec_request_id: &str,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(entry) = inner.get_mut(carrier_id) else {
            return false;
        };
        if entry.consumed
            || entry.output_tx.is_closed()
            || entry.browser_connection_id != browser_connection_id
            || entry.target_connection_id != target_connection_id
            || entry.exec_request_id != exec_request_id
        {
            return false;
        }
        entry.consumed = true;
        true
    }

    /// Roll back a carrier consumed by an approval whose waiting tool call has
    /// already disappeared. The browser endpoint observes the dropped sender
    /// and closes; no consumed carrier can be replayed by a later approval.
    pub fn release_failed_approval(&self, carrier_id: &str, exec_request_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.get(carrier_id).is_some_and(|entry| {
            entry.exec_request_id == exec_request_id
                && entry.consumed
                && entry.execution_generation.is_none()
                && entry.opened.is_none()
        }) {
            inner.remove(carrier_id);
        }
    }

    /// Final live check immediately before the central brain writes the sealed
    /// edge request to the host. Once bound, an endpoint disconnect can cancel
    /// by generation even if it races before `PtyStreamOpened` returns.
    pub fn bind_for_dispatch(
        &self,
        carrier_id: &str,
        target_connection_id: &str,
        exec_request_id: &str,
        execution_generation: &str,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(entry) = inner.get_mut(carrier_id) else {
            return false;
        };
        if !entry.consumed
            || entry.output_tx.is_closed()
            || entry.target_connection_id != target_connection_id
            || entry.exec_request_id != exec_request_id
            || entry.execution_generation.is_some()
        {
            return false;
        }
        entry.execution_generation = Some(execution_generation.to_string());
        true
    }

    pub fn release_dispatch(&self, carrier_id: &str, execution_generation: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.get(carrier_id).is_some_and(|entry| {
            entry.execution_generation.as_deref() == Some(execution_generation)
                && entry.opened.is_none()
        }) {
            inner.remove(carrier_id);
        }
    }

    pub async fn forward_browser_binary(
        &self,
        carrier_id: &str,
        bytes: Bytes,
        connections: &SharedConnectionMap,
    ) -> Result<(), CarrierError> {
        let frame = exec_pty_wire::decode(&bytes).map_err(|_| CarrierError::StaleBinding)?;
        let target_connection_id = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let entry = inner.get_mut(carrier_id).ok_or(CarrierError::Missing)?;
            if !entry.consumed {
                return Err(CarrierError::NotConsumed);
            }
            let opened = entry.opened.clone().ok_or(CarrierError::StaleBinding)?;
            match &frame {
                PtyWireFrame::Input(input) => {
                    validate_browser_binding(
                        entry,
                        &opened,
                        &input.stream_id,
                        &input.execution_generation,
                        &input.session_target_id,
                        input.registration_generation,
                        input.worker_incarnation,
                        Some(input.sequence),
                    )?;
                }
                PtyWireFrame::Resize(resize) => {
                    validate_browser_binding(
                        entry,
                        &opened,
                        &resize.stream_id,
                        &resize.execution_generation,
                        &resize.session_target_id,
                        resize.registration_generation,
                        resize.worker_incarnation,
                        Some(resize.sequence),
                    )?;
                }
                PtyWireFrame::Cancel(cancel) => {
                    validate_browser_binding(
                        entry,
                        &opened,
                        &cancel.stream_id,
                        &cancel.execution_generation,
                        &cancel.session_target_id,
                        cancel.registration_generation,
                        cancel.worker_incarnation,
                        None,
                    )?;
                }
                _ => return Err(CarrierError::WrongDirection),
            }
            entry.target_connection_id.clone()
        };
        let target = connections
            .read()
            .await
            .get(&target_connection_id)
            .cloned()
            .ok_or(CarrierError::TargetOffline)?;
        target
            .session
            .write()
            .await
            .binary(bytes)
            .await
            .map_err(|_| CarrierError::Transport)
    }

    async fn route_host_binary(
        &self,
        source: &ConnectionState,
        bytes: Bytes,
    ) -> Result<(), CarrierError> {
        if source.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Server
            || source.auth_context.auth_kind != AuthKind::TokenAuth
        {
            return Err(CarrierError::WrongTarget);
        }
        let frame = exec_pty_wire::decode(&bytes).map_err(|_| CarrierError::StaleBinding)?;
        let carrier_id = frame.stream_id().to_string();
        let (output_tx, cancel) =
            match self.admit_host_frame(&source.model.connection_id, &carrier_id, &frame) {
                Ok(route) => route,
                Err(CarrierError::Missing) => {
                    if let Some(cancel) = cancel_from_host_frame(&frame) {
                        send_cancel(source, cancel).await;
                        return Ok(());
                    }
                    // A terminal Closed frame may race with browser disconnect or
                    // output-queue cancellation, both of which remove the volatile
                    // carrier first. It is an idempotent completion, not a reason
                    // to tear down the host's shared signaling socket.
                    if matches!(frame, PtyWireFrame::Closed(_)) {
                        return Ok(());
                    }
                    return Err(CarrierError::Missing);
                }
                Err(error) => return Err(error),
            };
        match output_tx.try_send(bytes.to_vec()) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.inner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&carrier_id);
                if let Some(cancel) = cancel {
                    send_cancel(source, cancel).await;
                }
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.inner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&carrier_id);
                if let Some(cancel) = cancel {
                    send_cancel(source, cancel).await;
                }
                Ok(())
            }
        }
    }

    fn admit_host_frame(
        &self,
        source_connection_id: &str,
        carrier_id: &str,
        frame: &PtyWireFrame,
    ) -> Result<(mpsc::Sender<Vec<u8>>, Option<PtyCancelFrame>), CarrierError> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entry = inner.get_mut(carrier_id).ok_or(CarrierError::Missing)?;
        if !entry.consumed {
            return Err(CarrierError::NotConsumed);
        }
        if entry.target_connection_id != source_connection_id {
            return Err(CarrierError::WrongTarget);
        }
        match frame {
            PtyWireFrame::Opened(opened) => {
                if entry.opened.is_some()
                    || opened.task_id != entry.exec_request_id
                    || opened.stream_id != carrier_id
                    || entry.execution_generation.as_deref()
                        != Some(opened.execution_generation.as_str())
                {
                    return Err(CarrierError::StaleBinding);
                }
                entry.opened = Some(opened.clone());
            }
            PtyWireFrame::Output(output) => {
                let opened = entry.opened.as_ref().ok_or(CarrierError::StaleBinding)?;
                validate_host_binding(
                    opened,
                    &output.stream_id,
                    &output.execution_generation,
                    &output.session_target_id,
                    output.registration_generation,
                    output.worker_incarnation,
                )?;
                if output.sequence != entry.next_output_sequence {
                    return Err(CarrierError::SequenceViolation);
                }
                entry.next_output_sequence = entry
                    .next_output_sequence
                    .checked_add(1)
                    .ok_or(CarrierError::SequenceViolation)?;
            }
            PtyWireFrame::Closed(closed) => {
                let opened = entry.opened.as_ref().ok_or(CarrierError::StaleBinding)?;
                validate_host_binding(
                    opened,
                    &closed.stream_id,
                    &closed.execution_generation,
                    &closed.session_target_id,
                    closed.registration_generation,
                    closed.worker_incarnation,
                )?;
            }
            _ => return Err(CarrierError::WrongDirection),
        }
        let output_tx = entry.output_tx.clone();
        let cancel = entry
            .opened
            .as_ref()
            .map(|opened| cancel_from_opened(opened, PtyCloseReason::SlowConsumer));
        if matches!(frame, PtyWireFrame::Closed(_)) {
            inner.remove(carrier_id);
        }
        Ok((output_tx, cancel))
    }

    pub async fn disconnect(
        &self,
        carrier_id: &str,
        connections: &SharedConnectionMap,
        reason: PtyCloseReason,
    ) {
        let removed = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(carrier_id);
        let Some(entry) = removed else { return };
        if let Some(opened) = entry.opened {
            let target = connections
                .read()
                .await
                .get(&entry.target_connection_id)
                .cloned();
            if let Some(target) = target {
                send_cancel(&target, cancel_from_opened(&opened, reason)).await;
            }
        } else if let Some(execution_generation) = entry.execution_generation {
            let target = connections
                .read()
                .await
                .get(&entry.target_connection_id)
                .cloned();
            if let Some(target) = target {
                send_generation_cancel(&target, execution_generation).await;
            }
        }
        crate::agent_exec::global_agent_exec_pending().cancel_approval(&entry.exec_request_id);
    }

    pub async fn disconnect_target(&self, target_connection_id: &str) {
        let removed = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let ids = inner
                .iter()
                .filter(|(_, entry)| entry.target_connection_id == target_connection_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| inner.remove(&id))
                .collect::<Vec<_>>()
        };
        for entry in &removed {
            crate::agent_exec::global_agent_exec_pending().cancel_approval(&entry.exec_request_id);
        }
        drop(removed);
    }
}

fn validate_browser_binding(
    entry: &mut CarrierEntry,
    opened: &PtyStreamOpened,
    stream_id: &str,
    generation: &str,
    session_target_id: &str,
    registration_generation: u64,
    worker_incarnation: u64,
    sequence: Option<u64>,
) -> Result<(), CarrierError> {
    validate_host_binding(
        opened,
        stream_id,
        generation,
        session_target_id,
        registration_generation,
        worker_incarnation,
    )?;
    if let Some(sequence) = sequence {
        if sequence != entry.next_input_sequence {
            return Err(CarrierError::SequenceViolation);
        }
        entry.next_input_sequence = entry
            .next_input_sequence
            .checked_add(1)
            .ok_or(CarrierError::SequenceViolation)?;
    }
    Ok(())
}

fn validate_host_binding(
    opened: &PtyStreamOpened,
    stream_id: &str,
    generation: &str,
    session_target_id: &str,
    registration_generation: u64,
    worker_incarnation: u64,
) -> Result<(), CarrierError> {
    if opened.stream_id != stream_id
        || opened.execution_generation != generation
        || opened.session_target_id != session_target_id
        || opened.registration_generation != registration_generation
        || opened.worker_incarnation != worker_incarnation
    {
        return Err(CarrierError::StaleBinding);
    }
    Ok(())
}

fn cancel_from_opened(opened: &PtyStreamOpened, reason: PtyCloseReason) -> PtyCancelFrame {
    PtyCancelFrame {
        stream_id: opened.stream_id.clone(),
        execution_generation: opened.execution_generation.clone(),
        session_target_id: opened.session_target_id.clone(),
        registration_generation: opened.registration_generation,
        worker_incarnation: opened.worker_incarnation,
        reason,
    }
}

fn cancel_from_host_frame(frame: &PtyWireFrame) -> Option<PtyCancelFrame> {
    match frame {
        PtyWireFrame::Opened(opened) => Some(cancel_from_opened(
            opened,
            PtyCloseReason::CarrierDisconnected,
        )),
        PtyWireFrame::Output(output) => Some(PtyCancelFrame {
            stream_id: output.stream_id.clone(),
            execution_generation: output.execution_generation.clone(),
            session_target_id: output.session_target_id.clone(),
            registration_generation: output.registration_generation,
            worker_incarnation: output.worker_incarnation,
            reason: PtyCloseReason::CarrierDisconnected,
        }),
        PtyWireFrame::Closed(_) => None,
        _ => None,
    }
}

async fn send_cancel(target: &ConnectionState, cancel: PtyCancelFrame) {
    if let Ok(encoded) = exec_pty_wire::encode(&PtyWireFrame::Cancel(cancel)) {
        let _ = target.session.write().await.binary(encoded).await;
    }
}

async fn send_generation_cancel(target: &ConnectionState, execution_generation: String) {
    let payload = desk_agent_protocol::exec_lifecycle::ExecControlPayload {
        execution_generation: execution_generation.clone(),
        action: desk_agent_protocol::exec_lifecycle::ExecControlAction::Cancel {
            requested_by: "pty-carrier-disconnect".into(),
        },
    };
    let model = SignalingModel::new(
        &execution_generation,
        SignalingType::ControlExecution,
        None,
        Some(target.model.connection_id.clone()),
        serde_json::to_value(payload).ok(),
        None,
    );
    if let Ok(text) = serde_json::to_string(&model) {
        let _ = target.session.write().await.text(text).await;
    }
}

impl BinaryFrameObserver for SignalExecPtyCarriers {
    fn on_binary_frame<'a>(
        &'a self,
        source: &'a ConnectionState,
        frame: Bytes,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.route_host_binary(source, frame)
                .await
                .map_err(|error| {
                    DeskSignalFacadeError::new_custom_error(
                        DeskErrorCode::INVALID_PARAMS,
                        error.message(),
                    )
                })
        })
    }
}

pub fn global_exec_pty_carriers() -> Arc<SignalExecPtyCarriers> {
    static REGISTRY: OnceLock<Arc<SignalExecPtyCarriers>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Arc::new(SignalExecPtyCarriers::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_metadata_is_bounded() {
        let mut prepare = PtyCarrierPrepare {
            browser_connection_id: "browser".into(),
            target_connection_id: "target".into(),
            exec_request_id: "exec".into(),
        };
        assert_eq!(prepare.validate(), Ok(()));
        prepare.exec_request_id = "x".repeat(129);
        assert!(prepare.validate().is_err());
    }
}
