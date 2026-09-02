//! Single-node OSS Signal remote Provider transport.
//!
//! The Device Assistant loop runs in Signal while the host worker performs
//! local reads and confirmed mutations. Reads use server-stamped remote-tool
//! frames; writes use exact durable grants and sealed Computer Action plans.

mod object_read;
use desk_diagnose_core::input_read_context::object_read::requires_objects;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use desk_agent_protocol::authz::{
    AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthorizedControlPayload, AuthzActor,
    AuthzDevice, ExecAdmissionPolicy,
};
#[cfg(test)]
use desk_agent_protocol::browser_control::{
    BROWSER_CONTROL_SCHEMA_VERSION, BrowserAction, BrowserActionRequest, BrowserActionResult,
    BrowserElementRef, BrowserFormFieldReadback, BrowserFormReadbackKind, BrowserMutationClass,
    BrowserPageRef,
};
use desk_agent_protocol::capability_grant::{
    CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer, CapabilityGrantLimits,
    CapabilityGrantUsePolicy, CapabilityRiskTier,
};
use desk_agent_protocol::capability_provider::{
    AuthorizationResourceKind, ExecutionLocality, ProductSurface,
};
use desk_agent_protocol::communication::{
    CommunicationPrepareVerification, CommunicationSendAuthority, GmailWebDraftHandoffInput,
    SlackWebDraftHandoffInput,
};
use desk_agent_protocol::computer_use::{
    BatchDocumentOutput, COMPUTER_USE_SCHEMA_VERSION, ComputerActionCompleted, ComputerActionKind,
    ComputerActionOutput, ComputerActionResultClass, ComputerActionStarted,
    ComputerActionStateReport, ComputerActionStep, ComputerUseAdapterKind, ComputerUseAdapterRef,
    DocumentLiveBatchPatchAction, DocumentLivePatchAction, FileContentReadParams,
    FileMetadataInspectParams, FilePatchAction, LiveDocumentInspectParams, ObjectKind, ObjectRef,
    OfficeInspectParams, PresentationLiveBatchPatchAction, PresentationLivePatchAction,
    SealedComputerActionPlan, SpreadsheetFileInspectParams, SpreadsheetLiveBatchPatchAction,
    SpreadsheetLivePatchAction, SpreadsheetMergePreviewParams, TerminalOutputInspectParams,
    UiSemanticAction,
};
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
use desk_agent_protocol::exec::{ApprovalId, CommandDraft, ExecRequestId};
use desk_agent_protocol::remote_tool::{
    MAX_REMOTE_TOOL_RESULT_BYTES, REMOTE_TOOL_TIMEOUT_SECS, RemoteToolOutput, RemoteToolRequest,
    RemoteToolResponse, RemoteToolResponseChunk,
};
use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentError, AgentErrorKind, AgentOperation, AgentOutcome,
    AgentScope, AuditMeta, CallerRef, CallerType, ContextKind, ExecutionMode, OperationInput,
    ProtocolVersion, ReadContextInput, RequestId, TargetRef,
};
use desk_diagnose_core::capability_grant::{
    CapabilityGrantCall, canonical_compiled_scope, exact_command_resource_scope,
    exact_external_query_resource_scope, exact_external_url_resource_scope,
    fresh_object_resource_scope, match_capability_grant,
};
use desk_diagnose_core::chat::ToolCall;
use desk_diagnose_core::chunk::ByteReassembler;
use desk_diagnose_core::device_assistant::{
    EXECUTE_CONFIRMED_RAW_INPUT_TOOL, EXECUTE_CONFIRMED_UI_ACTION_TOOL,
    PREVIEW_COMPUTER_ACTION_TOOL, validate_preview_call,
};
use desk_diagnose_core::permission_tools::canonical_tool_permission_input_json;
use desk_diagnose_core::provider_registry::ProviderRegistry;
use desk_diagnose_core::read_tools::build_read_operation;
use desk_diagnose_core::seam::{ExecContext, ExecOutcome, ToolRunOutput, ToolSeam, WaitOutcome};
use desk_diagnose_core::sink_authorizer::{
    DefaultSinkAuthorizer, ExportDataAuthorization, SinkAuthorizer, SinkInput, authorize_export,
};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::service::{ComputerActionObserver, RemoteToolObserver};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::capability_grant_store::{
    CapabilityDispatchCompletion, CapabilityDispatchOutcome, DispatchClaimResult,
    DispatchIntentResult, PrepareCapabilityCall, SignalCapabilityGrantStore,
};
use crate::entity::agent_session;
use crate::web_research::{
    WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME, fetch_public_web_page, search_public_web,
    validate_fetch_call, validate_search_call,
};

pub(crate) mod completion;

fn error(
    kind: AgentErrorKind,
    message: impl Into<String>,
    retryable: bool,
    safe_for_model: bool,
) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model,
        error_code: None,
    }
}

fn browser_policy_auto_authorized(risk_tier: CapabilityRiskTier) -> bool {
    matches!(risk_tier, CapabilityRiskTier::R0 | CapabilityRiskTier::R1)
}

#[cfg(test)]
fn exact_field_readback<'a>(
    result: &'a BrowserActionResult,
    expected: &BrowserElementRef,
    value: &str,
) -> Option<&'a BrowserFormFieldReadback> {
    let mut matching = result.form_readback.iter().filter(|readback| {
        readback.request_element_id == expected.element_id
            && readback.request_role == expected.role
            && readback.request_accessible_name == expected.accessible_name
            && readback.value == value
    });
    let matched = matching.next()?;
    matching.next().is_none().then_some(matched)
}

#[cfg(test)]
fn gmail_exact_form_readback(
    result: &BrowserActionResult,
    gmail: &GmailWebDraftHandoffInput,
) -> bool {
    if result.form_readback.len() != 3 {
        return false;
    }
    let Some(to) = exact_field_readback(
        result,
        &gmail.to_field,
        gmail.draft.recipients[0].address.as_str(),
    ) else {
        return false;
    };
    let Some(subject) =
        exact_field_readback(result, &gmail.subject_field, gmail.draft.subject.as_str())
    else {
        return false;
    };
    let Some(body) = exact_field_readback(
        result,
        &gmail.body_field,
        gmail.draft.body_plain_text.as_str(),
    ) else {
        return false;
    };
    if subject.kind != BrowserFormReadbackKind::ControlValue
        || body.kind != BrowserFormReadbackKind::ControlValue
    {
        return false;
    }
    let Some(container) = to.container_element_id.as_deref() else {
        return false;
    };
    if subject.container_element_id.as_deref() != Some(container)
        || body.container_element_id.as_deref() != Some(container)
    {
        return false;
    }
    matches!(
        to.kind,
        BrowserFormReadbackKind::ControlValue | BrowserFormReadbackKind::CommittedText
    )
}

#[cfg(test)]
fn gmail_exact_attachment_readback(
    result: &BrowserActionResult,
    gmail: &GmailWebDraftHandoffInput,
) -> bool {
    let Some(attachment) = &gmail.attachment else {
        return result.outcome
            == desk_agent_protocol::browser_control::BrowserActionOutcome::FormFilled;
    };
    result.outcome == desk_agent_protocol::browser_control::BrowserActionOutcome::FormFilledWithFile
        && result.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.elements.iter().any(|element| {
                element.accessible_name == attachment.artifact.file_name
                    || element.value.as_deref() == Some(attachment.artifact.file_name.as_str())
            })
        })
}

fn decode_output(bytes: &[u8]) -> Result<RemoteToolOutput, AgentError> {
    let output = serde_json::from_slice::<RemoteToolOutput>(bytes).map_err(|e| {
        error(
            AgentErrorKind::TransportError,
            format!("failed to decode remote tool output: {e}"),
            false,
            true,
        )
    })?;
    if let Some(image) = &output.image {
        desk_diagnose_core::image_input::validate_remote_tool_image(image).map_err(|e| {
            error(
                AgentErrorKind::TransportError,
                format!("invalid remote tool image: {e}"),
                false,
                true,
            )
        })?;
    }
    Ok(output)
}

fn tool_output_fingerprint(output: &ToolRunOutput) -> Result<(u64, String), AgentError> {
    let payload = desk_diagnose_core::model_egress::message_payload_bytes(
        &output.content,
        output.image_data_url.as_deref(),
    )
    .map_err(|err| {
        error(
            AgentErrorKind::InvalidInput,
            format!("read result cannot be enveloped: {err}"),
            false,
            true,
        )
    })?;
    let size_bytes = u64::try_from(payload.len()).map_err(|_| {
        error(
            AgentErrorKind::InvalidInput,
            "read result is too large to envelope",
            false,
            true,
        )
    })?;
    if size_bytes == 0 {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "read result is empty",
            false,
            true,
        ));
    }
    Ok((size_bytes, format!("{:x}", Sha256::digest(&payload))))
}

struct PendingRemoteTool {
    source_connection_id: String,
    reassembler: ByteReassembler,
    completion: oneshot::Sender<Result<RemoteToolOutput, AgentError>>,
}

pub enum AcceptOutcome {
    NeedMore,
    Delivered,
    Rejected(&'static str),
}

#[derive(Default)]
pub struct SignalRemoteToolPendingStore {
    inner: Mutex<HashMap<String, PendingRemoteTool>>,
}

impl SignalRemoteToolPendingStore {
    pub fn register(
        &self,
        request_id: String,
        source_connection_id: String,
        completion: oneshot::Sender<Result<RemoteToolOutput, AgentError>>,
    ) -> bool {
        let mut pending = self.inner.lock().expect("remote tool pending lock");
        if pending.contains_key(&request_id) {
            return false;
        }
        pending.insert(
            request_id,
            PendingRemoteTool {
                source_connection_id,
                reassembler: ByteReassembler::new(MAX_REMOTE_TOOL_RESULT_BYTES),
                completion,
            },
        );
        true
    }

    pub fn accept_chunk(
        &self,
        source_connection_id: &str,
        chunk: &RemoteToolResponseChunk,
    ) -> AcceptOutcome {
        let mut map = self.inner.lock().expect("remote tool pending lock");
        let push = {
            let Some(pending) = map.get_mut(&chunk.request_id) else {
                return AcceptOutcome::Rejected("no pending request");
            };
            if pending.source_connection_id != source_connection_id {
                return AcceptOutcome::Rejected("response source does not match target");
            }
            pending.reassembler.push(chunk)
        };
        if let Err(e) = push {
            if let Some(pending) = map.remove(&chunk.request_id) {
                let _ = pending.completion.send(Err(error(
                    AgentErrorKind::TransportError,
                    format!("remote tool chunk rejected: {e}"),
                    false,
                    true,
                )));
            }
            return AcceptOutcome::Delivered;
        }
        if !chunk.last {
            return AcceptOutcome::NeedMore;
        }
        let Some(pending) = map.remove(&chunk.request_id) else {
            return AcceptOutcome::Rejected("pending request vanished");
        };
        let result = pending
            .reassembler
            .finish()
            .map_err(|e| {
                error(
                    AgentErrorKind::TransportError,
                    format!("remote tool reassembly failed: {e}"),
                    false,
                    true,
                )
            })
            .and_then(|bytes| decode_output(&bytes));
        let _ = pending.completion.send(result);
        AcceptOutcome::Delivered
    }

    pub fn fail(
        &self,
        source_connection_id: &str,
        request_id: &str,
        remote_error: AgentError,
    ) -> AcceptOutcome {
        let mut map = self.inner.lock().expect("remote tool pending lock");
        let Some(pending) = map.get(request_id) else {
            return AcceptOutcome::Rejected("no pending request");
        };
        if pending.source_connection_id != source_connection_id {
            return AcceptOutcome::Rejected("error source does not match target");
        }
        let pending = map.remove(request_id).expect("pending checked above");
        let _ = pending.completion.send(Err(remote_error));
        AcceptOutcome::Delivered
    }

    pub fn cancel(&self, request_id: &str) {
        self.inner
            .lock()
            .expect("remote tool pending lock")
            .remove(request_id);
    }

    pub fn drain_for_connection(&self, connection_id: &str) -> usize {
        let mut map = self.inner.lock().expect("remote tool pending lock");
        let ids: Vec<_> = map
            .iter()
            .filter(|(_, pending)| pending.source_connection_id == connection_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            if let Some(pending) = map.remove(id) {
                let _ = pending.completion.send(Err(error(
                    AgentErrorKind::TargetOffline,
                    "the host disconnected during a Device Assistant observation",
                    true,
                    true,
                )));
            }
        }
        ids.len()
    }
}

pub fn global_remote_tool_pending() -> Arc<SignalRemoteToolPendingStore> {
    static STORE: OnceLock<Arc<SignalRemoteToolPendingStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(SignalRemoteToolPendingStore::default()))
        .clone()
}

struct PendingComputerAction {
    source_connection_id: String,
    completion: oneshot::Sender<Result<ComputerActionCompleted, AgentError>>,
}

#[derive(Default)]
pub struct SignalComputerActionPendingStore {
    inner: Mutex<HashMap<String, PendingComputerAction>>,
}

impl SignalComputerActionPendingStore {
    pub fn register(
        &self,
        generation: String,
        source_connection_id: String,
        completion: oneshot::Sender<Result<ComputerActionCompleted, AgentError>>,
    ) -> bool {
        self.inner
            .lock()
            .expect("computer action pending lock")
            .insert(
                generation,
                PendingComputerAction {
                    source_connection_id,
                    completion,
                },
            )
            .is_none()
    }

    pub fn note_started(
        &self,
        source_connection_id: &str,
        started: &ComputerActionStarted,
    ) -> bool {
        self.inner
            .lock()
            .expect("computer action pending lock")
            .get(&started.execution_generation)
            .is_some_and(|pending| pending.source_connection_id == source_connection_id)
    }

    pub fn complete(&self, source_connection_id: &str, completed: ComputerActionCompleted) -> bool {
        let mut pending = self.inner.lock().expect("computer action pending lock");
        let Some(entry) = pending.get(&completed.execution_generation) else {
            return false;
        };
        if entry.source_connection_id != source_connection_id {
            return false;
        }
        let entry = pending
            .remove(&completed.execution_generation)
            .expect("pending action checked above");
        let _ = entry.completion.send(Ok(completed));
        true
    }

    pub fn cancel(&self, generation: &str) {
        self.inner
            .lock()
            .expect("computer action pending lock")
            .remove(generation);
    }

    pub fn drain_for_connection(&self, connection_id: &str) -> usize {
        let mut pending = self.inner.lock().expect("computer action pending lock");
        let generations = pending
            .iter()
            .filter(|(_, entry)| entry.source_connection_id == connection_id)
            .map(|(generation, _)| generation.clone())
            .collect::<Vec<_>>();
        for generation in &generations {
            if let Some(entry) = pending.remove(generation) {
                let _ = entry.completion.send(Err(error(
                    AgentErrorKind::TargetOffline,
                    "the host disconnected during a Computer Action",
                    false,
                    true,
                )));
            }
        }
        generations.len()
    }
}

pub fn global_computer_action_pending() -> Arc<SignalComputerActionPendingStore> {
    static STORE: OnceLock<Arc<SignalComputerActionPendingStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(SignalComputerActionPendingStore::default()))
        .clone()
}

pub struct SignalComputerActionObserver {
    pending: Arc<SignalComputerActionPendingStore>,
    store: SignalCapabilityGrantStore,
}

impl SignalComputerActionObserver {
    pub fn new(pending: Arc<SignalComputerActionPendingStore>, db: DatabaseConnection) -> Self {
        Self {
            pending,
            store: SignalCapabilityGrantStore::new(db),
        }
    }
}

impl ComputerActionObserver for SignalComputerActionObserver {
    fn on_computer_action_lifecycle<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if source.auth_context.auth_kind
                != desk_signal_facade::model::auth_context::AuthKind::TokenAuth
                || source.auth_context.remote_desk_type
                    != desk_signal_facade::model::signal::RemoteDeskTypeEnum::Server
                || model.to_connection_id.is_some()
            {
                return;
            }
            let source_id = source.model.connection_id.as_str();
            match model.signaling_type {
                SignalingType::ComputerActionStateReported => {
                    if !model
                        .response_state
                        .as_ref()
                        .is_some_and(|state| state.is_success())
                    {
                        return;
                    }
                    if let (Some(audience), Ok(state)) = (
                        source.model.version_info.client_id.as_deref(),
                        model.get_data::<ComputerActionStateReport>(),
                    ) && self
                        .store
                        .accept_computer_cancel_state(
                            source_id,
                            audience,
                            &model.request_id,
                            &state,
                        )
                        .await
                        .is_err()
                    {
                        log::warn!("[computer-action] rejected inconsistent stop observation");
                    }
                }
                SignalingType::ComputerActionStarted => {
                    if let Ok(started) = model.get_data::<ComputerActionStarted>() {
                        let Some(audience) = source.model.version_info.client_id.as_deref() else {
                            return;
                        };
                        if self
                            .store
                            .accept_computer_started(
                                source_id,
                                audience,
                                &model.request_id,
                                &started,
                            )
                            .await
                            .is_err()
                        {
                            log::warn!(
                                "[computer-action] rejected inconsistent executor acceptance"
                            );
                            return;
                        }
                        let _ = self.pending.note_started(source_id, &started);
                    }
                }
                SignalingType::ComputerActionCompleted => {
                    if let Ok(completed) = model.get_data::<ComputerActionCompleted>() {
                        let Some(audience) = source.model.version_info.client_id.as_deref() else {
                            return;
                        };
                        use crate::capability_grant_store::computer_completion::CompletionObservation;
                        match self
                            .store
                            .accept_computer_completion(
                                source_id,
                                audience,
                                &model.request_id,
                                &completed,
                            )
                            .await
                        {
                            Ok(
                                CompletionObservation::Stored
                                | CompletionObservation::Unknown
                                | CompletionObservation::Duplicate
                                | CompletionObservation::InlineOrLegacy,
                            ) => {
                                let _ = self.pending.complete(source_id, completed);
                            }
                            Ok(CompletionObservation::Stale) => {}
                            Err(_) => {
                                log::warn!("[computer-action] rejected inconsistent completion")
                            }
                        }
                    }
                }
                _ => {}
            }
        })
    }
}

pub struct SignalRemoteToolObserver {
    pending: Arc<SignalRemoteToolPendingStore>,
}

impl SignalRemoteToolObserver {
    pub fn new(pending: Arc<SignalRemoteToolPendingStore>) -> Self {
        Self { pending }
    }
}

impl RemoteToolObserver for SignalRemoteToolObserver {
    fn on_remote_tool_response<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let response = match model.get_data::<RemoteToolResponse>() {
                Ok(response) => response,
                Err(e) => {
                    log::warn!("[device-assistant] malformed remote tool response: {e}");
                    return;
                }
            };
            let source_id = source.model.connection_id.as_str();
            let outcome = match response {
                RemoteToolResponse::Chunk(chunk) => self.pending.accept_chunk(source_id, &chunk),
                RemoteToolResponse::Error(remote_error) => {
                    self.pending
                        .fail(source_id, &remote_error.request_id, remote_error.error)
                }
            };
            if let AcceptOutcome::Rejected(reason) = outcome {
                log::warn!(
                    "[device-assistant] dropped remote tool response from {source_id}: {reason}"
                );
            }
        })
    }
}

/// Per-turn Provider seam for the OSS Device Assistant.
pub struct SignalDeviceAssistantTools {
    db: DatabaseConnection,
    provider_registry: ProviderRegistry,
    connections: Arc<SharedConnectionMap>,
    pending: Arc<SignalRemoteToolPendingStore>,
    target_connection_id: String,
    target_device_id: String,
    actor_id: String,
    model_provider: Option<String>,
    model_name: Option<String>,
    timeout: Duration,
    selected_office_document: Option<ObjectRef>,
    selected_live_spreadsheet: Option<ObjectRef>,
    selected_live_document: Option<ObjectRef>,
    selected_live_presentation: Option<ObjectRef>,
    selected_file_roots: Vec<ObjectRef>,
    selected_spreadsheet_roots: Vec<ObjectRef>,
    selected_terminal_roots: Vec<ObjectRef>,
    selected_browser_surface: Option<ObjectRef>,
    selected_outlook_surface: Option<ObjectRef>,
    current_user_message: String,
    run_id: String,
    turn_id: String,
    policy_revision: i64,
    readiness_revision: u64,
    max_command_runtime_ms: u32,
    exec_tools: crate::agent_exec::SignalAgentTools,
    model_egress_policy: Option<desk_diagnose_core::model_egress::ModelEgressPolicy>,
    original_input: OnceLock<object_read::OriginalInput>,
    verified_read_labels: Mutex<HashMap<String, VerifiedReadLabel>>,
}

#[derive(Clone)]
struct VerifiedReadLabel {
    digest_sha256: String,
    expires_at_unix_ms: u64,
    failed: bool,
}

struct ProviderInvokeError {
    error: AgentError,
    known_completion: Option<(CapabilityDispatchOutcome, String)>,
}

impl ProviderInvokeError {
    fn known(
        error: AgentError,
        outcome: CapabilityDispatchOutcome,
        result_digest_sha256: String,
    ) -> Self {
        Self {
            error,
            known_completion: Some((outcome, result_digest_sha256)),
        }
    }
}

impl From<AgentError> for ProviderInvokeError {
    fn from(error: AgentError) -> Self {
        Self {
            error,
            known_completion: None,
        }
    }
}

fn verify_read_label(
    output: &ToolRunOutput,
    verified: &VerifiedReadLabel,
    observed_at_unix_ms: u64,
) -> Result<(), AgentError> {
    let (_, digest_sha256) = tool_output_fingerprint(output)?;
    if digest_sha256 != verified.digest_sha256 || observed_at_unix_ms >= verified.expires_at_unix_ms
    {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "Provider result changed or expired before labeling",
            false,
            false,
        ));
    }
    Ok(())
}

fn exact_selected_batch_file(roots: &[ObjectRef]) -> Result<ObjectRef, AgentError> {
    let mut files = roots
        .iter()
        .filter(|object_ref| object_ref.object_kind == ObjectKind::File);
    let file = files.next().cloned().ok_or_else(|| {
        error(
            AgentErrorKind::PermissionDenied,
            "select exactly one native iWork file before BatchDocument inspection",
            false,
            true,
        )
    })?;
    if files.next().is_some() {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "select exactly one native iWork file before BatchDocument inspection",
            false,
            true,
        ));
    }
    Ok(file)
}

fn validate_selected_batch_destination(
    roots: &[ObjectRef],
    destination: &ObjectRef,
) -> Result<(), AgentError> {
    if destination.object_kind != ObjectKind::Directory {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "BatchDocument output requires an exact directory reference",
            false,
            true,
        ));
    }
    if !roots.contains(destination) {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "BatchDocument output directory was not selected by the owner for this turn",
            false,
            true,
        ));
    }
    Ok(())
}

fn semantic_action_target_kind(action: &ComputerActionKind) -> Option<ObjectKind> {
    match action {
        ComputerActionKind::Ui(_) => Some(ObjectKind::UiElement),
        ComputerActionKind::RawInput(_) => Some(ObjectKind::Application),
        ComputerActionKind::SpreadsheetLive(_) => Some(ObjectKind::Range),
        ComputerActionKind::DocumentLive(_) => Some(ObjectKind::Document),
        ComputerActionKind::PresentationLive(_) => Some(ObjectKind::Slide),
        ComputerActionKind::SpreadsheetLiveBatch(_)
        | ComputerActionKind::DocumentLiveBatch(_)
        | ComputerActionKind::PresentationLiveBatch(_) => Some(ObjectKind::File),
        _ => None,
    }
}

impl SignalDeviceAssistantTools {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: DatabaseConnection,
        provider_registry: ProviderRegistry,
        connections: Arc<SharedConnectionMap>,
        pending: Arc<SignalRemoteToolPendingStore>,
        target_connection_id: String,
        target_device_id: String,
        actor_id: String,
        model_provider: Option<String>,
        model_name: Option<String>,
        selected_office_document: Option<ObjectRef>,
        selected_live_spreadsheet: Option<ObjectRef>,
        selected_live_document: Option<ObjectRef>,
        selected_live_presentation: Option<ObjectRef>,
        selected_file_roots: Vec<ObjectRef>,
        selected_spreadsheet_roots: Vec<ObjectRef>,
        selected_terminal_roots: Vec<ObjectRef>,
        selected_browser_surface: Option<ObjectRef>,
        selected_outlook_surface: Option<ObjectRef>,
        current_user_message: String,
        run_id: String,
        turn_id: String,
        policy_revision: i64,
        readiness_revision: u64,
        available_exec_shells: Vec<String>,
        max_command_runtime_ms: u32,
    ) -> Self {
        let exec_tools = crate::agent_exec::SignalAgentTools::new(
            db.clone(),
            connections.clone(),
            crate::agent_exec::global_agent_exec_pending(),
            target_connection_id.clone(),
            run_id.clone(),
            desk_agent_protocol::evidence::EvidenceSnapshot::record(
                "device-assistant-command",
                "pre-approved safe-template command dispatch",
                chrono::Utc::now().to_rfc3339(),
                Vec::new(),
            ),
            ExecAdmissionPolicy::TemplateOnly,
            desk_agent_protocol::RiskLevel::High,
            available_exec_shells,
            max_command_runtime_ms,
        );
        Self {
            db,
            provider_registry,
            connections,
            pending,
            target_connection_id,
            target_device_id,
            actor_id,
            model_provider,
            model_name,
            timeout: Duration::from_secs(REMOTE_TOOL_TIMEOUT_SECS),
            selected_office_document,
            selected_live_spreadsheet,
            selected_live_document,
            selected_live_presentation,
            selected_file_roots,
            selected_spreadsheet_roots,
            selected_terminal_roots,
            selected_browser_surface,
            selected_outlook_surface,
            current_user_message,
            run_id,
            turn_id,
            policy_revision,
            readiness_revision,
            max_command_runtime_ms,
            exec_tools,
            model_egress_policy: None,
            original_input: OnceLock::new(),
            verified_read_labels: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn with_model_egress_policy(
        mut self,
        policy: Option<desk_diagnose_core::model_egress::ModelEgressPolicy>,
    ) -> Self {
        self.model_egress_policy = policy;
        self
    }

    fn canonical_call_input(call: &ToolCall) -> Result<(String, String), AgentError> {
        let value = serde_json::from_str(&call.arguments_json).map_err(|decode_error| {
            error(
                AgentErrorKind::InvalidInput,
                format!("invalid Provider tool input: {decode_error}"),
                false,
                true,
            )
        })?;
        let canonical =
            canonical_tool_permission_input_json(&call.name, value).map_err(|encode_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to canonicalize Provider tool input: {encode_error}"),
                    false,
                    false,
                )
            })?;
        let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        Ok((canonical, digest))
    }

    fn validate_batch_destination(&self, destination: &ObjectRef) -> Result<(), AgentError> {
        validate_selected_batch_destination(&self.selected_file_roots, destination)
    }

    fn preflight_selected_context(
        &self,
        call: &ToolCall,
        capability: desk_agent_protocol::Capability,
    ) -> Result<(), AgentError> {
        if call.name == PREVIEW_COMPUTER_ACTION_TOOL {
            validate_preview_call(call)?;
            return Ok(());
        }
        if capability == desk_agent_protocol::Capability::WebResearchFetch {
            validate_fetch_call(call, &self.current_user_message)?;
            return Ok(());
        }
        if capability == desk_agent_protocol::Capability::WebResearchSearch {
            validate_search_call(call, &self.current_user_message)?;
            return Ok(());
        }
        build_read_operation(call)?;
        if matches!(
            call.name.as_str(),
            "inspect_selected_numbers_with_iwork"
                | "inspect_selected_pages_with_iwork"
                | "inspect_selected_keynote_with_iwork"
        ) {
            exact_selected_batch_file(&self.selected_file_roots)?;
        }
        match capability {
            desk_agent_protocol::Capability::OfficeDocumentInspect
                if self.selected_office_document.is_none() =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no exact paired Excel document was selected for this turn",
                    false,
                    true,
                ))
            }
            desk_agent_protocol::Capability::SpreadsheetLiveInspect
                if call.name != "inspect_selected_numbers_with_iwork"
                    && self.selected_live_spreadsheet.is_none() =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no fresh Numbers selection was paired with this turn",
                    false,
                    true,
                ))
            }
            desk_agent_protocol::Capability::DocumentLiveInspect
                if call.name != "inspect_selected_pages_with_iwork"
                    && self.selected_live_document.is_none() =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no fresh Pages document was paired with this turn",
                    false,
                    true,
                ))
            }
            desk_agent_protocol::Capability::PresentationLiveInspect
                if call.name != "inspect_selected_keynote_with_iwork"
                    && self.selected_live_presentation.is_none() =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no fresh Keynote slide was paired with this turn",
                    false,
                    true,
                ))
            }
            desk_agent_protocol::Capability::FileMetadataRead
                if self.selected_file_roots.is_empty() =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no active file attachment was selected for this turn",
                    false,
                    true,
                ))
            }
            desk_agent_protocol::Capability::FileContentRead
                if self
                    .selected_file_roots
                    .iter()
                    .filter(|object_ref| object_ref.object_kind == ObjectKind::File)
                    .count()
                    != 1 =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "select exactly one regular file before reading its text",
                    false,
                    true,
                ))
            }
            desk_agent_protocol::Capability::SpreadsheetFileInspect
            | desk_agent_protocol::Capability::SpreadsheetMergePreview
                if self.selected_spreadsheet_roots.is_empty() =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "select at least one spreadsheet file or directory before inspecting it",
                    false,
                    true,
                ))
            }
            desk_agent_protocol::Capability::TerminalOutputRead
                if self.selected_terminal_roots.is_empty() =>
            {
                Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no active terminal output attachment was selected for this turn",
                    false,
                    true,
                ))
            }
            _ => Ok(()),
        }
    }

    async fn authoritative_session(
        &self,
    ) -> Result<desk_diagnose_core::session::PersistedAgentSession, AgentError> {
        desk_diagnose_core::assistant_policy::require_current_policy(self.policy_revision)?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&self.run_id))
            .one(&self.db)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load Provider authorization session: {db_error}"),
                    false,
                    false,
                )
            })?
            .ok_or_else(|| {
                error(
                    AgentErrorKind::PermissionDenied,
                    "Provider authorization session no longer exists",
                    false,
                    true,
                )
            })?;
        let session =
            desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|decode_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("invalid Provider authorization session: {decode_error}"),
                        false,
                        false,
                    )
                })?;
        if session.actor_id != self.actor_id
            || session.device_id != self.target_device_id
            || session.policy_revision != self.policy_revision
        {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Provider authorization subject or policy changed",
                false,
                true,
            ));
        }
        Ok(session)
    }

    fn capability_risk(
        capability: &desk_diagnose_core::provider_registry::CapabilityDescriptor,
        call: &ToolCall,
    ) -> Result<CapabilityRiskTier, AgentError> {
        desk_diagnose_core::provider_preflight::classify_provider_call(capability, call)
    }

    async fn verify_current_readiness(
        &self,
        capability: &desk_diagnose_core::provider_registry::CapabilityDescriptor,
    ) -> Result<(), AgentError> {
        if capability.wire.execution_locality == ExecutionLocality::Central {
            return Ok(());
        }
        let current = crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&self.target_connection_id, chrono::Utc::now())
            .ok_or_else(|| {
                error(
                    AgentErrorKind::TargetOffline,
                    "Provider readiness is no longer available",
                    true,
                    true,
                )
            })?;
        let ready = current.readiness.revision == self.readiness_revision
            && current.readiness.capabilities.iter().any(|item| {
                item.capability == capability.required_capability && item.supported && item.ready
            });
        if !ready {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Provider readiness changed; refresh capabilities before retrying",
                false,
                true,
            ));
        }
        Ok(())
    }

    async fn authorize_and_invoke(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        let capability = self
            .provider_registry
            .capability_for_tool(&call.name)
            .ok_or_else(|| {
                error(
                    AgentErrorKind::UnsupportedCapability,
                    "Provider tool is not registered",
                    false,
                    true,
                )
            })?;
        let provider = self
            .provider_registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered capability has a Provider");
        self.preflight_selected_context(call, capability.required_capability)?;
        self.verify_current_readiness(capability).await?;

        let session = self.authoritative_session().await?;
        let (canonical_input_json, canonical_input_digest_sha256) =
            Self::canonical_call_input(call)?;
        let server_call_id = format!(
            "capability-call-{:x}",
            Sha256::digest(format!("{}:{}:{}", self.run_id, self.turn_id, call.id).as_bytes())
        );
        let compiled_scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        );
        let read_preflight = if capability.wire.execution_locality == ExecutionLocality::Edge {
            self.validate_original_objects().await?;
            Some(
                desk_diagnose_core::provider_preflight::read::ReadCallPreflight::build(
                    &self.provider_registry,
                    ProductSurface::OssPersonalOwner,
                    call,
                    &self.object_binding()?,
                )?,
            )
        } else {
            None
        };
        let mut resource_scope = compiled_scope.as_ref().map_or_else(
            || vec!["target:current_device".to_string()],
            |scope| scope.resources.clone(),
        );
        if capability.wire.authorization_hint.resources == [AuthorizationResourceKind::ExternalUrl]
        {
            resource_scope = exact_external_url_resource_scope(&canonical_input_digest_sha256);
        }
        if capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::ExternalQuery]
        {
            resource_scope = exact_external_query_resource_scope(&canonical_input_digest_sha256);
        }
        if let Some(preflight) = &read_preflight {
            resource_scope = preflight.resource_scope().to_vec();
        }
        let operation_scope =
            compiled_scope.map_or_else(|| vec!["observe".to_string()], |scope| scope.operations);
        let risk_tier = Self::capability_risk(capability, call)?;
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "system clock predates the Unix epoch",
                false,
                false,
            )
        })?;
        let export_destinations = if capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::ExternalQuery]
        {
            vec![DestinationIdentity::WebResearch {
                connector_id: desk_diagnose_core::device_assistant::DUCKDUCKGO_HTML_CONNECTOR_ID
                    .into(),
            }]
        } else {
            Vec::new()
        };
        let call_authority = CapabilityGrantCall {
            actor_id: &self.actor_id,
            run_id: &self.run_id,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: &self.target_device_id,
            target_session_id: None,
            provider_id: &provider.wire.provider_id,
            capability_id: &capability.wire.capability_id,
            tool_name: &call.name,
            tool_schema_version: capability.wire.input_schema_version,
            effect: capability.wire.effect,
            risk_tier,
            resource_scope: &resource_scope,
            operation_scope: &operation_scope,
            export_destinations: &export_destinations,
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: &canonical_input_digest_sha256,
            byte_count: 0,
            item_count: read_preflight
                .as_ref()
                .map_or(1, |preflight| preflight.root_count()),
            policy_revision: self.policy_revision,
            readiness_revision: self.readiness_revision,
            now_unix_ms,
        };
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        let grant_id = if let Some(existing) = store
            .prepared_grant_id(&server_call_id)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load prepared Provider authority: {db_error}"),
                    false,
                    false,
                )
            })? {
            existing
        } else if let Some(grant) = store
            .list_for_subject(&self.run_id, &self.actor_id, &self.target_device_id)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load Provider grants: {db_error}"),
                    false,
                    false,
                )
            })?
            .into_iter()
            .find(|grant| match_capability_grant(grant, &call_authority).is_ok())
        {
            grant.grant_id
        } else if risk_tier == CapabilityRiskTier::R0 {
            let grant_id = format!(
                "policy-auto-{:x}",
                Sha256::digest(server_call_id.as_bytes())
            );
            let grant = CapabilityGrant {
                schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
                grant_id: grant_id.clone(),
                actor_id: self.actor_id.clone(),
                run_id: self.run_id.clone(),
                surface: ProductSurface::OssPersonalOwner,
                target_device_id: self.target_device_id.clone(),
                target_session_id: None,
                provider_id: provider.wire.provider_id.clone(),
                capability_id: capability.wire.capability_id.clone(),
                tool_name: call.name.clone(),
                tool_schema_version: capability.wire.input_schema_version,
                effect: capability.wire.effect,
                risk_tier,
                resource_scope: resource_scope.clone(),
                operation_scope: operation_scope.clone(),
                export_destinations: Vec::new(),
                allowed_envelope_ids: Vec::new(),
                allowed_content_digests_sha256: Vec::new(),
                use_policy: CapabilityGrantUsePolicy::Reusable,
                canonical_input_digest_sha256: Some(canonical_input_digest_sha256.clone()),
                issued_by: CapabilityGrantIssuer::PolicyAuto,
                issued_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: now_unix_ms.saturating_add(120_000).min(
                    read_preflight
                        .as_ref()
                        .map_or(u64::MAX, |preflight| preflight.valid_until_unix_ms()),
                ),
                remaining_uses: 1,
                limits: CapabilityGrantLimits {
                    max_bytes_per_call: capability.wire.limits.max_output_bytes,
                    max_items_per_call: capability.wire.limits.max_objects,
                    max_calls: 1,
                },
                policy_revision: self.policy_revision,
                readiness_revision: self.readiness_revision,
                revoked_at_unix_ms: None,
                revoked_reason: None,
            };
            store.issue(&grant).await.map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to issue policy-auto Provider grant: {db_error}"),
                    false,
                    false,
                )
            })?;
            grant_id
        } else {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "this Provider call requires an approved capability grant",
                false,
                true,
            ));
        };

        let prepare = |now_unix_ms| {
            let mut current = call_authority.clone();
            current.now_unix_ms = now_unix_ms;
            PrepareCapabilityCall {
                grant_id: &grant_id,
                call_id: &server_call_id,
                turn_id: &self.turn_id,
                input_revision: session.input_revision,
                input_watermark: session.latest_input_seq,
                generation: 1,
                canonical_input_json: &canonical_input_json,
                call: current,
            }
        };
        store
            .prepare(prepare(now_unix_ms))
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::PermissionDenied,
                    format!("Provider call authorization failed: {db_error}"),
                    false,
                    true,
                )
            })?;
        let dispatch_id = match store
            .record_dispatch_intent(prepare(now_unix_ms))
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::PermissionDenied,
                    format!("Provider dispatch authorization failed: {db_error}"),
                    false,
                    true,
                )
            })? {
            DispatchIntentResult::Recorded { dispatch_id, .. } => dispatch_id,
            DispatchIntentResult::SupersededBeforeIntent { .. } => {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "Provider call was superseded by newer user input",
                    false,
                    true,
                ));
            }
            DispatchIntentResult::RevokedBeforeIntent { .. } => {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "Provider grant was revoked before dispatch",
                    false,
                    true,
                ));
            }
        };
        match store
            .claim_dispatch(&dispatch_id, now_unix_ms)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to claim Provider dispatch: {db_error}"),
                    false,
                    false,
                )
            })? {
            DispatchClaimResult::Claimed(_) => {}
            DispatchClaimResult::OutcomeUnknown { .. } => {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "Provider dispatch outcome is unknown and cannot be retried automatically",
                    false,
                    true,
                ));
            }
        }

        let current_time = || {
            u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "system clock predates the Unix epoch",
                    false,
                    false,
                )
            })
        };
        let grant = store
            .validate_claimed_dispatch(&dispatch_id, prepare(current_time()?))
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::PermissionDenied,
                    format!("Provider dispatch authority is unavailable: {db_error}"),
                    false,
                    true,
                )
            })?;
        let authority_expiry = grant.expires_at_unix_ms.min(
            read_preflight
                .as_ref()
                .map_or(u64::MAX, |preflight| preflight.valid_until_unix_ms()),
        );

        let result: Result<ToolRunOutput, ProviderInvokeError> =
            if call.name == PREVIEW_COMPUTER_ACTION_TOOL {
                validate_preview_call(call)
                    .map(|content| ToolRunOutput {
                        content,
                        image_data_url: None,
                    })
                    .map_err(Into::into)
            } else {
                self.invoke(call, &grant, authority_expiry).await
            };
        match result {
            Ok(output) => {
                let post_now = current_time()?;
                let post_authority = async {
                    let post_grant = store
                        .validate_claimed_dispatch(&dispatch_id, prepare(post_now))
                        .await
                        .map_err(|db_error| {
                            error(
                                AgentErrorKind::PermissionDenied,
                                format!("Provider result authority is unavailable: {db_error}"),
                                false,
                                true,
                            )
                        })?;
                    if capability.wire.execution_locality == ExecutionLocality::Edge {
                        self.verify_current_readiness(capability).await?;
                        self.validate_original_objects().await?;
                    }
                    desk_diagnose_core::provider_preflight::read::limits::validate_output(
                        &self.provider_registry,
                        call,
                        &output,
                        &grant.limits,
                    )?;
                    Ok::<_, AgentError>(post_grant)
                }
                .await;
                let (_, result_digest_sha256) = tool_output_fingerprint(&output)?;
                let completion = CapabilityDispatchCompletion {
                    dispatch_id: dispatch_id.clone(),
                    call_id: server_call_id.clone(),
                    generation: 1,
                    outcome: CapabilityDispatchOutcome::Succeeded,
                    result_digest_sha256,
                };
                if let Err(db_error) = store
                    .record_dispatch_completion(&completion, current_time()?)
                    .await
                {
                    let _ = store
                        .mark_dispatch_outcome_unknown(
                            &dispatch_id,
                            &server_call_id,
                            1,
                            current_time()?,
                        )
                        .await;
                    return Err(error(
                        AgentErrorKind::Internal,
                        format!("Provider result could not be persisted safely: {db_error}"),
                        false,
                        false,
                    ));
                }
                let post_grant = post_authority?;
                if post_grant != grant || current_time()? >= authority_expiry {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "Provider result authority changed or expired",
                        false,
                        true,
                    ));
                }
                self.verified_read_labels
                    .lock()
                    .map_err(|_| {
                        error(
                            AgentErrorKind::Internal,
                            "Provider result label state is unavailable",
                            false,
                            false,
                        )
                    })?
                    .insert(
                        call.id.clone(),
                        VerifiedReadLabel {
                            digest_sha256: completion.result_digest_sha256,
                            expires_at_unix_ms: authority_expiry,
                            failed: false,
                        },
                    );
                Ok(output)
            }
            Err(provider_error) => {
                if let Some((outcome, result_digest_sha256)) = provider_error.known_completion {
                    let completion = CapabilityDispatchCompletion {
                        dispatch_id: dispatch_id.clone(),
                        call_id: server_call_id.clone(),
                        generation: 1,
                        outcome,
                        result_digest_sha256,
                    };
                    if let Err(db_error) = store
                        .record_dispatch_completion(&completion, current_time()?)
                        .await
                    {
                        let _ = store
                            .mark_dispatch_outcome_unknown(
                                &dispatch_id,
                                &server_call_id,
                                1,
                                current_time()?,
                            )
                            .await;
                        return Err(error(
                            AgentErrorKind::Internal,
                            format!("Provider result could not be persisted safely: {db_error}"),
                            false,
                            false,
                        ));
                    }
                    return Err(provider_error.error);
                }
                store
                    .mark_dispatch_outcome_unknown(
                        &dispatch_id,
                        &server_call_id,
                        1,
                        current_time()?,
                    )
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("Provider unknown outcome could not be persisted: {db_error}"),
                            false,
                            false,
                        )
                    })?;
                Err(provider_error.error)
            }
        }
    }

    async fn authorize_and_execute_command(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        if call.name != "execute_confirmed_command" {
            return Err(error(
                AgentErrorKind::UnsupportedCapability,
                "command Provider is not registered",
                false,
                true,
            ));
        }
        let command: CommandDraft =
            serde_json::from_str(&call.arguments_json).map_err(|decode_error| {
                error(
                    AgentErrorKind::InvalidInput,
                    format!("invalid confirmed command input: {decode_error}"),
                    false,
                    true,
                )
            })?;
        command.validate().map_err(|validation_error| {
            error(
                AgentErrorKind::InvalidInput,
                format!("invalid confirmed command input: {validation_error}"),
                false,
                true,
            )
        })?;
        let shell = desk_diagnose_core::exec_tools::canonical_exec_shell(&command.shell)
            .ok_or_else(|| {
                error(
                    AgentErrorKind::InvalidInput,
                    "confirmed command shell is not supported",
                    false,
                    true,
                )
            })?;
        let mut validation_input = desk_agent_protocol::ExecInput {
            target: desk_agent_protocol::ExecTarget::Shell {
                shell: shell.into(),
            },
            command: command.command,
            cwd: command.cwd,
            io_mode: desk_agent_protocol::exec::ExecIoMode::NonInteractive,
            timeout_ms: command.timeout_ms,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        };
        desk_diagnose_core::exec_tools::apply_exec_runtime_ceiling(
            &mut validation_input,
            self.max_command_runtime_ms,
        );
        let classified = desk_diagnose_core::exec_classify::classify_command_with_policy(
            &validation_input,
            &[],
            desk_agent_protocol::exec_policy::builtin_blocklist(),
            ExecAdmissionPolicy::TemplateOnly,
        );
        let Some(command_plan) = classified.draft else {
            return Ok(ExecOutcome::Rejected {
                reason: Some(classified.classification.impact),
            });
        };
        if classified.classification.decision
            != desk_agent_protocol::exec::ExecDecision::ConfirmRequired
            || command_plan.execution_basis
                != desk_agent_protocol::exec::ExecExecutionBasis::Template
        {
            return Ok(ExecOutcome::Rejected {
                reason: Some("the command does not match a safe template".into()),
            });
        }

        let capability = self
            .provider_registry
            .capability_for_tool(&call.name)
            .filter(|capability| {
                capability.required_capability
                    == desk_agent_protocol::Capability::ShellExecConfirmed
            })
            .ok_or_else(|| {
                error(
                    AgentErrorKind::UnsupportedCapability,
                    "command Provider is not registered",
                    false,
                    true,
                )
            })?;
        let provider = self
            .provider_registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered command capability has a Provider");
        self.verify_current_readiness(capability).await?;
        let session = self.authoritative_session().await?;
        let (canonical_input_json, canonical_input_digest_sha256) =
            Self::canonical_call_input(call)?;
        let identity = desk_agent_protocol::exec::CanonicalCommandIdentity {
            schema_version: desk_agent_protocol::exec::COMMAND_DRAFT_SCHEMA_VERSION,
            target_device_id: self.target_device_id.clone(),
            policy_revision: self.policy_revision,
            input_revision: session.input_revision,
            plan: command_plan,
            canonical_input_digest_sha256: canonical_input_digest_sha256.clone(),
        };
        identity.validate().map_err(|validation_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to freeze exact command identity: {validation_error}"),
                false,
                false,
            )
        })?;
        let resource_scope = exact_command_resource_scope(&canonical_input_digest_sha256);
        let operation_scope = vec!["execute_confirmed_command".into()];
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "system clock predates the Unix epoch",
                false,
                false,
            )
        })?;
        let server_call_id = format!(
            "capability-call-{:x}",
            Sha256::digest(format!("{}:{}:{}", self.run_id, self.turn_id, call.id).as_bytes())
        );
        let call_authority = CapabilityGrantCall {
            actor_id: &self.actor_id,
            run_id: &self.run_id,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: &self.target_device_id,
            target_session_id: None,
            provider_id: &provider.wire.provider_id,
            capability_id: &capability.wire.capability_id,
            tool_name: &call.name,
            tool_schema_version: capability.wire.input_schema_version,
            effect: capability.wire.effect,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: &resource_scope,
            operation_scope: &operation_scope,
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: &canonical_input_digest_sha256,
            byte_count: canonical_input_json.len() as u64,
            item_count: 1,
            policy_revision: self.policy_revision,
            readiness_revision: self.readiness_revision,
            now_unix_ms,
        };
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        let grant_id = if let Some(existing) = store
            .prepared_grant_id(&server_call_id)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load prepared command authority: {db_error}"),
                    false,
                    false,
                )
            })? {
            existing
        } else {
            store
                .list_for_subject(&self.run_id, &self.actor_id, &self.target_device_id)
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("failed to load command grants: {db_error}"),
                        false,
                        false,
                    )
                })?
                .into_iter()
                .find(|grant| match_capability_grant(grant, &call_authority).is_ok())
                .map(|grant| grant.grant_id)
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        "command execution requires an active one-shot exact grant",
                        false,
                        true,
                    )
                })?
        };
        let prepare = || PrepareCapabilityCall {
            grant_id: &grant_id,
            call_id: &server_call_id,
            turn_id: &self.turn_id,
            input_revision: session.input_revision,
            input_watermark: session.latest_input_seq,
            generation: 1,
            canonical_input_json: &canonical_input_json,
            call: call_authority.clone(),
        };
        let prepared = store.prepare(prepare()).await.map_err(|db_error| {
            error(
                AgentErrorKind::PermissionDenied,
                format!("command call authorization failed: {db_error}"),
                false,
                true,
            )
        })?;
        let dispatch_id =
            match store
                .record_dispatch_intent(prepare())
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        format!("command dispatch authorization failed: {db_error}"),
                        false,
                        true,
                    )
                })? {
                DispatchIntentResult::Recorded { dispatch_id, .. } => dispatch_id,
                DispatchIntentResult::SupersededBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "command call was superseded by newer user input",
                        false,
                        true,
                    ));
                }
                DispatchIntentResult::RevokedBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "command grant was revoked before dispatch",
                        false,
                        true,
                    ));
                }
            };
        match store
            .claim_dispatch(&dispatch_id, now_unix_ms)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to claim command dispatch: {db_error}"),
                    false,
                    false,
                )
            })? {
            DispatchClaimResult::Claimed(_) => {}
            DispatchClaimResult::OutcomeUnknown { .. } => {
                return Ok(ExecOutcome::Unknown(
                    desk_diagnose_core::session::ActionIdentity::new(
                        prepared.work_id,
                        server_call_id,
                        dispatch_id,
                        desk_diagnose_core::session::WorkKind::AgentExec,
                    ),
                ));
            }
        }

        let result = self
            .exec_tools
            .execute_preapproved(
                validation_input,
                ctx,
                ExecRequestId(server_call_id.clone()),
                dispatch_id.clone(),
                ApprovalId(grant_id),
            )
            .await;
        match result {
            Ok(ExecOutcome::Executed {
                output,
                event_id,
                data_envelope,
            }) => {
                let (_, result_digest_sha256) = tool_output_fingerprint(&output)?;
                store
                    .record_dispatch_completion(
                        &CapabilityDispatchCompletion {
                            dispatch_id,
                            call_id: server_call_id,
                            generation: 1,
                            outcome: CapabilityDispatchOutcome::Succeeded,
                            result_digest_sha256,
                        },
                        now_unix_ms,
                    )
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("failed to persist command completion: {db_error}"),
                            false,
                            false,
                        )
                    })?;
                Ok(ExecOutcome::Executed {
                    output,
                    event_id,
                    data_envelope,
                })
            }
            Ok(ExecOutcome::Rejected { reason }) => {
                let digest = format!(
                    "{:x}",
                    Sha256::digest(reason.as_deref().unwrap_or("command rejected").as_bytes())
                );
                store
                    .record_dispatch_completion(
                        &CapabilityDispatchCompletion {
                            dispatch_id,
                            call_id: server_call_id,
                            generation: 1,
                            outcome: CapabilityDispatchOutcome::Failed,
                            result_digest_sha256: digest,
                        },
                        now_unix_ms,
                    )
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("failed to persist command rejection: {db_error}"),
                            false,
                            false,
                        )
                    })?;
                Ok(ExecOutcome::Rejected { reason })
            }
            Ok(ExecOutcome::Unknown(identity)) => {
                store
                    .mark_dispatch_outcome_unknown(&dispatch_id, &server_call_id, 1, now_unix_ms)
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("failed to persist command unknown outcome: {db_error}"),
                            false,
                            false,
                        )
                    })?;
                Ok(ExecOutcome::Unknown(identity))
            }
            Ok(other @ ExecOutcome::Dispatched(_)) => Ok(other),
            Ok(other) => Ok(other),
            Err(exec_error) => {
                let digest = format!("{:x}", Sha256::digest(exec_error.message.as_bytes()));
                store
                    .record_dispatch_completion(
                        &CapabilityDispatchCompletion {
                            dispatch_id,
                            call_id: server_call_id,
                            generation: 1,
                            outcome: CapabilityDispatchOutcome::Failed,
                            result_digest_sha256: digest,
                        },
                        now_unix_ms,
                    )
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("failed to persist command failure: {db_error}"),
                            false,
                            false,
                        )
                    })?;
                Err(exec_error)
            }
        }
    }

    async fn authorize_and_execute_semantic_action(
        &self,
        call: &ToolCall,
    ) -> Result<ExecOutcome, AgentError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpreadsheetActionArgs {
            target: ObjectRef,
            action: SpreadsheetLivePatchAction,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct DocumentActionArgs {
            target: ObjectRef,
            text: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PresentationActionArgs {
            target: ObjectRef,
            action: PresentationLivePatchAction,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpreadsheetBatchActionArgs {
            target: ObjectRef,
            output: BatchDocumentOutput,
            action: SpreadsheetLivePatchAction,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct DocumentBatchActionArgs {
            target: ObjectRef,
            output: BatchDocumentOutput,
            text: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PresentationBatchActionArgs {
            target: ObjectRef,
            output: BatchDocumentOutput,
            action: PresentationLivePatchAction,
        }

        let decode = |decode_error| {
            error(
                AgentErrorKind::InvalidInput,
                format!("invalid bounded semantic action input: {decode_error}"),
                false,
                true,
            )
        };
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "system clock predates the Unix epoch",
                false,
                false,
            )
        })?;
        let shared_iwork =
            if desk_diagnose_core::provider_preflight::IworkCallPreflight::supports(&call.name) {
                self.validate_original_objects().await?;
                Some(
                    desk_diagnose_core::provider_preflight::IworkCallPreflight::build(
                        &self.provider_registry,
                        ProductSurface::OssPersonalOwner,
                        call,
                        self.object_binding()?.original,
                        now_unix_ms,
                    )?,
                )
            } else {
                None
            };
        let shared_raw_input = if call.name == EXECUTE_CONFIRMED_RAW_INPUT_TOOL {
            Some(
                desk_diagnose_core::provider_preflight::RawInputCallPreflight::build(
                    &self.provider_registry,
                    ProductSurface::OssPersonalOwner,
                    call,
                    now_unix_ms,
                )?,
            )
        } else {
            None
        };
        let (
            target_ref,
            authority_refs,
            computer_action,
            required_capability,
            adapter_kind,
            action_name,
        ) = match call.name.as_str() {
            EXECUTE_CONFIRMED_UI_ACTION_TOOL => {
                let (target, action) =
                    desk_diagnose_core::provider_preflight::ui_action_from_call(call)?;
                let action_name = match &action {
                    UiSemanticAction::Invoke => "invoke",
                    UiSemanticAction::Select => "select",
                    UiSemanticAction::Focus => "focus",
                    UiSemanticAction::SetValue { .. } => "set value",
                    UiSemanticAction::Toggle { .. } => "toggle",
                    UiSemanticAction::Scroll { .. } => unreachable!(),
                };
                #[cfg(windows)]
                let ui_adapter_kind = ComputerUseAdapterKind::WindowsUia;
                #[cfg(target_os = "macos")]
                let ui_adapter_kind = ComputerUseAdapterKind::MacosAccessibility;
                #[cfg(not(any(windows, target_os = "macos")))]
                let ui_adapter_kind = ComputerUseAdapterKind::WindowsUia;
                (
                    target.clone(),
                    vec![target],
                    ComputerActionKind::Ui(action),
                    desk_agent_protocol::Capability::DesktopUiActionConfirmed,
                    ui_adapter_kind,
                    action_name,
                )
            }
            EXECUTE_CONFIRMED_RAW_INPUT_TOOL => {
                let input = shared_raw_input.as_ref().expect("raw-input preflight");
                (
                    input.target().clone(),
                    vec![input.target().clone()],
                    ComputerActionKind::RawInput(input.action().clone()),
                    input.required_capability(),
                    ComputerUseAdapterKind::WindowsRawInput,
                    "single raw-input fallback",
                )
            }
            "patch_live_spreadsheet_cell" => {
                let args: SpreadsheetActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(decode)?;
                if shared_iwork.as_ref().map(|preflight| preflight.target()) != Some(&args.target) {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "spreadsheet patch requires the exact current Numbers cell reference",
                        false,
                        true,
                    ));
                }
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::SpreadsheetLive(args.action),
                    desk_agent_protocol::Capability::SpreadsheetLivePatchConfirmed,
                    ComputerUseAdapterKind::IworkNumbers,
                    "spreadsheet cell patch",
                )
            }
            "replace_live_document_body" => {
                let args: DocumentActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(decode)?;
                if shared_iwork.as_ref().map(|preflight| preflight.target()) != Some(&args.target) {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "document patch requires the exact current Pages document reference",
                        false,
                        true,
                    ));
                }
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::DocumentLive(DocumentLivePatchAction::ReplaceBodyText {
                        text: args.text,
                    }),
                    desk_agent_protocol::Capability::DocumentLivePatchConfirmed,
                    ComputerUseAdapterKind::IworkPages,
                    "document body replacement",
                )
            }
            "patch_live_presentation_slide" => {
                let args: PresentationActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(decode)?;
                if shared_iwork.as_ref().map(|preflight| preflight.target()) != Some(&args.target) {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "presentation patch requires the exact current Keynote slide reference",
                        false,
                        true,
                    ));
                }
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::PresentationLive(args.action),
                    desk_agent_protocol::Capability::PresentationLivePatchConfirmed,
                    ComputerUseAdapterKind::IworkKeynote,
                    "presentation slide patch",
                )
            }
            "patch_selected_numbers_copy" => {
                let args: SpreadsheetBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(decode)?;
                self.validate_batch_destination(&args.output.destination_parent)?;
                let authority_refs =
                    vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    authority_refs,
                    ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
                        output: args.output,
                        action: args.action,
                    }),
                    desk_agent_protocol::Capability::SpreadsheetLivePatchConfirmed,
                    ComputerUseAdapterKind::IworkNumbers,
                    "selected Numbers copy patch",
                )
            }
            "replace_selected_pages_copy_body" => {
                let args: DocumentBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(decode)?;
                self.validate_batch_destination(&args.output.destination_parent)?;
                let authority_refs =
                    vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    authority_refs,
                    ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
                        output: args.output,
                        action: DocumentLivePatchAction::ReplaceBodyText { text: args.text },
                    }),
                    desk_agent_protocol::Capability::DocumentLivePatchConfirmed,
                    ComputerUseAdapterKind::IworkPages,
                    "selected Pages copy body replacement",
                )
            }
            "patch_selected_keynote_copy" => {
                let args: PresentationBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(decode)?;
                self.validate_batch_destination(&args.output.destination_parent)?;
                let authority_refs =
                    vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    authority_refs,
                    ComputerActionKind::PresentationLiveBatch(PresentationLiveBatchPatchAction {
                        output: args.output,
                        action: args.action,
                    }),
                    desk_agent_protocol::Capability::PresentationLivePatchConfirmed,
                    ComputerUseAdapterKind::IworkKeynote,
                    "selected Keynote copy patch",
                )
            }
            _ => {
                return Err(error(
                    AgentErrorKind::UnsupportedCapability,
                    "bounded semantic action Provider is not registered",
                    false,
                    true,
                ));
            }
        };
        if semantic_action_target_kind(&computer_action) != Some(target_ref.object_kind) {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "semantic action target kind does not match the selected Provider",
                false,
                true,
            ));
        }
        if shared_iwork.as_ref().is_some_and(|preflight| {
            preflight.target() != &target_ref
                || preflight.action() != &computer_action
                || preflight.required_capability() != required_capability
                || preflight.adapter_kind() != adapter_kind
        }) {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "iWork action no longer matches the original selected object",
                false,
                true,
            ));
        }

        let capability = self
            .provider_registry
            .capability_for_tool(&call.name)
            .filter(|capability| capability.required_capability == required_capability)
            .ok_or_else(|| {
                error(
                    AgentErrorKind::UnsupportedCapability,
                    "bounded semantic action Provider is not registered",
                    false,
                    true,
                )
            })?;
        let provider = self
            .provider_registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered semantic UI action capability has a Provider");
        self.verify_current_readiness(capability).await?;
        let session = self.authoritative_session().await?;
        let (canonical_input_json, canonical_input_digest_sha256) =
            Self::canonical_call_input(call)?;
        let resource_scope = shared_iwork.as_ref().map_or_else(
            || fresh_object_resource_scope(&authority_refs),
            |preflight| preflight.resource_scope().to_vec(),
        );
        let operation_scope = vec!["use_selected_object".to_string()];
        let risk_tier = Self::capability_risk(capability, call)?;
        let server_call_id = format!(
            "capability-call-{:x}",
            Sha256::digest(format!("{}:{}:{}", self.run_id, self.turn_id, call.id).as_bytes())
        );
        let subject = desk_diagnose_core::provider_preflight::ProviderCallSubject {
            actor_id: &self.actor_id,
            run_id: &self.run_id,
            target_device_id: &self.target_device_id,
            policy_revision: self.policy_revision,
            readiness_revision: self.readiness_revision,
            now_unix_ms,
        };
        let call_authority = if let Some(preflight) = &shared_iwork {
            preflight.grant_call(&subject)?
        } else {
            CapabilityGrantCall {
                actor_id: &self.actor_id,
                run_id: &self.run_id,
                surface: ProductSurface::OssPersonalOwner,
                target_device_id: &self.target_device_id,
                target_session_id: None,
                provider_id: &provider.wire.provider_id,
                capability_id: &capability.wire.capability_id,
                tool_name: &call.name,
                tool_schema_version: capability.wire.input_schema_version,
                effect: capability.wire.effect,
                risk_tier,
                resource_scope: &resource_scope,
                operation_scope: &operation_scope,
                export_destinations: &[],
                envelope_ids: &[],
                content_digests_sha256: &[],
                canonical_input_digest_sha256: &canonical_input_digest_sha256,
                byte_count: canonical_input_json.len() as u64,
                item_count: 1,
                policy_revision: self.policy_revision,
                readiness_revision: self.readiness_revision,
                now_unix_ms,
            }
        };
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        let grant_id = if let Some(existing) = store
            .prepared_grant_id(&server_call_id)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load prepared semantic UI authority: {db_error}"),
                    false,
                    false,
                )
            })? {
            existing
        } else {
            store
                .list_for_subject(&self.run_id, &self.actor_id, &self.target_device_id)
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("failed to load semantic UI grants: {db_error}"),
                        false,
                        false,
                    )
                })?
                .into_iter()
                .find(|grant| match_capability_grant(grant, &call_authority).is_ok())
                .map(|grant| grant.grant_id)
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        "semantic UI action requires an active exact approved grant",
                        false,
                        true,
                    )
                })?
        };
        let prepare = || PrepareCapabilityCall {
            grant_id: &grant_id,
            call_id: &server_call_id,
            turn_id: &self.turn_id,
            input_revision: session.input_revision,
            input_watermark: session.latest_input_seq,
            generation: 1,
            canonical_input_json: &canonical_input_json,
            call: call_authority.clone(),
        };
        let prepared = store.prepare(prepare()).await.map_err(|db_error| {
            error(
                AgentErrorKind::PermissionDenied,
                format!("semantic UI call authorization failed: {db_error}"),
                false,
                true,
            )
        })?;
        let dispatch_id =
            match store
                .record_dispatch_intent(prepare())
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        format!("semantic UI dispatch authorization failed: {db_error}"),
                        false,
                        true,
                    )
                })? {
                DispatchIntentResult::Recorded { dispatch_id, .. } => dispatch_id,
                DispatchIntentResult::SupersededBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "semantic UI action was superseded by newer user input",
                        false,
                        true,
                    ));
                }
                DispatchIntentResult::RevokedBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "semantic UI grant was revoked before dispatch",
                        false,
                        true,
                    ));
                }
            };
        let claimed = match store
            .claim_dispatch(&dispatch_id, now_unix_ms)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to claim semantic UI dispatch: {db_error}"),
                    false,
                    false,
                )
            })? {
            DispatchClaimResult::Claimed(payload) => payload,
            DispatchClaimResult::OutcomeUnknown { .. } => {
                return Ok(ExecOutcome::Unknown(
                    desk_diagnose_core::session::ActionIdentity::new(
                        prepared.work_id,
                        server_call_id,
                        dispatch_id,
                        desk_diagnose_core::session::WorkKind::ComputerAction,
                    ),
                ));
            }
        };

        let dispatch_material = async {
            let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&self.target_connection_id, chrono::Utc::now())
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::TargetOffline,
                        "semantic UI readiness expired before dispatch",
                        false,
                        true,
                    )
                })?;
            let adapter = readiness
                .readiness
                .capabilities
                .iter()
                .find(|item| item.capability == required_capability && item.supported && item.ready)
                .map(|item| item.adapter.clone())
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        "semantic UI adapter is no longer ready",
                        false,
                        true,
                    )
                })?;
            if adapter.kind != adapter_kind {
                return Err(error(
                    AgentErrorKind::UnsupportedCapability,
                    "the ready adapter does not match the selected semantic Provider",
                    false,
                    true,
                ));
            }
            let generation = dispatch_id.clone();
            let raw_input = required_capability
                == desk_agent_protocol::Capability::DesktopInputFallbackConfirmed;
            let plan = SealedComputerActionPlan {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                work_id: claimed.work_id.to_string(),
                action_request_id: server_call_id.clone(),
                execution_generation: generation.clone(),
                device_id: self.target_device_id.clone(),
                interactive_session_incarnation: readiness
                    .readiness
                    .interactive_session_incarnation,
                adapter,
                approval_id: grant_id.clone(),
                approved_actor_id: self.actor_id.clone(),
                draft_hash: canonical_input_digest_sha256,
                expires_at: (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                timeout_ms: 30_000,
                actions: vec![ComputerActionStep {
                    target: target_ref,
                    action: computer_action,
                    before_summary: "fresh exact object resolved from the inspected snapshot"
                        .into(),
                    after_intent: if raw_input {
                        format!("perform one bounded {action_name} action")
                    } else {
                        format!("perform one bounded semantic {action_name} action")
                    },
                    verification: if raw_input {
                        "re-observe foreground application and display/DPI, then require a later semantic or screen observation before completion"
                            .into()
                    } else {
                        "re-locate the exact object and independently read back semantic state"
                            .into()
                    },
                }],
            };
            plan.validate().map_err(|validation_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to seal semantic UI plan: {validation_error}"),
                    false,
                    false,
                )
            })?;
            let target = {
                let map = self.connections.read().await;
                map.get(&self.target_connection_id).cloned()
            }
            .ok_or_else(|| {
                error(
                    AgentErrorKind::TargetOffline,
                    "target host is not connected",
                    false,
                    true,
                )
            })?;
            let audience = target.model.version_info.client_id.clone().ok_or_else(|| {
                error(
                    AgentErrorKind::PermissionDenied,
                    "target host has no bound client id",
                    false,
                    false,
                )
            })?;
            if audience != self.target_device_id {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "target device binding changed before semantic UI dispatch",
                    false,
                    false,
                ));
            }
            let authz = AuthorizationBlock {
                version: AUTHORIZATION_BLOCK_VERSION,
                exec_admission_policy: ExecAdmissionPolicy::OwnerInteractive,
                scope: AgentScope {
                    granted: vec![required_capability],
                    mode: ExecutionMode::ConfirmEachAction,
                    expires_at: None,
                    policy_name: Some("oss-device-assistant-semantic-action".into()),
                },
                orchestrator_grants: vec![capability.wire.capability_id.clone()],
                max_risk: desk_agent_protocol::RiskLevel::Medium,
                actor: AuthzActor {
                    user_id: self.actor_id.parse().ok(),
                },
                device: AuthzDevice { device_id: None },
                request_id: generation.clone(),
                session_id: None,
                expires_at: Some((chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()),
                issuer: "signal".into(),
                audience,
                signature: None,
            };
            let wrapper = AuthorizedControlPayload {
                inner: serde_json::to_value(&plan).map_err(|encode_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("failed to encode semantic UI plan: {encode_error}"),
                        false,
                        false,
                    )
                })?,
                authz,
            };
            let mut frame = SignalingModel::new_request(
                SignalingType::DispatchComputerAction,
                None,
                Some(&wrapper),
            )
            .map_err(|frame_error| {
                error(
                    AgentErrorKind::TransportError,
                    format!("failed to build semantic UI frame: {frame_error}"),
                    false,
                    false,
                )
            })?;
            frame.request_id = generation.clone();
            let text = serde_json::to_string(&frame).map_err(|encode_error| {
                error(
                    AgentErrorKind::TransportError,
                    format!("failed to encode semantic UI frame: {encode_error}"),
                    false,
                    false,
                )
            })?;
            store.bind_computer_transport(&self.target_connection_id, &plan, &session, call, self.model_egress_policy.as_ref())
                .await.map_err(|_| error(AgentErrorKind::Internal,
                    "failed to freeze original Computer Action transport", false, false))?;
            Ok::<_, AgentError>((target, generation, text, plan))
        }
        .await;
        let (target, generation, text, plan) = match dispatch_material {
            Ok(material) => material,
            Err(dispatch_error) => {
                record_computer_action_pre_send_failure(
                    &store,
                    &dispatch_id,
                    &server_call_id,
                    now_unix_ms,
                    &dispatch_error,
                )
                .await?;
                return Err(dispatch_error);
            }
        };
        let (completion_tx, completion_rx) = oneshot::channel();
        if !global_computer_action_pending().register(
            generation.clone(),
            self.target_connection_id.clone(),
            completion_tx,
        ) {
            let dispatch_error = error(
                AgentErrorKind::Internal,
                "duplicate semantic UI execution generation",
                false,
                false,
            );
            record_computer_action_pre_send_failure(
                &store,
                &dispatch_id,
                &server_call_id,
                now_unix_ms,
                &dispatch_error,
            )
            .await?;
            return Err(dispatch_error);
        }
        let sent = target.session.write().await.text(text).await.is_ok();
        self.finish_computer_action(&store, call, &plan, completion_rx, sent, true)
            .await
    }

    async fn authorize_and_execute_artifact(
        &self,
        call: &ToolCall,
    ) -> Result<ExecOutcome, AgentError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TextArgs {
            file_name: String,
            content_utf8: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpreadsheetArgs {
            preview_id: String,
            file_name: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpreadsheetFormulaArgs {
            preview_id: String,
            file_name: String,
            target_cell: String,
            formula: String,
            locale: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WordArgs {
            preview_id: String,
            file_name: String,
            title: String,
            #[serde(default)]
            web_search_call_id: Option<String>,
            #[serde(default)]
            web_sources: Vec<desk_agent_protocol::computer_use::WordReportWebSource>,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LocalDraftArgs {
            file_name: String,
            draft: desk_agent_protocol::communication::LocalDraftDocument,
        }
        enum ArtifactRequest {
            Text(TextArgs),
            Spreadsheet(SpreadsheetArgs),
            SpreadsheetFormula {
                args: SpreadsheetFormulaArgs,
                policy_digest_sha256: String,
            },
            Word(WordArgs),
            LocalDraft(LocalDraftArgs),
        }

        let (args, required_capability, _operation, orchestrator_grant) = match call.name.as_str() {
            "create_text_artifact_in_selected_directory" => (
                ArtifactRequest::Text(serde_json::from_str(&call.arguments_json).map_err(
                    |decode_error| {
                        error(
                            AgentErrorKind::InvalidInput,
                            format!("invalid text artifact Provider input: {decode_error}"),
                            false,
                            true,
                        )
                    },
                )?),
                desk_agent_protocol::Capability::FileArtifactCreateConfirmed,
                "create_new_artifact",
                desk_diagnose_core::device_assistant::FILE_ARTIFACT_CREATE_CAPABILITY_ID,
            ),
            "create_local_communication_draft" => {
                let args: LocalDraftArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|decode_error| {
                        error(
                            AgentErrorKind::InvalidInput,
                            format!("invalid local communication draft input: {decode_error}"),
                            false,
                            true,
                        )
                    })?;
                args.draft.validate().map_err(|validation_error| {
                    error(
                        AgentErrorKind::InvalidInput,
                        format!("invalid local communication draft: {validation_error}"),
                        false,
                        true,
                    )
                })?;
                (
                    ArtifactRequest::LocalDraft(args),
                    desk_agent_protocol::Capability::CommunicationLocalDraftCreateConfirmed,
                    "create_new_artifact",
                    desk_diagnose_core::device_assistant::LOCAL_COMMUNICATION_DRAFT_CREATE_CAPABILITY_ID,
                )
            }
            "create_workbook_from_merge_preview" => (
                ArtifactRequest::Spreadsheet(serde_json::from_str(&call.arguments_json).map_err(
                    |decode_error| {
                        error(
                            AgentErrorKind::InvalidInput,
                            format!("invalid spreadsheet artifact Provider input: {decode_error}"),
                            false,
                            true,
                        )
                    },
                )?),
                desk_agent_protocol::Capability::SpreadsheetWorkbookCreateConfirmed,
                "create_new_artifact",
                desk_diagnose_core::device_assistant::SPREADSHEET_WORKBOOK_CREATE_CAPABILITY_ID,
            ),
            "create_formula_workbook_from_merge_preview" => {
                let args: SpreadsheetFormulaArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|decode_error| {
                        error(
                            AgentErrorKind::InvalidInput,
                            format!(
                                "invalid spreadsheet formula artifact Provider input: {decode_error}"
                            ),
                            false,
                            true,
                        )
                    })?;
                let validated = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
                    &args.formula,
                    &args.target_cell,
                    &args.locale,
                    &["Merged".into(), "Statistics".into()],
                )
                .map_err(|validation_error| {
                    error(
                        AgentErrorKind::InvalidInput,
                        validation_error.message(),
                        false,
                        true,
                    )
                })?;
                if !matches!(
                    &validated.target,
                    desk_diagnose_core::spreadsheet_formula::FormulaExpr::Cell { reference }
                        if reference.sheet.as_deref() == Some("Merged")
                ) {
                    return Err(error(
                        AgentErrorKind::InvalidInput,
                        "batch formula target must be one explicit cell on the Merged sheet",
                        false,
                        true,
                    ));
                }
                (
                    ArtifactRequest::SpreadsheetFormula {
                        args,
                        policy_digest_sha256: validated.ast_digest_sha256,
                    },
                    desk_agent_protocol::Capability::SpreadsheetFormulaWorkbookCreateConfirmed,
                    "create_new_artifact",
                    desk_diagnose_core::device_assistant::SPREADSHEET_FORMULA_WORKBOOK_CREATE_CAPABILITY_ID,
                )
            }
            "create_word_report_from_merge_preview" => {
                let args: WordArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|decode_error| {
                        error(
                            AgentErrorKind::InvalidInput,
                            format!("invalid Word report artifact Provider input: {decode_error}"),
                            false,
                            true,
                        )
                    })?;
                if args.web_search_call_id.is_some() != !args.web_sources.is_empty()
                    || args.web_sources.len() > 8
                {
                    return Err(error(
                        AgentErrorKind::InvalidInput,
                        "Word report Web sources require one prior Web Search call id and 1 to 8 exact source entries",
                        false,
                        true,
                    ));
                }
                (
                    ArtifactRequest::Word(args),
                    desk_agent_protocol::Capability::WordDocumentCreateConfirmed,
                    "create_new_artifact",
                    desk_diagnose_core::device_assistant::WORD_DOCUMENT_CREATE_CAPABILITY_ID,
                )
            }
            _ => {
                return Err(error(
                    AgentErrorKind::UnsupportedCapability,
                    "artifact Provider is not registered",
                    false,
                    true,
                ));
            }
        };
        let selected_directories = self
            .selected_file_roots
            .iter()
            .filter(|object_ref| object_ref.object_kind == ObjectKind::Directory)
            .cloned()
            .collect::<Vec<_>>();
        if selected_directories.len() != 1 {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "artifact creation requires exactly one active directory selection",
                false,
                true,
            ));
        }
        let artifact_preflight =
            desk_diagnose_core::provider_preflight::ArtifactCallPreflight::build(
                &self.provider_registry,
                ProductSurface::OssPersonalOwner,
                call,
                &self.selected_file_roots,
                u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
                    error(
                        AgentErrorKind::Internal,
                        "system clock predates the Unix epoch",
                        false,
                        false,
                    )
                })?,
            )?;
        if artifact_preflight.required_capability() != required_capability
            || artifact_preflight.target() != &selected_directories[0]
        {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "artifact Provider authority does not match the selected directory",
                false,
                true,
            ));
        }
        let capability = self
            .provider_registry
            .capability_for_tool(&call.name)
            .filter(|capability| capability.required_capability == required_capability)
            .ok_or_else(|| {
                error(
                    AgentErrorKind::UnsupportedCapability,
                    "artifact Provider is not registered",
                    false,
                    true,
                )
            })?;
        self.verify_current_readiness(capability).await?;
        let session = self.authoritative_session().await?;
        let (canonical_input_json, canonical_input_digest_sha256) =
            Self::canonical_call_input(call)?;
        if canonical_input_json != artifact_preflight.canonical_input_json() {
            return Err(error(
                AgentErrorKind::Internal,
                "artifact Provider canonical inputs diverged",
                false,
                false,
            ));
        }
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "system clock predates the Unix epoch",
                false,
                false,
            )
        })?;
        let server_call_id = format!(
            "capability-call-{:x}",
            Sha256::digest(format!("{}:{}:{}", self.run_id, self.turn_id, call.id).as_bytes())
        );
        let subject = desk_diagnose_core::provider_preflight::ProviderCallSubject {
            actor_id: &self.actor_id,
            run_id: &self.run_id,
            target_device_id: &self.target_device_id,
            policy_revision: self.policy_revision,
            readiness_revision: self.readiness_revision,
            now_unix_ms,
        };
        let call_authority = artifact_preflight.grant_call(&subject)?;
        if call_authority.canonical_input_digest_sha256 != canonical_input_digest_sha256 {
            return Err(error(
                AgentErrorKind::Internal,
                "artifact Provider canonical digests diverged",
                false,
                false,
            ));
        }
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        let grant_id = if let Some(existing) = store
            .prepared_grant_id(&server_call_id)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load prepared artifact authority: {db_error}"),
                    false,
                    false,
                )
            })? {
            existing
        } else {
            store
                .list_for_subject(&self.run_id, &self.actor_id, &self.target_device_id)
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("failed to load artifact grants: {db_error}"),
                        false,
                        false,
                    )
                })?
                .into_iter()
                .find(|grant| match_capability_grant(grant, &call_authority).is_ok())
                .map(|grant| grant.grant_id)
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        "artifact creation requires an active approved capability grant",
                        false,
                        true,
                    )
                })?
        };
        let prepare = || PrepareCapabilityCall {
            grant_id: &grant_id,
            call_id: &server_call_id,
            turn_id: &self.turn_id,
            input_revision: session.input_revision,
            input_watermark: session.latest_input_seq,
            generation: 1,
            canonical_input_json: &canonical_input_json,
            call: call_authority.clone(),
        };
        let prepared = store.prepare(prepare()).await.map_err(|db_error| {
            error(
                AgentErrorKind::PermissionDenied,
                format!("artifact call authorization failed: {db_error}"),
                false,
                true,
            )
        })?;
        let dispatch_id =
            match store
                .record_dispatch_intent(prepare())
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        format!("artifact dispatch authorization failed: {db_error}"),
                        false,
                        true,
                    )
                })? {
                DispatchIntentResult::Recorded { dispatch_id, .. } => dispatch_id,
                DispatchIntentResult::SupersededBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "artifact call was superseded by newer user input",
                        false,
                        true,
                    ));
                }
                DispatchIntentResult::RevokedBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "artifact grant was revoked before dispatch",
                        false,
                        true,
                    ));
                }
            };
        let claimed = match store
            .claim_dispatch(&dispatch_id, now_unix_ms)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to claim artifact dispatch: {db_error}"),
                    false,
                    false,
                )
            })? {
            DispatchClaimResult::Claimed(payload) => payload,
            DispatchClaimResult::OutcomeUnknown { .. } => {
                return Ok(ExecOutcome::Unknown(
                    desk_diagnose_core::session::ActionIdentity::new(
                        prepared.work_id,
                        server_call_id,
                        dispatch_id,
                        desk_diagnose_core::session::WorkKind::ComputerAction,
                    ),
                ));
            }
        };

        macro_rules! artifact_pre_send {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(dispatch_error) => {
                        record_computer_action_pre_send_failure(
                            &store,
                            &dispatch_id,
                            &server_call_id,
                            now_unix_ms,
                            &dispatch_error,
                        )
                        .await?;
                        return Err(dispatch_error);
                    }
                }
            };
        }

        let readiness = artifact_pre_send!(
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&self.target_connection_id, chrono::Utc::now())
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::TargetOffline,
                        "artifact readiness expired before dispatch",
                        false,
                        true,
                    )
                })
        );
        let generation = dispatch_id.clone();
        let (decoded_action, before_summary, after_intent, verification) = match args {
            ArtifactRequest::Text(args) => (
                ComputerActionKind::File(FilePatchAction::CreateTextArtifact {
                    file_name: args.file_name,
                    content_utf8: args.content_utf8,
                }),
                "new artifact does not exist in the selected directory".into(),
                "create one new UTF-8 artifact without overwrite".into(),
                "reopen through the retained parent handle and verify exact bytes plus SHA-256"
                    .into(),
            ),
            ArtifactRequest::Spreadsheet(args) => (
                ComputerActionKind::File(FilePatchAction::CreateSpreadsheetArtifact {
                    preview_id: args.preview_id,
                    file_name: args.file_name,
                }),
                "new workbook does not exist in the selected directory".into(),
                "materialize the retained merge preview as one new formula-free XLSX without overwrite".into(),
                "reopen through the retained parent handle and verify the exact generated XLSX bytes plus SHA-256".into(),
            ),
            ArtifactRequest::SpreadsheetFormula {
                args,
                policy_digest_sha256,
            } => (
                ComputerActionKind::File(FilePatchAction::CreateSpreadsheetFormulaArtifact {
                    preview_id: args.preview_id,
                    file_name: args.file_name,
                    target_cell: args.target_cell,
                    formula: args.formula,
                    locale: args.locale,
                    formula_policy_digest_sha256: policy_digest_sha256,
                }),
                "new formula workbook does not exist in the selected directory".into(),
                "materialize the retained merge preview as one new XLSX copy with one AST-approved formula cell and no overwrite".into(),
                "reopen the generated package, verify the exact formula cell and policy digest, then verify exact artifact bytes plus SHA-256".into(),
            ),
            ArtifactRequest::Word(args) => (
                ComputerActionKind::File(FilePatchAction::CreateWordReportArtifact {
                    preview_id: args.preview_id,
                    file_name: args.file_name,
                    title: args.title,
                    web_sources: args.web_sources,
                }),
                "new Word report does not exist in the selected directory".into(),
                "materialize the retained merge preview as one new deterministic macro-free DOCX without overwrite".into(),
                "reopen through the retained parent handle and verify the exact generated DOCX bytes plus SHA-256".into(),
            ),
            ArtifactRequest::LocalDraft(args) => (
                ComputerActionKind::File(
                    FilePatchAction::CreateLocalCommunicationDraftArtifact {
                        file_name: args.file_name,
                        draft: args.draft,
                    },
                ),
                "new local communication draft does not exist in the selected directory".into(),
                "create one inert local-only UTF-8 plain-text draft without overwrite or external delivery".into(),
                "re-render with shared trusted logic, reopen through the retained parent handle, and verify exact bytes plus SHA-256".into(),
            ),
        };
        let action = ComputerActionKind::File(artifact_preflight.action().clone());
        if action != decoded_action {
            let dispatch_error = error(
                AgentErrorKind::Internal,
                "artifact Provider parsers produced different typed actions",
                false,
                false,
            );
            record_computer_action_pre_send_failure(
                &store,
                &dispatch_id,
                &server_call_id,
                now_unix_ms,
                &dispatch_error,
            )
            .await?;
            return Err(dispatch_error);
        }
        let plan = SealedComputerActionPlan {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            work_id: claimed.work_id.to_string(),
            action_request_id: server_call_id.clone(),
            execution_generation: generation.clone(),
            device_id: self.target_device_id.clone(),
            interactive_session_incarnation: readiness.readiness.interactive_session_incarnation,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::FileSystem,
                version: artifact_preflight.adapter_version().into(),
            },
            approval_id: grant_id.clone(),
            approved_actor_id: self.actor_id.clone(),
            draft_hash: canonical_input_digest_sha256.clone(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
            timeout_ms: 30_000,
            actions: vec![ComputerActionStep {
                target: selected_directories[0].clone(),
                action,
                before_summary,
                after_intent,
                verification,
            }],
        };
        artifact_pre_send!(plan.validate().map_err(|validation_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to seal artifact plan: {validation_error}"),
                false,
                false,
            )
        }));
        let target = artifact_pre_send!({
            let map = self.connections.read().await;
            map.get(&self.target_connection_id).cloned().ok_or_else(|| {
                error(
                    AgentErrorKind::TargetOffline,
                    "target host is not connected",
                    false,
                    true,
                )
            })
        });
        let audience =
            artifact_pre_send!(target.model.version_info.client_id.clone().ok_or_else(|| {
                error(
                    AgentErrorKind::PermissionDenied,
                    "target host has no bound client id",
                    false,
                    false,
                )
            }));
        if audience != self.target_device_id {
            let dispatch_error = error(
                AgentErrorKind::PermissionDenied,
                "target device binding changed before artifact dispatch",
                false,
                false,
            );
            record_computer_action_pre_send_failure(
                &store,
                &dispatch_id,
                &server_call_id,
                now_unix_ms,
                &dispatch_error,
            )
            .await?;
            return Err(dispatch_error);
        }
        let authz = AuthorizationBlock {
            version: AUTHORIZATION_BLOCK_VERSION,
            exec_admission_policy: ExecAdmissionPolicy::OwnerInteractive,
            scope: AgentScope {
                granted: vec![required_capability],
                mode: ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: Some("oss-device-assistant-artifact".into()),
            },
            orchestrator_grants: vec![orchestrator_grant.into()],
            max_risk: desk_agent_protocol::RiskLevel::Medium,
            actor: AuthzActor {
                user_id: self.actor_id.parse().ok(),
            },
            device: AuthzDevice { device_id: None },
            request_id: generation.clone(),
            session_id: None,
            expires_at: Some((chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()),
            issuer: "signal".into(),
            audience,
            signature: None,
        };
        let wrapper = AuthorizedControlPayload {
            inner: artifact_pre_send!(serde_json::to_value(&plan).map_err(|encode_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to encode artifact plan: {encode_error}"),
                    false,
                    false,
                )
            })),
            authz,
        };
        let frame = artifact_pre_send!(
            SignalingModel::new_request(
                SignalingType::DispatchComputerAction,
                None,
                Some(&wrapper),
            )
            .map_err(|frame_error| {
                error(
                    AgentErrorKind::TransportError,
                    format!("failed to build artifact frame: {frame_error}"),
                    false,
                    false,
                )
            })
        );
        let mut frame = frame;
        frame.request_id = generation.clone();
        let text = artifact_pre_send!(serde_json::to_string(&frame).map_err(|encode_error| {
            error(
                AgentErrorKind::TransportError,
                format!("failed to encode artifact frame: {encode_error}"),
                false,
                false,
            )
        }));
        artifact_pre_send!(
            store
                .bind_computer_transport(
                    &self.target_connection_id,
                    &plan,
                    &session,
                    call,
                    self.model_egress_policy.as_ref()
                )
                .await
                .map_err(|_| error(
                    AgentErrorKind::Internal,
                    "failed to freeze original Computer Action transport",
                    false,
                    false
                ))
        );
        let (completion_tx, completion_rx) = oneshot::channel();
        if !global_computer_action_pending().register(
            generation.clone(),
            self.target_connection_id.clone(),
            completion_tx,
        ) {
            let dispatch_error = error(
                AgentErrorKind::Internal,
                "duplicate artifact execution generation",
                false,
                false,
            );
            record_computer_action_pre_send_failure(
                &store,
                &dispatch_id,
                &server_call_id,
                now_unix_ms,
                &dispatch_error,
            )
            .await?;
            return Err(dispatch_error);
        }
        let sent = target.session.write().await.text(text).await.is_ok();
        self.finish_computer_action(&store, call, &plan, completion_rx, sent, true)
            .await
    }

    #[cfg(test)]
    fn browser_action_from_call(
        call: &ToolCall,
        server_call_id: &str,
    ) -> Result<BrowserActionRequest, AgentError> {
        desk_diagnose_core::provider_preflight::browser_action_from_call(call, server_call_id)
    }

    async fn authorize_and_execute_browser(
        &self,
        call: &ToolCall,
    ) -> Result<ExecOutcome, AgentError> {
        let surface = self.selected_browser_surface.clone().ok_or_else(|| {
            error(
                AgentErrorKind::PermissionDenied,
                "no fresh browser surface is available for this turn",
                false,
                true,
            )
        })?;
        let capability = self
            .provider_registry
            .capability_for_tool(&call.name)
            .ok_or_else(|| {
                error(
                    AgentErrorKind::UnsupportedCapability,
                    "browser Provider is not registered",
                    false,
                    true,
                )
            })?;
        self.verify_current_readiness(capability).await?;
        let provider = self
            .provider_registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered browser capability has a Provider");
        let slack_input = if call.name == "prepare_slack_web_message_handoff" {
            let input: SlackWebDraftHandoffInput = serde_json::from_str(&call.arguments_json)
                .map_err(|decode_error| {
                    error(
                        AgentErrorKind::InvalidInput,
                        format!("invalid Slack Web handoff input: {decode_error}"),
                        false,
                        true,
                    )
                })?;
            input.validate().map_err(|validation_error| {
                error(
                    AgentErrorKind::InvalidInput,
                    format!("invalid Slack Web handoff input: {validation_error}"),
                    false,
                    true,
                )
            })?;
            Some(input)
        } else {
            None
        };
        let gmail_input = if call.name == "prepare_gmail_web_draft_handoff" {
            let input: GmailWebDraftHandoffInput = serde_json::from_str(&call.arguments_json)
                .map_err(|decode_error| {
                    error(
                        AgentErrorKind::InvalidInput,
                        format!("invalid Gmail Web handoff input: {decode_error}"),
                        false,
                        true,
                    )
                })?;
            input.validate().map_err(|validation_error| {
                error(
                    AgentErrorKind::InvalidInput,
                    format!("invalid Gmail Web handoff input: {validation_error}"),
                    false,
                    true,
                )
            })?;
            Some(input)
        } else {
            None
        };
        let session = self.authoritative_session().await?;
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "invalid browser preflight clock",
                false,
                false,
            )
        })?;
        let server_call_id = format!(
            "capability-call-{:x}",
            Sha256::digest(format!("{}:{}:{}", self.run_id, self.turn_id, call.id).as_bytes())
        );
        let evaluated = desk_diagnose_core::provider_preflight::BrowserCallPreflight::build(
            &self.provider_registry,
            ProductSurface::OssPersonalOwner,
            call,
            &server_call_id,
            &surface,
            now_unix_ms,
        )?;
        let subject = desk_diagnose_core::provider_preflight::ProviderCallSubject {
            actor_id: &self.actor_id,
            run_id: &self.run_id,
            target_device_id: &self.target_device_id,
            policy_revision: self.policy_revision,
            readiness_revision: self.readiness_revision,
            now_unix_ms,
        };
        let call_authority = evaluated.grant_call(&subject)?;
        let request = evaluated.request().clone();
        let canonical_input_json = evaluated.canonical_input_json().to_string();
        let canonical_input_digest_sha256 =
            call_authority.canonical_input_digest_sha256.to_string();
        let resource_scope = call_authority.resource_scope.to_vec();
        let operation_scope = call_authority.operation_scope.to_vec();
        let export_destinations = call_authority.export_destinations.to_vec();
        let risk_tier = call_authority.risk_tier;
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        let grant_id = if let Some(existing) = store
            .prepared_grant_id(&server_call_id)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load prepared browser authority: {db_error}"),
                    false,
                    false,
                )
            })? {
            existing
        } else if browser_policy_auto_authorized(risk_tier) {
            let grant_id = format!(
                "policy-auto-browser-{:x}",
                Sha256::digest(format!("{}:{}:{}", self.run_id, self.turn_id, call.id).as_bytes())
            );
            let grant = CapabilityGrant {
                schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
                grant_id: grant_id.clone(),
                actor_id: self.actor_id.clone(),
                run_id: self.run_id.clone(),
                surface: ProductSurface::OssPersonalOwner,
                target_device_id: self.target_device_id.clone(),
                target_session_id: None,
                provider_id: provider.wire.provider_id.clone(),
                capability_id: capability.wire.capability_id.clone(),
                tool_name: call.name.clone(),
                tool_schema_version: capability.wire.input_schema_version,
                effect: capability.wire.effect,
                risk_tier,
                resource_scope: resource_scope.clone(),
                operation_scope: operation_scope.clone(),
                export_destinations: export_destinations.clone(),
                allowed_envelope_ids: Vec::new(),
                allowed_content_digests_sha256: Vec::new(),
                use_policy: CapabilityGrantUsePolicy::OneShotExact,
                canonical_input_digest_sha256: Some(canonical_input_digest_sha256.clone()),
                issued_by: CapabilityGrantIssuer::PolicyAuto,
                issued_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: now_unix_ms.saturating_add(60_000),
                remaining_uses: 1,
                limits: CapabilityGrantLimits {
                    max_bytes_per_call: capability.wire.limits.max_output_bytes,
                    max_items_per_call: capability.wire.limits.max_objects,
                    max_calls: 1,
                },
                policy_revision: self.policy_revision,
                readiness_revision: self.readiness_revision,
                revoked_at_unix_ms: None,
                revoked_reason: None,
            };
            store.issue(&grant).await.map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to issue browser policy-auto grant: {db_error}"),
                    false,
                    false,
                )
            })?;
            grant_id
        } else {
            store
                .list_for_subject(&self.run_id, &self.actor_id, &self.target_device_id)
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("failed to load browser grants: {db_error}"),
                        false,
                        false,
                    )
                })?
                .into_iter()
                .find(|grant| match_capability_grant(grant, &call_authority).is_ok())
                .map(|grant| grant.grant_id)
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        "browser action requires an active approved capability grant",
                        false,
                        true,
                    )
                })?
        };
        if slack_input.is_some() || gmail_input.is_some() {
            let site = if gmail_input.is_some() {
                "gmail"
            } else {
                "slack"
            };
            let destination = export_destinations
                .first()
                .expect("Web communication handoff has one fixed destination")
                .clone();
            let source_envelope_id = format!("{site}-source-{server_call_id}");
            let source = DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: source_envelope_id.clone(),
                content: ContentRef::ImmutableBlob {
                    blob_id: format!("{site}-input-{server_call_id}"),
                    sha256: canonical_input_digest_sha256.clone(),
                    size_bytes: canonical_input_json.len() as u64,
                    media_type: "application/json".into(),
                },
                provenance: DataProvenance {
                    source_provider_id: provider.wire.provider_id.clone(),
                    source_tool_name: call.name.clone(),
                    source_object_id: Some(server_call_id.clone()),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: canonical_input_digest_sha256.clone(),
                sensitivity: Sensitivity::Sensitive,
                allowed_destinations: Vec::new(),
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(now_unix_ms.saturating_add(60_000)),
                    delete_with_run: true,
                },
            };
            let (exported, _) = authorize_export(
                &source,
                &format!("{site}-export-{server_call_id}"),
                &ExportDataAuthorization {
                    authorization_id: grant_id.clone(),
                    source_envelope_ids: vec![source_envelope_id],
                    destination: destination.clone(),
                    max_sensitivity: Sensitivity::Sensitive,
                    expires_at_unix_ms: now_unix_ms.saturating_add(60_000),
                    max_bytes: capability.wire.limits.max_input_bytes,
                },
                now_unix_ms,
            )
            .map_err(|authorization_error| {
                error(
                    AgentErrorKind::PermissionDenied,
                    format!("Web communication sink authorization failed: {authorization_error}"),
                    false,
                    true,
                )
            })?;
            DefaultSinkAuthorizer
                .authorize(
                    &destination,
                    &[SinkInput {
                        envelope: &exported,
                        bytes: canonical_input_json.as_bytes(),
                    }],
                    now_unix_ms,
                    capability.wire.limits.max_input_bytes as usize,
                )
                .map_err(|authorization_error| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        format!(
                            "Web communication sink projection was rejected: {authorization_error}"
                        ),
                        false,
                        true,
                    )
                })?;
        }
        let prepare = || PrepareCapabilityCall {
            grant_id: &grant_id,
            call_id: &server_call_id,
            turn_id: &self.turn_id,
            input_revision: session.input_revision,
            input_watermark: session.latest_input_seq,
            generation: 1,
            canonical_input_json: &canonical_input_json,
            call: call_authority.clone(),
        };
        let prepared = store.prepare(prepare()).await.map_err(|db_error| {
            error(
                AgentErrorKind::PermissionDenied,
                format!("browser call authorization failed: {db_error}"),
                false,
                true,
            )
        })?;
        let dispatch_id =
            match store
                .record_dispatch_intent(prepare())
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        format!("browser dispatch authorization failed: {db_error}"),
                        false,
                        true,
                    )
                })? {
                DispatchIntentResult::Recorded { dispatch_id, .. } => dispatch_id,
                DispatchIntentResult::SupersededBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "browser call was superseded by newer user input",
                        false,
                        true,
                    ));
                }
                DispatchIntentResult::RevokedBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "browser grant was revoked before dispatch",
                        false,
                        true,
                    ));
                }
            };
        let claimed = match store
            .claim_dispatch(&dispatch_id, now_unix_ms)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to claim browser dispatch: {db_error}"),
                    false,
                    false,
                )
            })? {
            DispatchClaimResult::Claimed(payload) => payload,
            DispatchClaimResult::OutcomeUnknown { .. } => {
                return Ok(ExecOutcome::Unknown(
                    desk_diagnose_core::session::ActionIdentity::new(
                        prepared.work_id,
                        server_call_id,
                        dispatch_id,
                        desk_diagnose_core::session::WorkKind::ComputerAction,
                    ),
                ));
            }
        };

        macro_rules! browser_pre_send {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(dispatch_error) => {
                        record_computer_action_pre_send_failure(
                            &store,
                            &dispatch_id,
                            &server_call_id,
                            now_unix_ms,
                            &dispatch_error,
                        )
                        .await?;
                        return Err(dispatch_error);
                    }
                }
            };
        }

        let readiness = browser_pre_send!(
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&self.target_connection_id, chrono::Utc::now())
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::TargetOffline,
                        "browser readiness expired before dispatch",
                        false,
                        true,
                    )
                })
        );
        let generation = dispatch_id.clone();
        let plan = SealedComputerActionPlan {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            work_id: claimed.work_id.to_string(),
            action_request_id: server_call_id.clone(),
            execution_generation: generation.clone(),
            device_id: self.target_device_id.clone(),
            interactive_session_incarnation: readiness.readiness.interactive_session_incarnation,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::BrowserDevtoolsMcp,
                version: desk_diagnose_core::device_assistant::BROWSER_DEVTOOLS_ADAPTER_VERSION
                    .into(),
            },
            approval_id: grant_id.clone(),
            approved_actor_id: self.actor_id.clone(),
            draft_hash: canonical_input_digest_sha256.clone(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
            timeout_ms: 30_000,
            actions: vec![ComputerActionStep {
                target: surface,
                action: ComputerActionKind::Browser(request),
                before_summary:
                    "browser surface is bound to the approved profile and page revision".into(),
                after_intent: if gmail_input.is_some() {
                    "fill one Gmail Web To/Subject/Body set and stop without activating Send".into()
                } else if slack_input.is_some() {
                    "fill one Slack Web composer and stop without activating Send".into()
                } else {
                    "perform one closed semantic browser action".into()
                },
                verification: if gmail_input.is_some() {
                    "read back the exact Gmail To/Subject/Body values and return ManualOnly handoff"
                        .into()
                } else if slack_input.is_some() {
                    "read back the exact Slack composer value and return ManualOnly handoff".into()
                } else {
                    "return only a typed page-bound semantic result".into()
                },
            }],
        };
        browser_pre_send!(plan.validate().map_err(|validation_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to seal browser plan: {validation_error}"),
                false,
                false,
            )
        }));
        let target = browser_pre_send!({
            let map = self.connections.read().await;
            map.get(&self.target_connection_id).cloned().ok_or_else(|| {
                error(
                    AgentErrorKind::TargetOffline,
                    "target host is not connected",
                    false,
                    true,
                )
            })
        });
        let audience =
            browser_pre_send!(target.model.version_info.client_id.clone().ok_or_else(|| {
                error(
                    AgentErrorKind::PermissionDenied,
                    "target host has no bound client id",
                    false,
                    false,
                )
            }));
        if audience != self.target_device_id {
            let dispatch_error = error(
                AgentErrorKind::PermissionDenied,
                "target device binding changed before browser dispatch",
                false,
                false,
            );
            record_computer_action_pre_send_failure(
                &store,
                &dispatch_id,
                &server_call_id,
                now_unix_ms,
                &dispatch_error,
            )
            .await?;
            return Err(dispatch_error);
        }
        let max_risk = match risk_tier {
            CapabilityRiskTier::R0 | CapabilityRiskTier::R1 => desk_agent_protocol::RiskLevel::Low,
            CapabilityRiskTier::R2 => desk_agent_protocol::RiskLevel::Medium,
            CapabilityRiskTier::R3 => desk_agent_protocol::RiskLevel::High,
        };
        let authz = AuthorizationBlock {
            version: AUTHORIZATION_BLOCK_VERSION,
            exec_admission_policy: ExecAdmissionPolicy::OwnerInteractive,
            scope: AgentScope {
                granted: vec![capability.required_capability],
                mode: ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: Some("oss-device-assistant-browser".into()),
            },
            orchestrator_grants: vec![capability.wire.capability_id.clone()],
            max_risk,
            actor: AuthzActor {
                user_id: self.actor_id.parse().ok(),
            },
            device: AuthzDevice { device_id: None },
            request_id: generation.clone(),
            session_id: None,
            expires_at: Some((chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()),
            issuer: "signal".into(),
            audience,
            signature: None,
        };
        let wrapper = AuthorizedControlPayload {
            inner: browser_pre_send!(serde_json::to_value(&plan).map_err(|encode_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to encode browser plan: {encode_error}"),
                    false,
                    false,
                )
            })),
            authz,
        };
        let mut frame = browser_pre_send!(
            SignalingModel::new_request(
                SignalingType::DispatchComputerAction,
                None,
                Some(&wrapper),
            )
            .map_err(|frame_error| {
                error(
                    AgentErrorKind::TransportError,
                    format!("failed to build browser frame: {frame_error}"),
                    false,
                    false,
                )
            })
        );
        frame.request_id = generation.clone();
        let text = browser_pre_send!(serde_json::to_string(&frame).map_err(|encode_error| {
            error(
                AgentErrorKind::TransportError,
                format!("failed to encode browser frame: {encode_error}"),
                false,
                false,
            )
        }));
        if capability.wire.effect.is_side_effecting() {
            browser_pre_send!(
                store
                    .bind_computer_transport(
                        &self.target_connection_id,
                        &plan,
                        &session,
                        call,
                        self.model_egress_policy.as_ref()
                    )
                    .await
                    .map_err(|_| error(
                        AgentErrorKind::Internal,
                        "failed to freeze original Computer Action transport",
                        false,
                        false
                    ))
            );
        }
        let (completion_tx, completion_rx) = oneshot::channel();
        if !global_computer_action_pending().register(
            generation.clone(),
            self.target_connection_id.clone(),
            completion_tx,
        ) {
            let dispatch_error = error(
                AgentErrorKind::Internal,
                "duplicate browser execution generation",
                false,
                false,
            );
            record_computer_action_pre_send_failure(
                &store,
                &dispatch_id,
                &server_call_id,
                now_unix_ms,
                &dispatch_error,
            )
            .await?;
            return Err(dispatch_error);
        }
        let sent = target.session.write().await.text(text).await.is_ok();
        self.finish_computer_action(
            &store,
            call,
            &plan,
            completion_rx,
            sent,
            capability.wire.effect.is_side_effecting(),
        )
        .await
    }

    async fn authorize_and_execute_outlook_handoff(
        &self,
        call: &ToolCall,
    ) -> Result<ExecOutcome, AgentError> {
        let surface_ref = self.selected_outlook_surface.clone().ok_or_else(|| {
            error(
                AgentErrorKind::PermissionDenied,
                "no fresh Outlook (new) application surface is available for this turn",
                false,
                true,
            )
        })?;
        let capability = self
            .provider_registry
            .capability_for_tool(&call.name)
            .ok_or_else(|| {
                error(
                    AgentErrorKind::UnsupportedCapability,
                    "Outlook handoff Provider is not registered",
                    false,
                    true,
                )
            })?;
        self.verify_current_readiness(capability).await?;
        let initial_readiness =
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&self.target_connection_id, chrono::Utc::now())
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::TargetOffline,
                        "Outlook readiness expired before request sealing",
                        false,
                        true,
                    )
                })?;
        if initial_readiness.readiness.revision != self.readiness_revision {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Outlook readiness changed before request sealing",
                false,
                true,
            ));
        }
        let interactive_session_incarnation = initial_readiness
            .readiness
            .interactive_session_incarnation
            .clone();
        let provider = self
            .provider_registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered Outlook capability has a Provider");
        let session = self.authoritative_session().await?;
        let (canonical_input_json, canonical_input_digest_sha256) =
            Self::canonical_call_input(call)?;
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "system clock predates the Unix epoch",
                false,
                false,
            )
        })?;
        let server_call_id = format!(
            "capability-call-{:x}",
            Sha256::digest(format!("{}:{}:{}", self.run_id, self.turn_id, call.id).as_bytes())
        );
        let evaluated = desk_diagnose_core::provider_preflight::OutlookCallPreflight::build(
            &self.provider_registry,
            ProductSurface::OssPersonalOwner,
            call,
            &server_call_id,
            &self.run_id,
            &self.target_device_id,
            &interactive_session_incarnation,
            self.readiness_revision,
            &surface_ref,
            now_unix_ms,
        )?;
        if evaluated.canonical_input_json() != canonical_input_json {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Outlook canonical input changed during request sealing",
                false,
                false,
            ));
        }
        let request = evaluated.request().clone();
        let subject = desk_diagnose_core::provider_preflight::ProviderCallSubject {
            actor_id: &self.actor_id,
            run_id: &self.run_id,
            target_device_id: &self.target_device_id,
            policy_revision: self.policy_revision,
            readiness_revision: self.readiness_revision,
            now_unix_ms,
        };
        let call_authority = evaluated.grant_call(&subject)?;
        if call_authority.canonical_input_digest_sha256 != canonical_input_digest_sha256 {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Outlook canonical input digest changed during request sealing",
                false,
                false,
            ));
        }
        let destination = call_authority
            .export_destinations
            .first()
            .cloned()
            .ok_or_else(|| {
                error(
                    AgentErrorKind::Internal,
                    "Outlook handoff has no fixed account destination",
                    false,
                    false,
                )
            })?;
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        let grant_id = if let Some(existing) = store
            .prepared_grant_id(&server_call_id)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to load prepared Outlook authority: {db_error}"),
                    false,
                    false,
                )
            })? {
            existing
        } else {
            store
                .list_for_subject(&self.run_id, &self.actor_id, &self.target_device_id)
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("failed to load Outlook grants: {db_error}"),
                        false,
                        false,
                    )
                })?
                .into_iter()
                .find(|grant| match_capability_grant(grant, &call_authority).is_ok())
                .map(|grant| grant.grant_id)
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        "Outlook draft handoff requires an active exact WriteExternalDraft grant",
                        false,
                        true,
                    )
                })?
        };

        // The exact canonical tool bytes cross into an external, potentially
        // cloud-synchronised draft sink. Expand their destination only under
        // the matched grant, then run the unified sink check before dispatch.
        let source_envelope_id = format!("outlook-source-{server_call_id}");
        let source = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: source_envelope_id.clone(),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("outlook-input-{server_call_id}"),
                sha256: canonical_input_digest_sha256.clone(),
                size_bytes: canonical_input_json.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: provider.wire.provider_id.clone(),
                source_tool_name: call.name.clone(),
                source_object_id: Some(server_call_id.clone()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: canonical_input_digest_sha256.clone(),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(now_unix_ms.saturating_add(60_000)),
                delete_with_run: true,
            },
        };
        let (exported, _) = authorize_export(
            &source,
            &format!("outlook-export-{server_call_id}"),
            &ExportDataAuthorization {
                authorization_id: grant_id.clone(),
                source_envelope_ids: vec![source_envelope_id],
                destination: destination.clone(),
                max_sensitivity: Sensitivity::Sensitive,
                expires_at_unix_ms: now_unix_ms.saturating_add(60_000),
                max_bytes: capability.wire.limits.max_input_bytes,
            },
            now_unix_ms,
        )
        .map_err(|authorization_error| {
            error(
                AgentErrorKind::PermissionDenied,
                format!("Outlook sink authorization failed: {authorization_error}"),
                false,
                true,
            )
        })?;
        DefaultSinkAuthorizer
            .authorize(
                &destination,
                &[SinkInput {
                    envelope: &exported,
                    bytes: canonical_input_json.as_bytes(),
                }],
                now_unix_ms,
                capability.wire.limits.max_input_bytes as usize,
            )
            .map_err(|authorization_error| {
                error(
                    AgentErrorKind::PermissionDenied,
                    format!("Outlook sink projection was rejected: {authorization_error}"),
                    false,
                    true,
                )
            })?;

        // Re-resolve all mutable target/session bindings before creating the
        // durable dispatch intent. After intent, any transport uncertainty is
        // OutcomeUnknown and the one-shot grant is never restored.
        let dispatch_readiness =
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&self.target_connection_id, chrono::Utc::now())
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::TargetOffline,
                        "Outlook readiness expired before dispatch intent",
                        false,
                        true,
                    )
                })?;
        if dispatch_readiness.readiness.revision != self.readiness_revision
            || dispatch_readiness.readiness.interactive_session_incarnation
                != interactive_session_incarnation
        {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Outlook readiness or interactive session changed before dispatch intent",
                false,
                true,
            ));
        }
        let target = {
            let map = self.connections.read().await;
            map.get(&self.target_connection_id).cloned()
        }
        .ok_or_else(|| {
            error(
                AgentErrorKind::TargetOffline,
                "target host is not connected",
                false,
                true,
            )
        })?;
        let audience = target.model.version_info.client_id.clone().ok_or_else(|| {
            error(
                AgentErrorKind::PermissionDenied,
                "target host has no bound client id",
                false,
                false,
            )
        })?;
        if audience != self.target_device_id {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "target device binding changed before Outlook dispatch intent",
                false,
                false,
            ));
        }

        let prepare = || PrepareCapabilityCall {
            grant_id: &grant_id,
            call_id: &server_call_id,
            turn_id: &self.turn_id,
            input_revision: session.input_revision,
            input_watermark: session.latest_input_seq,
            generation: 1,
            canonical_input_json: &canonical_input_json,
            call: call_authority.clone(),
        };
        let prepared = store.prepare(prepare()).await.map_err(|db_error| {
            error(
                AgentErrorKind::PermissionDenied,
                format!("Outlook handoff authorization failed: {db_error}"),
                false,
                true,
            )
        })?;
        let dispatch_id =
            match store
                .record_dispatch_intent(prepare())
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::PermissionDenied,
                        format!("Outlook dispatch authorization failed: {db_error}"),
                        false,
                        true,
                    )
                })? {
                DispatchIntentResult::Recorded { dispatch_id, .. } => dispatch_id,
                DispatchIntentResult::SupersededBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "Outlook handoff was superseded by newer user input",
                        false,
                        true,
                    ));
                }
                DispatchIntentResult::RevokedBeforeIntent { .. } => {
                    return Err(error(
                        AgentErrorKind::PermissionDenied,
                        "Outlook grant was revoked before dispatch",
                        false,
                        true,
                    ));
                }
            };
        let claimed = match store
            .claim_dispatch(&dispatch_id, now_unix_ms)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to claim Outlook dispatch: {db_error}"),
                    false,
                    false,
                )
            })? {
            DispatchClaimResult::Claimed(payload) => payload,
            DispatchClaimResult::OutcomeUnknown { .. } => {
                return Ok(ExecOutcome::Unknown(
                    desk_diagnose_core::session::ActionIdentity::new(
                        prepared.work_id,
                        server_call_id,
                        dispatch_id,
                        desk_diagnose_core::session::WorkKind::ComputerAction,
                    ),
                ));
            }
        };
        let generation = dispatch_id.clone();
        let plan = SealedComputerActionPlan {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            work_id: claimed.work_id.to_string(),
            action_request_id: server_call_id.clone(),
            execution_generation: generation.clone(),
            device_id: self.target_device_id.clone(),
            interactive_session_incarnation,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OutlookNewMailto,
                version:
                    desk_diagnose_core::device_assistant::OUTLOOK_NEW_MAILTO_ADAPTER_VERSION
                        .into(),
            },
            approval_id: grant_id,
            approved_actor_id: self.actor_id.clone(),
            draft_hash: canonical_input_digest_sha256,
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
            timeout_ms: 30_000,
            actions: vec![ComputerActionStep {
                target: surface_ref,
                action: ComputerActionKind::Communication(request),
                before_summary: "the reviewed Outlook (new) mailto handler is ready in the current interactive session".into(),
                after_intent: "open one bounded compose surface and stop before send".into(),
                verification: "return only AssistiveUnverified ManualOnly HandedOffToUser".into(),
            }],
        };
        plan.validate().map_err(|validation_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to seal Outlook handoff plan: {validation_error}"),
                false,
                false,
            )
        })?;
        let authz = AuthorizationBlock {
            version: AUTHORIZATION_BLOCK_VERSION,
            exec_admission_policy: ExecAdmissionPolicy::OwnerInteractive,
            scope: AgentScope {
                granted: vec![capability.required_capability],
                mode: ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: Some("oss-device-assistant-outlook-handoff".into()),
            },
            orchestrator_grants: vec![capability.wire.capability_id.clone()],
            max_risk: desk_agent_protocol::RiskLevel::Medium,
            actor: AuthzActor {
                user_id: self.actor_id.parse().ok(),
            },
            device: AuthzDevice { device_id: None },
            request_id: generation.clone(),
            session_id: None,
            expires_at: Some((chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()),
            issuer: "signal".into(),
            audience,
            signature: None,
        };
        let wrapper = AuthorizedControlPayload {
            inner: serde_json::to_value(&plan).map_err(|encode_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to encode Outlook handoff plan: {encode_error}"),
                    false,
                    false,
                )
            })?,
            authz,
        };
        let mut frame = SignalingModel::new_request(
            SignalingType::DispatchComputerAction,
            None,
            Some(&wrapper),
        )
        .map_err(|frame_error| {
            error(
                AgentErrorKind::TransportError,
                format!("failed to build Outlook handoff frame: {frame_error}"),
                false,
                false,
            )
        })?;
        frame.request_id = generation.clone();
        let text = serde_json::to_string(&frame).map_err(|encode_error| {
            error(
                AgentErrorKind::TransportError,
                format!("failed to encode Outlook handoff frame: {encode_error}"),
                false,
                false,
            )
        })?;
        store
            .bind_computer_transport(
                &self.target_connection_id,
                &plan,
                &session,
                call,
                self.model_egress_policy.as_ref(),
            )
            .await
            .map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "failed to freeze original Computer Action transport",
                    false,
                    false,
                )
            })?;
        let (completion_tx, completion_rx) = oneshot::channel();
        if !global_computer_action_pending().register(
            generation.clone(),
            self.target_connection_id.clone(),
            completion_tx,
        ) {
            return Err(error(
                AgentErrorKind::Internal,
                "duplicate Outlook execution generation",
                false,
                false,
            ));
        }
        let sent = target.session.write().await.text(text).await.is_ok();
        self.finish_computer_action(&store, call, &plan, completion_rx, sent, true)
            .await
    }

    async fn invoke(
        &self,
        call: &ToolCall,
        grant: &CapabilityGrant,
        authority_expiry: u64,
    ) -> Result<ToolRunOutput, ProviderInvokeError> {
        if call.name == WEB_FETCH_TOOL_NAME {
            let validated = validate_fetch_call(call, &self.current_user_message)?;
            return fetch_public_web_page(validated).await.map_err(Into::into);
        }
        if call.name == WEB_SEARCH_TOOL_NAME {
            let validated = validate_search_call(call, &self.current_user_message)?;
            return search_public_web(validated, &call.id)
                .await
                .map_err(Into::into);
        }
        let (capability, mut input) = build_read_operation(call)?;
        if capability == desk_agent_protocol::Capability::OfficeDocumentInspect {
            let document = self.selected_office_document.clone().ok_or_else(|| {
                error(
                    AgentErrorKind::PermissionDenied,
                    "no exact paired Excel document was selected for this turn",
                    false,
                    true,
                )
            })?;
            input = OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::OfficeDocumentInspect(OfficeInspectParams {
                    document: Some(document),
                    selection_only: true,
                    max_objects: 16,
                    max_bytes: 256 * 1024,
                }),
            });
        }
        let is_iwork_batch_inspect = matches!(
            call.name.as_str(),
            "inspect_selected_numbers_with_iwork"
                | "inspect_selected_pages_with_iwork"
                | "inspect_selected_keynote_with_iwork"
        );
        if is_iwork_batch_inspect {
            let file = exact_selected_batch_file(&self.selected_file_roots)?;
            let requested_max_bytes = match &input {
                OperationInput::ReadContext(ReadContextInput {
                    kind:
                        ContextKind::SpreadsheetLiveInspect(params)
                        | ContextKind::DocumentLiveInspect(params)
                        | ContextKind::PresentationLiveInspect(params),
                }) => params.max_bytes,
                _ => {
                    return Err(error(
                        AgentErrorKind::InvalidInput,
                        "BatchDocument inspection received the wrong operation input",
                        false,
                        true,
                    )
                    .into());
                }
            };
            let params = LiveDocumentInspectParams {
                target: None,
                batch_file: Some(file),
                max_bytes: requested_max_bytes,
            };
            input = OperationInput::ReadContext(ReadContextInput {
                kind: match call.name.as_str() {
                    "inspect_selected_numbers_with_iwork" => {
                        ContextKind::SpreadsheetLiveInspect(params)
                    }
                    "inspect_selected_pages_with_iwork" => ContextKind::DocumentLiveInspect(params),
                    "inspect_selected_keynote_with_iwork" => {
                        ContextKind::PresentationLiveInspect(params)
                    }
                    _ => unreachable!(),
                },
            });
        }
        let live_target = match capability {
            desk_agent_protocol::Capability::SpreadsheetLiveInspect => {
                self.selected_live_spreadsheet.clone()
            }
            desk_agent_protocol::Capability::DocumentLiveInspect => {
                self.selected_live_document.clone()
            }
            desk_agent_protocol::Capability::PresentationLiveInspect => {
                self.selected_live_presentation.clone()
            }
            _ => None,
        };
        if matches!(
            capability,
            desk_agent_protocol::Capability::SpreadsheetLiveInspect
                | desk_agent_protocol::Capability::DocumentLiveInspect
                | desk_agent_protocol::Capability::PresentationLiveInspect
        ) && !is_iwork_batch_inspect
        {
            let target = live_target.ok_or_else(|| {
                error(
                    AgentErrorKind::PermissionDenied,
                    "the selected iWork object is no longer available; refresh context",
                    false,
                    true,
                )
            })?;
            input = OperationInput::ReadContext(ReadContextInput {
                kind: match capability {
                    desk_agent_protocol::Capability::SpreadsheetLiveInspect => {
                        ContextKind::SpreadsheetLiveInspect(LiveDocumentInspectParams {
                            target: Some(target),
                            batch_file: None,
                            max_bytes: 256 * 1024,
                        })
                    }
                    desk_agent_protocol::Capability::DocumentLiveInspect => {
                        ContextKind::DocumentLiveInspect(LiveDocumentInspectParams {
                            target: Some(target),
                            batch_file: None,
                            max_bytes: 256 * 1024,
                        })
                    }
                    desk_agent_protocol::Capability::PresentationLiveInspect => {
                        ContextKind::PresentationLiveInspect(LiveDocumentInspectParams {
                            target: Some(target),
                            batch_file: None,
                            max_bytes: 256 * 1024,
                        })
                    }
                    _ => unreachable!(),
                },
            });
        }
        if capability == desk_agent_protocol::Capability::FileMetadataRead {
            if self.selected_file_roots.is_empty() {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no active file attachment was selected for this turn",
                    false,
                    true,
                )
                .into());
            }
            let requested = match &input {
                OperationInput::ReadContext(ReadContextInput {
                    kind: ContextKind::FileMetadataInspect(params),
                }) => params.clone(),
                _ => {
                    return Err(error(
                        AgentErrorKind::InvalidInput,
                        "file metadata capability received the wrong operation input",
                        false,
                        true,
                    )
                    .into());
                }
            };
            input = OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::FileMetadataInspect(FileMetadataInspectParams {
                    roots: self.selected_file_roots.clone(),
                    max_entries: 256,
                    max_bytes: 64 * 1024,
                    enumerate_directories: true,
                    file_extensions: requested.file_extensions,
                    min_file_bytes: requested.min_file_bytes,
                    max_file_bytes: requested.max_file_bytes,
                    modified_after: requested.modified_after,
                    modified_before: requested.modified_before,
                }),
            });
        }
        if capability == desk_agent_protocol::Capability::FileContentRead {
            let files = self
                .selected_file_roots
                .iter()
                .filter(|object_ref| object_ref.object_kind == ObjectKind::File)
                .cloned()
                .collect::<Vec<_>>();
            if files.len() != 1 {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "select exactly one regular file before reading its text",
                    false,
                    true,
                )
                .into());
            }
            input = OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::FileContentRead(FileContentReadParams {
                    file: files[0].clone(),
                    max_bytes: 64 * 1024,
                }),
            });
        }
        if capability == desk_agent_protocol::Capability::SpreadsheetFileInspect {
            let files = self.selected_spreadsheet_roots.clone();
            if files.is_empty() {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "select at least one spreadsheet file or directory before inspecting it",
                    false,
                    true,
                )
                .into());
            }
            input = OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::SpreadsheetFileInspect(SpreadsheetFileInspectParams {
                    files,
                    max_workbooks: 8,
                    max_sheets: 16,
                    max_rows: 200,
                    max_columns: 64,
                    max_bytes: 256 * 1024,
                }),
            });
        }
        if capability == desk_agent_protocol::Capability::SpreadsheetMergePreview {
            if self.selected_spreadsheet_roots.is_empty() {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "select at least one spreadsheet file or directory before previewing a merge",
                    false,
                    true,
                )
                .into());
            }
            let OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::SpreadsheetMergePreview(params),
            }) = input
            else {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "spreadsheet merge preview input is not typed",
                    false,
                    true,
                )
                .into());
            };
            input = OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::SpreadsheetMergePreview(SpreadsheetMergePreviewParams {
                    files: self.selected_spreadsheet_roots.clone(),
                    ..params
                }),
            });
        }
        if capability == desk_agent_protocol::Capability::TerminalOutputRead {
            if self.selected_terminal_roots.is_empty() {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no active terminal output attachment was selected for this turn",
                    false,
                    true,
                )
                .into());
            }
            input = OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::TerminalOutputInspect(TerminalOutputInspectParams {
                    roots: self.selected_terminal_roots.clone(),
                    max_bytes: 32 * 1024,
                }),
            });
        }
        if !matches!(
            capability,
            desk_agent_protocol::Capability::DesktopSessionInspect
                | desk_agent_protocol::Capability::DesktopUiInspect
                | desk_agent_protocol::Capability::OfficeDocumentInspect
                | desk_agent_protocol::Capability::FileMetadataRead
                | desk_agent_protocol::Capability::FileContentRead
                | desk_agent_protocol::Capability::SpreadsheetFileInspect
                | desk_agent_protocol::Capability::SpreadsheetMergePreview
                | desk_agent_protocol::Capability::TerminalOutputRead
                | desk_agent_protocol::Capability::ScreenCaptureCurrent
                | desk_agent_protocol::Capability::SystemInfo
                | desk_agent_protocol::Capability::ProcessList
                | desk_agent_protocol::Capability::NetworkPorts
                | desk_agent_protocol::Capability::ServiceStatus
                | desk_agent_protocol::Capability::LogRecent
                | desk_agent_protocol::Capability::ContainerList
                | desk_agent_protocol::Capability::SpreadsheetLiveInspect
                | desk_agent_protocol::Capability::DocumentLiveInspect
                | desk_agent_protocol::Capability::PresentationLiveInspect
        ) {
            return Err(error(
                AgentErrorKind::UnsupportedCapability,
                "Device Assistant may only invoke selected read-only observations",
                false,
                true,
            )
            .into());
        }
        let object_expiry = if requires_objects(&call.name) {
            self.validate_original_objects().await?;
            // Rebind from the model's original bounded request; legacy defaults
            // above cannot widen either the owner's or model's read limit.
            let (_, mut bounded) = build_read_operation(call)?;
            let binding = self.object_binding()?;
            binding.bind(call, &mut bounded)?;
            input = bounded;
            Some(
                chrono::DateTime::from_timestamp_millis(binding.expiry(call)? as i64)
                    .ok_or_else(|| {
                        error(
                            AgentErrorKind::Internal,
                            "invalid object expiry",
                            false,
                            false,
                        )
                    })?
                    .to_rfc3339(),
            )
        } else {
            None
        };
        desk_diagnose_core::provider_preflight::read::limits::bind(
            &self.provider_registry,
            call,
            &mut input,
            &grant.limits,
        )?;
        let envelope_expiry = object_expiry
            .and_then(|expiry| chrono::DateTime::parse_from_rfc3339(&expiry).ok())
            .and_then(|expiry| u64::try_from(expiry.timestamp_millis()).ok())
            .map_or(authority_expiry, |expiry| expiry.min(authority_expiry));
        let request_id = uuid::Uuid::new_v4().to_string();
        let envelope: desk_agent_protocol::ReadonlyAgentEnvelope = AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId(request_id.clone()),
            parent_task_id: None,
            target: TargetRef {
                device_id: self.target_device_id.clone(),
                session_id: None,
                worker_id: None,
            },
            actor: ActorRef {
                actor_type: ActorType::User,
                actor_id: self.actor_id.clone(),
            },
            caller: CallerRef {
                caller_type: CallerType::AiModel,
                model_provider: self.model_provider.clone(),
                model_name: self.model_name.clone(),
                adapter: Some("device-assistant-read-v1".into()),
            },
            scope: AgentScope {
                granted: vec![capability],
                mode: ExecutionMode::ReadOnly,
                expires_at: Some(
                    chrono::DateTime::from_timestamp_millis(envelope_expiry as i64)
                        .ok_or_else(|| {
                            error(
                                AgentErrorKind::Internal,
                                "invalid read authorization expiry",
                                false,
                                false,
                            )
                        })?
                        .to_rfc3339(),
                ),
                policy_name: Some("oss-device-assistant-read-only".into()),
            },
            operation: AgentOperation {
                risk_hint: None,
                input,
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("Device Assistant read-only observation".into()),
            },
        }
        .try_into()
        .map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "failed to construct read-only Device Assistant envelope",
                false,
                false,
            )
        })?;
        let request = RemoteToolRequest {
            request_id: request_id.clone(),
            tool_call_id: call.id.clone(),
            envelope,
        };
        let target = {
            let map = self.connections.read().await;
            map.get(&self.target_connection_id).cloned()
        }
        .ok_or_else(|| {
            error(
                AgentErrorKind::TargetOffline,
                "target host is not connected",
                true,
                true,
            )
        })?;
        let (tx, rx) = oneshot::channel();
        if !self
            .pending
            .register(request_id.clone(), self.target_connection_id.clone(), tx)
        {
            return Err(error(
                AgentErrorKind::Internal,
                "duplicate remote tool request id",
                false,
                false,
            )
            .into());
        }
        let frame =
            SignalingModel::new_request(SignalingType::InvokeRemoteTool, None, Some(&request))
                .map_err(|e| {
                    error(
                        AgentErrorKind::TransportError,
                        format!("failed to build remote observation request: {e}"),
                        true,
                        true,
                    )
                })?;
        let text = serde_json::to_string(&frame).map_err(|e| {
            error(
                AgentErrorKind::TransportError,
                format!("failed to encode remote observation request: {e}"),
                true,
                true,
            )
        })?;
        if let Err(e) = target.session.write().await.text(text).await {
            self.pending.cancel(&request_id);
            return Err(error(
                AgentErrorKind::TransportError,
                format!("failed to send remote observation request: {e}"),
                true,
                true,
            )
            .into());
        }
        let output = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => {
                self.pending.cancel(&request_id);
                return Err(error(
                    AgentErrorKind::TargetOffline,
                    "remote observation result channel closed",
                    true,
                    true,
                )
                .into());
            }
            Err(_) => {
                self.pending.cancel(&request_id);
                return Err(error(
                    AgentErrorKind::Timeout,
                    "timed out waiting for the remote observation",
                    true,
                    true,
                )
                .into());
            }
        };
        let descriptor_limit = self
            .provider_registry
            .capability_for_tool(&call.name)
            .ok_or_else(|| {
                error(
                    AgentErrorKind::UnsupportedCapability,
                    "Provider tool is no longer registered",
                    false,
                    true,
                )
            })?
            .wire
            .limits
            .max_output_bytes;
        let encoded_output = serde_json::to_vec(&output).map_err(|_| {
            ProviderInvokeError::from(error(
                AgentErrorKind::Internal,
                "remote observation result cannot be measured",
                false,
                false,
            ))
        })?;
        let result_digest_sha256 = format!("{:x}", Sha256::digest(&encoded_output));
        let provider_outcome = match &output.outcome {
            AgentOutcome::Ok(_) => CapabilityDispatchOutcome::Succeeded,
            AgentOutcome::Err(_) => CapabilityDispatchOutcome::Failed,
        };
        if encoded_output.len() as u64 > grant.limits.max_bytes_per_call.min(descriptor_limit) {
            return Err(ProviderInvokeError::known(
                error(
                    AgentErrorKind::PermissionDenied,
                    "remote observation exceeded its authorized output limit",
                    false,
                    true,
                ),
                provider_outcome,
                result_digest_sha256,
            ));
        }
        if requires_objects(&call.name) {
            self.validate_original_objects().await.map_err(|error| {
                ProviderInvokeError::known(error, provider_outcome, result_digest_sha256.clone())
            })?;
        }
        match output.outcome {
            AgentOutcome::Ok(value) => {
                let output = ToolRunOutput {
                    content: serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()),
                    image_data_url: output.image.map(|image| image.data_url),
                };
                desk_diagnose_core::provider_preflight::read::limits::validate_output(
                    &self.provider_registry,
                    call,
                    &output,
                    &grant.limits,
                )
                .map_err(|error| {
                    ProviderInvokeError::known(
                        error,
                        CapabilityDispatchOutcome::Succeeded,
                        result_digest_sha256,
                    )
                })?;
                Ok(output)
            }
            AgentOutcome::Err(remote_error) => Err(ProviderInvokeError::known(
                remote_error,
                CapabilityDispatchOutcome::Failed,
                result_digest_sha256,
            )),
        }
    }
}

#[async_trait(?Send)]
impl ToolSeam for SignalDeviceAssistantTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.verified_read_labels
            .lock()
            .map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "Provider result label state is unavailable",
                    false,
                    false,
                )
            })?
            .remove(&call.id);
        let browser_read = matches!(
            call.name.as_str(),
            "browser_take_snapshot" | "browser_wait_for"
        );
        let result = if browser_read {
            match self.authorize_and_execute_browser(call).await? {
                ExecOutcome::Executed { output, .. } => Ok(output),
                ExecOutcome::Unknown(_) => Err(error(
                    AgentErrorKind::PermissionDenied,
                    "browser observation outcome is unknown and cannot be retried automatically",
                    false,
                    true,
                )),
                ExecOutcome::Rejected { reason } => Err(error(
                    AgentErrorKind::PermissionDenied,
                    reason.unwrap_or_else(|| "browser observation was rejected".into()),
                    false,
                    true,
                )),
                _ => Err(error(
                    AgentErrorKind::Internal,
                    "browser observation returned an invalid execution state",
                    false,
                    false,
                )),
            }
        } else {
            self.authorize_and_invoke(call).await
        };
        if let Err(provider_error) = &result {
            let output = ToolRunOutput {
                content: if provider_error.safe_for_model {
                    format!("tool error: {}", provider_error.message)
                } else {
                    "tool error: the tool could not complete".into()
                },
                image_data_url: None,
            };
            let (_, digest_sha256) = tool_output_fingerprint(&output)?;
            self.verified_read_labels
                .lock()
                .map_err(|_| {
                    error(
                        AgentErrorKind::Internal,
                        "Provider result label state is unavailable",
                        false,
                        false,
                    )
                })?
                .insert(
                    call.id.clone(),
                    VerifiedReadLabel {
                        digest_sha256,
                        expires_at_unix_ms: chrono::Utc::now().timestamp_millis().max(0) as u64
                            + 120_000,
                        failed: true,
                    },
                );
        }
        result
    }

    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        if call.name == "execute_confirmed_command" {
            return self.authorize_and_execute_command(call, ctx).await;
        }
        if matches!(
            call.name.as_str(),
            EXECUTE_CONFIRMED_UI_ACTION_TOOL
                | EXECUTE_CONFIRMED_RAW_INPUT_TOOL
                | "patch_live_spreadsheet_cell"
                | "replace_live_document_body"
                | "patch_live_presentation_slide"
                | "patch_selected_numbers_copy"
                | "replace_selected_pages_copy_body"
                | "patch_selected_keynote_copy"
        ) {
            return self.authorize_and_execute_semantic_action(call).await;
        }
        if matches!(
            call.name.as_str(),
            "browser_open_page"
                | "browser_navigate_page"
                | "browser_take_snapshot"
                | "browser_wait_for"
                | "browser_fill_form"
                | "browser_activate_element"
                | "prepare_gmail_web_draft_handoff"
                | "prepare_slack_web_message_handoff"
        ) {
            return self.authorize_and_execute_browser(call).await;
        }
        if call.name == "prepare_outlook_new_draft_handoff" {
            return self.authorize_and_execute_outlook_handoff(call).await;
        }
        if !matches!(
            call.name.as_str(),
            "create_text_artifact_in_selected_directory"
                | "create_workbook_from_merge_preview"
                | "create_formula_workbook_from_merge_preview"
                | "create_word_report_from_merge_preview"
                | "create_local_communication_draft"
        ) {
            return Ok(ExecOutcome::Rejected {
                reason: Some("this Device Assistant mutation is not enabled".into()),
            });
        }
        self.authorize_and_execute_artifact(call).await
    }

    async fn ack_delivery(&self, event_id: &str) -> Result<(), AgentError> {
        if SignalCapabilityGrantStore::new(self.db.clone())
            .consume_computer_result(
                event_id,
                &self.run_id,
                &self.actor_id,
                &self.target_device_id,
            )
            .await
            .map_err(|_| completion::invalid())?
        {
            return Ok(());
        }
        <crate::agent_exec::SignalAgentTools as ToolSeam>::ack_delivery(&self.exec_tools, event_id)
            .await
    }

    async fn wait_for_task(
        &self,
        action_request_id: &str,
        execution_id: &str,
    ) -> Result<WaitOutcome, AgentError> {
        if let Some(outcome) = SignalCapabilityGrantStore::new(self.db.clone())
            .wait_computer_result(
                action_request_id,
                execution_id,
                &self.run_id,
                &self.actor_id,
                &self.target_device_id,
            )
            .await
            .map_err(|_| completion::invalid())?
        {
            return Ok(outcome);
        }
        let outcome = <crate::agent_exec::SignalAgentTools as ToolSeam>::wait_for_task(
            &self.exec_tools,
            action_request_id,
            execution_id,
        )
        .await?;
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "system clock predates the Unix epoch",
                false,
                false,
            )
        })?;
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        match &outcome {
            WaitOutcome::Completed { output, .. } => {
                let (_, result_digest_sha256) = tool_output_fingerprint(output)?;
                store
                    .record_dispatch_completion(
                        &CapabilityDispatchCompletion {
                            dispatch_id: execution_id.to_string(),
                            call_id: action_request_id.to_string(),
                            generation: 1,
                            outcome: CapabilityDispatchOutcome::Succeeded,
                            result_digest_sha256,
                        },
                        now_unix_ms,
                    )
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("failed to persist background command completion: {db_error}"),
                            false,
                            false,
                        )
                    })?;
            }
            WaitOutcome::Unknown => {
                store
                    .mark_dispatch_outcome_unknown(execution_id, action_request_id, 1, now_unix_ms)
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!(
                                "failed to persist background command unknown outcome: {db_error}"
                            ),
                            false,
                            false,
                        )
                    })?;
            }
            // A receipt-bearing Provider completion already has its own durable
            // lineage. It must not be recorded as a legacy command completion.
            WaitOutcome::StillRunning
            | WaitOutcome::CompletedWithReceipt { .. }
            | WaitOutcome::FailedWithReceipt { .. }
            | WaitOutcome::UnknownWithIdentity { .. } => {}
        }
        Ok(outcome)
    }

    fn read_data_envelope(
        &self,
        call: &ToolCall,
        output: &ToolRunOutput,
    ) -> Result<Option<DataEnvelope>, AgentError> {
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let capability = registry.capability_for_tool(&call.name).ok_or_else(|| {
            error(
                AgentErrorKind::UnsupportedCapability,
                "read result has no registered Provider capability",
                false,
                true,
            )
        })?;
        let source_object_id = if capability.wire.capability_id
            == desk_diagnose_core::device_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID
        {
            let (_, digest) = Self::canonical_call_input(call)?;
            Some(format!("external_url_input:sha256:{digest}"))
        } else {
            Some(format!("{}:{}", self.target_device_id, call.id))
        };
        let observed_at_unix_ms =
            u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "system clock predates the Unix epoch",
                    false,
                    false,
                )
            })?;
        let browser_read = matches!(
            call.name.as_str(),
            "browser_take_snapshot" | "browser_wait_for"
        );
        let verified = if browser_read {
            None
        } else {
            let verified = self
                .verified_read_labels
                .lock()
                .map_err(|_| {
                    error(
                        AgentErrorKind::Internal,
                        "Provider result label state is unavailable",
                        false,
                        false,
                    )
                })?
                .get(&call.id)
                .cloned()
                .ok_or_else(|| {
                    error(
                        AgentErrorKind::Internal,
                        "Provider result was not validated",
                        false,
                        false,
                    )
                })?;
            verify_read_label(output, &verified, observed_at_unix_ms)?;
            Some(verified)
        };
        let mut envelope = desk_diagnose_core::model_message_labels::read_result_envelope(
            &registry,
            call,
            output,
            desk_diagnose_core::model_message_labels::ReadResultLabel {
                envelope_id: format!("tool-result-{}", uuid::Uuid::new_v4()),
                observation_id: format!("observation-{}", uuid::Uuid::new_v4()),
                source_object_id,
                observed_at_unix_ms,
            },
        )?;
        if let Some(verified) = &verified {
            envelope.retention.expires_at_unix_ms = Some(
                envelope
                    .retention
                    .expires_at_unix_ms
                    .unwrap_or(verified.expires_at_unix_ms)
                    .min(verified.expires_at_unix_ms),
            );
        }
        if verified.as_ref().is_some_and(|verified| verified.failed) {
            Ok(Some(envelope))
        } else if requires_objects(&call.name) {
            let mut envelope = self.object_binding()?.label(call, output, envelope)?;
            if let Some(verified) = verified {
                envelope.retention.expires_at_unix_ms = Some(
                    envelope
                        .retention
                        .expires_at_unix_ms
                        .unwrap_or(verified.expires_at_unix_ms)
                        .min(verified.expires_at_unix_ms),
                );
            }
            Ok(Some(envelope))
        } else {
            Ok(Some(envelope))
        }
    }

    fn mutating_data_envelope(
        &self,
        call: &ToolCall,
        output: &ToolRunOutput,
    ) -> Result<Option<DataEnvelope>, AgentError> {
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let capability = registry.capability_for_tool(&call.name).ok_or_else(|| {
            error(
                AgentErrorKind::UnsupportedCapability,
                "mutation result has no registered Provider capability",
                false,
                true,
            )
        })?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered capability has a provider");
        let (size_bytes, digest_sha256) = tool_output_fingerprint(output)?;
        let browser_result = call.name.starts_with("browser_");
        let created_artifact = serde_json::from_str::<ComputerActionCompleted>(&output.content)
            .ok()
            .and_then(|completion| match completion.output {
                Some(ComputerActionOutput::FileArtifact(artifact))
                    if artifact.validate().is_ok() =>
                {
                    Some(artifact)
                }
                _ => None,
            });
        let observed_at_unix_ms =
            u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "system clock predates the Unix epoch",
                    false,
                    false,
                )
            })?;
        let expires_at_unix_ms = observed_at_unix_ms.saturating_add(5 * 60 * 1000);
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("mutation-result-{}", uuid::Uuid::new_v4()),
            // This envelope labels the exact tool-result bytes sent back to the
            // model, not the artifact bytes themselves.  A created artifact is
            // represented inside those result bytes by its typed immutable
            // ContentRef; binding the surrounding message directly to that
            // ContentRef would make the sink compare metadata JSON length and
            // digest with the file length and digest.  Keep the model-facing
            // result as an exact immutable blob and let artifact-consuming
            // mutations resolve lineage from the typed result below.
            content: if browser_result {
                ContentRef::EphemeralObservation {
                    observation_id: format!("browser-result-{}", uuid::Uuid::new_v4()),
                    size_bytes,
                    expires_at_unix_ms,
                }
            } else {
                ContentRef::ImmutableBlob {
                    blob_id: format!("mutation-content-{}", uuid::Uuid::new_v4()),
                    sha256: digest_sha256.clone(),
                    size_bytes,
                    media_type: "text/plain;charset=utf-8".into(),
                }
            },
            provenance: DataProvenance {
                source_provider_id: provider.wire.provider_id.clone(),
                source_tool_name: call.name.clone(),
                source_object_id: created_artifact
                    .as_ref()
                    .map(|artifact| format!("artifact:{}", artifact.file.token))
                    .or_else(|| Some(format!("{}:{}", self.target_device_id, call.id))),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256,
            sensitivity: Sensitivity::Sensitive,
            // Effect authorization is not ExportData authorization. The model
            // egress projector must authorize the exact resolved destination.
            allowed_destinations: Vec::new(),
            retention: if created_artifact.is_some() {
                RetentionBoundary {
                    expires_at_unix_ms: None,
                    delete_with_run: false,
                }
            } else {
                RetentionBoundary {
                    expires_at_unix_ms: browser_result.then_some(expires_at_unix_ms),
                    delete_with_run: true,
                }
            },
        };
        envelope.validate().map_err(|error_value| {
            error(
                AgentErrorKind::Internal,
                format!("failed to envelope mutation result: {error_value}"),
                false,
                false,
            )
        })?;
        Ok(Some(envelope))
    }
}

async fn record_computer_action_pre_send_failure(
    store: &SignalCapabilityGrantStore,
    dispatch_id: &str,
    call_id: &str,
    now_unix_ms: u64,
    dispatch_error: &AgentError,
) -> Result<(), AgentError> {
    store
        .record_dispatch_completion(
            &CapabilityDispatchCompletion {
                dispatch_id: dispatch_id.to_string(),
                call_id: call_id.to_string(),
                generation: 1,
                outcome: CapabilityDispatchOutcome::Failed,
                result_digest_sha256: format!(
                    "{:x}",
                    Sha256::digest(dispatch_error.message.as_bytes())
                ),
            },
            now_unix_ms,
        )
        .await
        .map(|_| ())
        .map_err(|db_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to persist Computer Action pre-send failure: {db_error}"),
                false,
                false,
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_ref(token: &str, object_kind: ObjectKind) -> ObjectRef {
        ObjectRef {
            token: token.into(),
            snapshot_id: format!("snapshot-{token}"),
            object_kind,
            expires_at: "2099-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn verified_read_label_rejects_changed_or_expired_result() {
        let output = ToolRunOutput {
            content: "bounded result".into(),
            image_data_url: None,
        };
        let (_, digest_sha256) = tool_output_fingerprint(&output).unwrap();
        let verified = VerifiedReadLabel {
            digest_sha256,
            expires_at_unix_ms: 200,
            failed: false,
        };
        verify_read_label(&output, &verified, 199).unwrap();
        assert!(verify_read_label(&output, &verified, 200).is_err());
        assert!(
            verify_read_label(
                &ToolRunOutput {
                    content: "changed result".into(),
                    image_data_url: None,
                },
                &verified,
                199,
            )
            .is_err()
        );
    }

    #[test]
    fn batch_document_selection_is_exact_and_destination_is_owner_selected() {
        let source = object_ref("source", ObjectKind::File);
        let destination = object_ref("destination", ObjectKind::Directory);
        let roots = vec![source.clone(), destination.clone()];
        assert_eq!(exact_selected_batch_file(&roots).unwrap(), source);
        validate_selected_batch_destination(&roots, &destination).unwrap();

        let second_source = object_ref("second-source", ObjectKind::File);
        assert!(exact_selected_batch_file(&[roots.clone(), vec![second_source]].concat()).is_err());
        assert!(
            validate_selected_batch_destination(
                &roots,
                &object_ref("unselected", ObjectKind::Directory)
            )
            .is_err()
        );
        assert!(validate_selected_batch_destination(&roots, &source).is_err());

        let action = ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
            output: BatchDocumentOutput {
                destination_parent: destination,
                native_file_name: "copy.pages".into(),
            },
            action: DocumentLivePatchAction::ReplaceBodyText {
                text: "replacement".into(),
            },
        });
        assert_eq!(semantic_action_target_kind(&action), Some(ObjectKind::File));
    }

    #[test]
    fn capability_input_canonicalization_sorts_nested_objects() {
        let first = ToolCall {
            id: "call-1".into(),
            name: "inspect_desktop_session".into(),
            arguments_json: r#"{"z":{"b":2,"a":1},"a":true}"#.into(),
        };
        let second = ToolCall {
            arguments_json: r#"{"a":true,"z":{"a":1,"b":2}}"#.into(),
            ..first.clone()
        };
        let (first_json, first_digest) =
            SignalDeviceAssistantTools::canonical_call_input(&first).unwrap();
        let (second_json, second_digest) =
            SignalDeviceAssistantTools::canonical_call_input(&second).unwrap();
        assert_eq!(first_json, r#"{"a":true,"z":{"a":1,"b":2}}"#);
        assert_eq!(first_json, second_json);
        assert_eq!(first_digest, second_digest);
    }

    #[test]
    fn web_search_exact_input_canonicalization_expands_default_result_count() {
        let omitted = ToolCall {
            id: "search-omitted".into(),
            name: WEB_SEARCH_TOOL_NAME.into(),
            arguments_json: r#"{"query":"Rust language"}"#.into(),
        };
        let explicit = ToolCall {
            id: "search-explicit".into(),
            name: WEB_SEARCH_TOOL_NAME.into(),
            arguments_json: r#"{"max_results":5,"query":"Rust language"}"#.into(),
        };
        let (omitted_json, omitted_digest) =
            SignalDeviceAssistantTools::canonical_call_input(&omitted).unwrap();
        let (explicit_json, explicit_digest) =
            SignalDeviceAssistantTools::canonical_call_input(&explicit).unwrap();
        assert_eq!(omitted_json, r#"{"max_results":5,"query":"Rust language"}"#);
        assert_eq!(omitted_json, explicit_json);
        assert_eq!(omitted_digest, explicit_digest);
    }

    #[test]
    fn provider_risk_classifies_bounded_diagnostics_and_sensitive_reads() {
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let call = |name: &str, arguments_json: &str| ToolCall {
            id: format!("call-{name}"),
            name: name.into(),
            arguments_json: arguments_json.into(),
        };
        let session = registry
            .capability(desk_diagnose_core::device_assistant::DESKTOP_SESSION_CAPABILITY_ID)
            .unwrap();
        let office = registry
            .capability(desk_diagnose_core::device_assistant::OFFICE_DOCUMENT_CAPABILITY_ID)
            .unwrap();
        let file = registry
            .capability(desk_diagnose_core::device_assistant::FILE_METADATA_CAPABILITY_ID)
            .unwrap();
        let system = registry
            .capability(desk_diagnose_core::device_assistant::SYSTEM_INFO_CAPABILITY_ID)
            .unwrap();
        let process = registry
            .capability(desk_diagnose_core::device_assistant::SYSTEM_PROCESS_CAPABILITY_ID)
            .unwrap();
        let logs = registry
            .capability(desk_diagnose_core::device_assistant::SYSTEM_LOG_CAPABILITY_ID)
            .unwrap();
        let browser_snapshot = registry
            .capability(desk_diagnose_core::device_assistant::BROWSER_SNAPSHOT_CAPABILITY_ID)
            .unwrap();
        let browser_open = registry
            .capability(desk_diagnose_core::device_assistant::BROWSER_OPEN_CAPABILITY_ID)
            .unwrap();
        let browser_fill = registry
            .capability(desk_diagnose_core::device_assistant::BROWSER_FILL_CAPABILITY_ID)
            .unwrap();
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(
                session,
                &call("inspect_desktop_session", "{}")
            )
            .unwrap(),
            CapabilityRiskTier::R0
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(
                office,
                &call("inspect_office_selection", "{}")
            )
            .unwrap(),
            CapabilityRiskTier::R1
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(
                file,
                &call("inspect_selected_file_metadata", "{}")
            )
            .unwrap(),
            CapabilityRiskTier::R1
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(system, &call("read_system_info", "{}"))
                .unwrap(),
            CapabilityRiskTier::R0
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(process, &call("read_process_list", "{}"))
                .unwrap(),
            CapabilityRiskTier::R0
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(
                process,
                &call("read_process_list", r#"{"include_command_line":true}"#)
            )
            .unwrap(),
            CapabilityRiskTier::R1
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(logs, &call("read_recent_logs", "{}"))
                .unwrap(),
            CapabilityRiskTier::R1
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(
                browser_snapshot,
                &call("browser_take_snapshot", "{}")
            )
            .unwrap(),
            CapabilityRiskTier::R1
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(
                browser_open,
                &call("browser_open_page", "{}")
            )
            .unwrap(),
            CapabilityRiskTier::R2
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(
                browser_fill,
                &call("browser_fill_form", "{}")
            )
            .unwrap(),
            CapabilityRiskTier::R3
        );
    }

    #[test]
    fn generic_browser_fill_is_server_pinned_to_input_fallback() {
        use desk_agent_protocol::browser_control::{
            BrowserAdapterRef, BrowserElementRole, BrowserEngineKind, BrowserOrigin,
            BrowserOriginKind,
        };
        let page = BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeDevtoolsMcp,
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                browser_major_version: 151,
                browser_version: "151.0.7922.174".into(),
                adapter_id: "chrome-devtools-mcp".into(),
                adapter_version: "1.7.0".into(),
                profile_incarnation: "profile-1".into(),
                connection_revision: 7,
            },
            page_id: "page-1".into(),
            page_incarnation: "page-incarnation-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: "mail.google.com".into(),
                port: 443,
            },
            document_revision: 2,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 42,
        };
        let element = BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "subject".into(),
            role: BrowserElementRole::Textbox,
            accessible_name: "Subject".into(),
            value: None,
            element_revision: 1,
        };
        let call = ToolCall {
            id: "model-call".into(),
            name: "browser_fill_form".into(),
            arguments_json: serde_json::json!({
                "page": page,
                "fields": [{"element": element, "value": "bounded value"}]
            })
            .to_string(),
        };
        let request =
            SignalDeviceAssistantTools::browser_action_from_call(&call, "server-call").unwrap();
        assert_eq!(request.call_id, "server-call");
        assert!(matches!(
            request.action,
            BrowserAction::FillForm {
                mutation_class: BrowserMutationClass::InputFallback,
                ..
            }
        ));
    }

    #[test]
    fn slack_web_handoff_is_server_pinned_to_external_draft_without_send() {
        use desk_agent_protocol::browser_control::{
            BrowserAdapterRef, BrowserElementRole, BrowserEngineKind, BrowserOrigin,
            BrowserOriginKind,
        };
        let page = BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeDevtoolsMcp,
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                browser_major_version: 151,
                browser_version: "151.0.0.0".into(),
                adapter_id: "chrome-devtools-mcp".into(),
                adapter_version: "1.7.0".into(),
                profile_incarnation: "profile-1".into(),
                connection_revision: 7,
            },
            page_id: "page-1".into(),
            page_incarnation: "page-incarnation-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: "app.slack.com".into(),
                port: 443,
            },
            document_revision: 2,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 42,
        };
        let composer = BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "composer-1".into(),
            role: BrowserElementRole::Textbox,
            accessible_name: "Message #test".into(),
            value: None,
            element_revision: 1,
        };
        let call = ToolCall {
            id: "model-call".into(),
            name: "prepare_slack_web_message_handoff".into(),
            arguments_json: serde_json::json!({
                "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
                "page": page,
                "composer": composer,
                "body_plain_text": "Stage 5 draft verification"
            })
            .to_string(),
        };
        let request =
            SignalDeviceAssistantTools::browser_action_from_call(&call, "server-call").unwrap();
        assert!(matches!(
            request.action,
            BrowserAction::FillForm {
                fields,
                mutation_class: BrowserMutationClass::WriteExternalDraft,
                ..
            } if fields.len() == 1 && fields[0].value == "Stage 5 draft verification"
        ));
    }

    #[test]
    fn gmail_web_handoff_is_server_pinned_to_three_external_draft_fields_without_send() {
        use desk_agent_protocol::browser_control::{
            BrowserAdapterRef, BrowserElementRole, BrowserEngineKind, BrowserOrigin,
            BrowserOriginKind,
        };
        let page = BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeDevtoolsMcp,
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                browser_major_version: 151,
                browser_version: "151.0.0.0".into(),
                adapter_id: "chrome-devtools-mcp".into(),
                adapter_version: "1.7.0".into(),
                profile_incarnation: "profile-1".into(),
                connection_revision: 7,
            },
            page_id: "page-1".into(),
            page_incarnation: "page-incarnation-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: "mail.google.com".into(),
                port: 443,
            },
            document_revision: 2,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 42,
        };
        let field = |element_id: &str, accessible_name: &str| BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: element_id.into(),
            role: BrowserElementRole::Textbox,
            accessible_name: accessible_name.into(),
            value: None,
            element_revision: 1,
        };
        let mut to_field = field("to-1", "To recipients");
        to_field.role = BrowserElementRole::Combobox;
        let call = ToolCall {
            id: "model-call".into(),
            name: "prepare_gmail_web_draft_handoff".into(),
            arguments_json: serde_json::json!({
                "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
                "page": page,
                "to_field": to_field,
                "subject_field": field("subject-1", "Subject"),
                "body_field": field("body-1", "Message Body"),
                "attachment": null,
                "draft": {
                    "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
                    "recipients": [{"role": "to", "address": "alice@example.com", "display_name": null}],
                    "subject": "Stage 5 Gmail verification",
                    "body_plain_text": "Semantic draft only; do not send.",
                    "attachment_labels": []
                }
            })
            .to_string(),
        };
        let request =
            SignalDeviceAssistantTools::browser_action_from_call(&call, "server-call").unwrap();
        assert!(matches!(
            request.action,
            BrowserAction::FillForm {
                fields,
                mutation_class: BrowserMutationClass::WriteExternalDraft,
                ..
            } if fields.len() == 3
                && fields[0].element.role == BrowserElementRole::Combobox
                && fields[0].value == "alice@example.com"
                && fields[1].value == "Stage 5 Gmail verification"
                && fields[2].value == "Semantic draft only; do not send."
        ));

        let mut attachment_arguments: serde_json::Value =
            serde_json::from_str(&call.arguments_json).unwrap();
        attachment_arguments["attachment"] = serde_json::json!({
            "element": field("attachment-1", "Attach files"),
            "artifact": {
                "file": {
                    "token": "artifact-token-1",
                    "snapshot_id": "artifact-snapshot-1",
                    "object_kind": "file",
                    "expires_at": "2026-08-29T06:00:00Z"
                },
                "file_name": "report.docx",
                "media_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "size_bytes": 7,
                "digest_sha256": "a".repeat(64),
                "content": {
                    "kind": "artifact",
                    "artifact_id": "artifact-token-1",
                    "sha256": "a".repeat(64),
                    "size_bytes": 7,
                    "media_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                }
            }
        });
        attachment_arguments["draft"]["attachment_labels"] = serde_json::json!(["report.docx"]);
        let attachment_call = ToolCall {
            id: "model-call-with-attachment".into(),
            name: "prepare_gmail_web_draft_handoff".into(),
            arguments_json: attachment_arguments.to_string(),
        };
        let attachment_request = SignalDeviceAssistantTools::browser_action_from_call(
            &attachment_call,
            "server-call-with-attachment",
        )
        .unwrap();
        assert!(matches!(
            attachment_request.action,
            BrowserAction::FillFormAndUpload {
                fields,
                file_name,
                size_bytes: 7,
                mutation_class: BrowserMutationClass::WriteExternalDraft,
                ..
            } if fields.len() == 3 && file_name == "report.docx"
        ));

        let input: GmailWebDraftHandoffInput = serde_json::from_str(&call.arguments_json).unwrap();
        let mut result_page = input.page.clone();
        result_page.document_revision += 1;
        let readback =
            |field: &BrowserElementRef,
             value: &str,
             source_element_id: &str,
             container_element_id: Option<&str>,
             kind: BrowserFormReadbackKind| BrowserFormFieldReadback {
                request_element_id: field.element_id.clone(),
                request_role: field.role,
                request_accessible_name: field.accessible_name.clone(),
                source_element_id: source_element_id.into(),
                container_element_id: container_element_id.map(str::to_string),
                kind,
                value: value.into(),
            };
        let result = BrowserActionResult {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "server-call".into(),
            outcome: desk_agent_protocol::browser_control::BrowserActionOutcome::FormFilled,
            page: result_page.clone(),
            snapshot: Some(
                desk_agent_protocol::browser_control::BrowserSemanticSnapshot {
                    schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                    page: result_page,
                    elements: Vec::new(),
                    truncated: false,
                    captured_at_unix_ms: 43,
                },
            ),
            form_readback: vec![
                readback(
                    &input.to_field,
                    input.draft.recipients[0].address.as_str(),
                    "recipient-chip",
                    Some("compose-form"),
                    BrowserFormReadbackKind::CommittedText,
                ),
                readback(
                    &input.subject_field,
                    input.draft.subject.as_str(),
                    input.subject_field.element_id.as_str(),
                    Some("compose-form"),
                    BrowserFormReadbackKind::ControlValue,
                ),
                readback(
                    &input.body_field,
                    input.draft.body_plain_text.as_str(),
                    input.body_field.element_id.as_str(),
                    Some("compose-form"),
                    BrowserFormReadbackKind::ControlValue,
                ),
            ],
            completed_at_unix_ms: 44,
        };
        result.validate().unwrap();
        assert!(gmail_exact_form_readback(&result, &input));

        let attachment_input: GmailWebDraftHandoffInput =
            serde_json::from_str(&attachment_call.arguments_json).unwrap();
        let mut attachment_result = result.clone();
        attachment_result.outcome =
            desk_agent_protocol::browser_control::BrowserActionOutcome::FormFilledWithFile;
        let snapshot = attachment_result.snapshot.as_mut().unwrap();
        snapshot.elements.push(BrowserElementRef {
            page_id: snapshot.page.page_id.clone(),
            page_incarnation: snapshot.page.page_incarnation.clone(),
            document_revision: snapshot.page.document_revision,
            element_id: "attachment-chip-1".into(),
            role: BrowserElementRole::Generic,
            accessible_name: "report.docx".into(),
            value: None,
            element_revision: 1,
        });
        attachment_result.validate().unwrap();
        assert!(gmail_exact_attachment_readback(
            &attachment_result,
            &attachment_input
        ));
        attachment_result.snapshot.as_mut().unwrap().elements[0].accessible_name =
            "different.docx".into();
        assert!(!gmail_exact_attachment_readback(
            &attachment_result,
            &attachment_input
        ));

        let mut wrong_container = result;
        wrong_container.form_readback[0].container_element_id = Some("other-form".into());
        assert!(!gmail_exact_form_readback(&wrong_container, &input));
        wrong_container.form_readback[0].container_element_id = Some("compose-form".into());
        wrong_container.form_readback[2].container_element_id = Some("other-form".into());
        assert!(!gmail_exact_form_readback(&wrong_container, &input));
    }

    #[test]
    fn browser_grant_and_runtime_share_compiled_operation_scope() {
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let capability = registry
            .capability_for_tool("browser_open_page")
            .expect("browser open capability is compiled");
        let scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        )
        .expect("browser capability has a compiled grant scope");

        assert_eq!(scope.operations, vec!["use_selected_object"]);
        assert_ne!(scope.operations, vec!["browser_open_page"]);
    }

    #[test]
    fn browser_policy_auto_authorizes_only_low_risk_observation() {
        assert!(browser_policy_auto_authorized(CapabilityRiskTier::R0));
        assert!(browser_policy_auto_authorized(CapabilityRiskTier::R1));
        assert!(!browser_policy_auto_authorized(CapabilityRiskTier::R2));
        assert!(!browser_policy_auto_authorized(CapabilityRiskTier::R3));
    }

    #[test]
    fn source_binding_rejects_a_different_host() {
        let store = SignalRemoteToolPendingStore::default();
        let (tx, _rx) = oneshot::channel();
        assert!(store.register("r1".into(), "host-a".into(), tx));
        let outcome = store.fail(
            "host-b",
            "r1",
            error(AgentErrorKind::Internal, "no", false, true),
        );
        assert!(matches!(outcome, AcceptOutcome::Rejected(_)));
    }

    #[test]
    fn screen_envelope_fingerprint_binds_the_image_bytes() {
        let text_only = ToolRunOutput {
            content: "screen metadata".into(),
            image_data_url: None,
        };
        let with_image = ToolRunOutput {
            content: text_only.content.clone(),
            image_data_url: Some("data:image/jpeg;base64,AQID".into()),
        };
        let (text_size, text_digest) = tool_output_fingerprint(&text_only).unwrap();
        let (image_size, image_digest) = tool_output_fingerprint(&with_image).unwrap();
        assert!(image_size > text_size);
        assert_ne!(image_digest, text_digest);

        let mut changed = with_image;
        changed.image_data_url = Some("data:image/jpeg;base64,BAUG".into());
        assert_ne!(tool_output_fingerprint(&changed).unwrap().1, image_digest);
    }
}
