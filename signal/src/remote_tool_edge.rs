//! Single-node OSS Signal remote read-tool transport.
//!
//! The Device Assistant loop runs in Signal while the Windows observer runs in
//! the host worker. This module sends one server-stamped, read-only
//! `RemoteToolRequest` to the exact host connection, source-binds and strictly
//! reassembles its chunked response, and returns the already-redacted result to
//! the shared agent loop. There is intentionally no mutation method or action
//! transport in this seam.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use desk_agent_protocol::authz::{
    AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthorizedControlPayload, AuthzActor,
    AuthzDevice, ExecAdmissionPolicy,
};
use desk_agent_protocol::capability_grant::{
    CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer, CapabilityGrantLimits,
    CapabilityGrantUsePolicy, CapabilityRiskTier,
};
use desk_agent_protocol::capability_provider::{
    AuthorizationResourceKind, CapabilityDataCategory, ExecutionLocality, ProductSurface,
};
use desk_agent_protocol::computer_use::{
    COMPUTER_USE_SCHEMA_VERSION, ComputerActionCompleted, ComputerActionKind,
    ComputerActionResultClass, ComputerActionStarted, ComputerActionStep, ComputerUseAdapterKind,
    ComputerUseAdapterRef, FileContentReadParams, FileMetadataInspectParams, FilePatchAction,
    ObjectKind, ObjectRef, OfficeInspectParams, SealedComputerActionPlan,
    SpreadsheetFileInspectParams, SpreadsheetMergePreviewParams, TerminalOutputInspectParams,
};
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
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
    CapabilityGrantCall, canonical_compiled_scope, exact_external_query_resource_scope,
    exact_external_url_resource_scope, fresh_object_resource_scope, match_capability_grant,
};
use desk_diagnose_core::capability_risk::{CapabilityRiskSignals, classify_capability_risk};
use desk_diagnose_core::chat::ToolCall;
use desk_diagnose_core::chunk::ByteReassembler;
use desk_diagnose_core::device_assistant::{PREVIEW_COMPUTER_ACTION_TOOL, validate_preview_call};
use desk_diagnose_core::provider_registry::ProviderRegistry;
use desk_diagnose_core::read_tools::build_read_operation;
use desk_diagnose_core::seam::{ExecContext, ExecOutcome, ToolRunOutput, ToolSeam};
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
}

impl SignalComputerActionObserver {
    pub fn new(pending: Arc<SignalComputerActionPendingStore>) -> Self {
        Self { pending }
    }
}

impl ComputerActionObserver for SignalComputerActionObserver {
    fn on_computer_action_lifecycle<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let source_id = source.model.connection_id.as_str();
            match model.signaling_type {
                SignalingType::ComputerActionStarted => {
                    if let Ok(started) = model.get_data::<ComputerActionStarted>() {
                        let _ = self.pending.note_started(source_id, &started);
                    }
                }
                SignalingType::ComputerActionCompleted => {
                    if let Ok(completed) = model.get_data::<ComputerActionCompleted>() {
                        let _ = self.pending.complete(source_id, completed);
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

/// Per-turn read-only tool seam for the OSS Device Assistant.
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
    selected_file_roots: Vec<ObjectRef>,
    selected_spreadsheet_roots: Vec<ObjectRef>,
    selected_terminal_roots: Vec<ObjectRef>,
    current_user_message: String,
    run_id: String,
    turn_id: String,
    policy_revision: i64,
    readiness_revision: u64,
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
        selected_file_roots: Vec<ObjectRef>,
        selected_spreadsheet_roots: Vec<ObjectRef>,
        selected_terminal_roots: Vec<ObjectRef>,
        current_user_message: String,
        run_id: String,
        turn_id: String,
        policy_revision: i64,
        readiness_revision: u64,
    ) -> Self {
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
            selected_file_roots,
            selected_spreadsheet_roots,
            selected_terminal_roots,
            current_user_message,
            run_id,
            turn_id,
            policy_revision,
            readiness_revision,
        }
    }

    fn canonical_call_input(call: &ToolCall) -> Result<(String, String), AgentError> {
        fn sort_json(value: serde_json::Value) -> serde_json::Value {
            match value {
                serde_json::Value::Array(values) => {
                    serde_json::Value::Array(values.into_iter().map(sort_json).collect())
                }
                serde_json::Value::Object(values) => {
                    let mut entries = values.into_iter().collect::<Vec<_>>();
                    entries.sort_by(|left, right| left.0.cmp(&right.0));
                    serde_json::Value::Object(
                        entries
                            .into_iter()
                            .map(|(key, value)| (key, sort_json(value)))
                            .collect(),
                    )
                }
                scalar => scalar,
            }
        }

        let value = serde_json::from_str(&call.arguments_json).map_err(|decode_error| {
            error(
                AgentErrorKind::InvalidInput,
                format!("invalid Provider tool input: {decode_error}"),
                false,
                true,
            )
        })?;
        let canonical = serde_json::to_string(&sort_json(value)).map_err(|encode_error| {
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
                    "select at least one spreadsheet file before inspecting it",
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
    ) -> CapabilityRiskTier {
        let sensitive_content = capability.wire.data_policy.reads.iter().any(|category| {
            !matches!(
                category,
                CapabilityDataCategory::UserRequest
                    | CapabilityDataCategory::DesktopSessionMetadata
                    | CapabilityDataCategory::FileMetadata
            )
        });
        classify_capability_risk(
            capability.wire.effect,
            CapabilityRiskSignals {
                sensitive_content,
                external_egress: capability.wire.data_policy.may_export_data,
                destructive_or_overwrite: false,
                unpredictable_input: false,
            },
        )
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
        if capability.wire.authorization_hint.resources
            == [desk_agent_protocol::capability_provider::AuthorizationResourceKind::FreshObjectReference]
        {
            resource_scope = match capability.required_capability {
                desk_agent_protocol::Capability::FileMetadataRead => {
                    fresh_object_resource_scope(&self.selected_file_roots)
                }
                desk_agent_protocol::Capability::FileContentRead => fresh_object_resource_scope(
                    &self
                        .selected_file_roots
                        .iter()
                        .filter(|object_ref| object_ref.object_kind == ObjectKind::File)
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                desk_agent_protocol::Capability::SpreadsheetFileInspect
                | desk_agent_protocol::Capability::SpreadsheetMergePreview => {
                    fresh_object_resource_scope(&self.selected_spreadsheet_roots)
                }
                desk_agent_protocol::Capability::TerminalOutputRead => {
                    fresh_object_resource_scope(&self.selected_terminal_roots)
                }
                _ => resource_scope,
            };
        }
        let operation_scope =
            compiled_scope.map_or_else(|| vec!["observe".to_string()], |scope| scope.operations);
        let risk_tier = Self::capability_risk(capability);
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
                expires_at_unix_ms: now_unix_ms.saturating_add(120_000),
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
        store.prepare(prepare()).await.map_err(|db_error| {
            error(
                AgentErrorKind::PermissionDenied,
                format!("Provider call authorization failed: {db_error}"),
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

        let result = if call.name == PREVIEW_COMPUTER_ACTION_TOOL {
            validate_preview_call(call).map(|content| ToolRunOutput {
                content,
                image_data_url: None,
            })
        } else {
            self.invoke(call).await
        };
        match result {
            Ok(output) => {
                let (_, result_digest_sha256) = tool_output_fingerprint(&output)?;
                let completion = CapabilityDispatchCompletion {
                    dispatch_id: dispatch_id.clone(),
                    call_id: server_call_id.clone(),
                    generation: 1,
                    outcome: CapabilityDispatchOutcome::Succeeded,
                    result_digest_sha256,
                };
                if let Err(db_error) = store
                    .record_dispatch_completion(&completion, now_unix_ms)
                    .await
                {
                    let _ = store
                        .mark_dispatch_outcome_unknown(
                            &dispatch_id,
                            &server_call_id,
                            1,
                            now_unix_ms,
                        )
                        .await;
                    return Err(error(
                        AgentErrorKind::Internal,
                        format!("Provider result could not be persisted safely: {db_error}"),
                        false,
                        false,
                    ));
                }
                Ok(output)
            }
            Err(provider_error) => {
                store
                    .mark_dispatch_outcome_unknown(&dispatch_id, &server_call_id, 1, now_unix_ms)
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("Provider unknown outcome could not be persisted: {db_error}"),
                            false,
                            false,
                        )
                    })?;
                Err(provider_error)
            }
        }
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
        }
        enum ArtifactRequest {
            Text(TextArgs),
            Spreadsheet(SpreadsheetArgs),
            SpreadsheetFormula {
                args: SpreadsheetFormulaArgs,
                policy_digest_sha256: String,
            },
            Word(WordArgs),
        }

        let (args, required_capability, operation, orchestrator_grant) = match call.name.as_str() {
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
            "create_word_report_from_merge_preview" => (
                ArtifactRequest::Word(serde_json::from_str(&call.arguments_json).map_err(
                    |decode_error| {
                        error(
                            AgentErrorKind::InvalidInput,
                            format!("invalid Word report artifact Provider input: {decode_error}"),
                            false,
                            true,
                        )
                    },
                )?),
                desk_agent_protocol::Capability::WordDocumentCreateConfirmed,
                "create_new_artifact",
                desk_diagnose_core::device_assistant::WORD_DOCUMENT_CREATE_CAPABILITY_ID,
            ),
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
        let provider = self
            .provider_registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered artifact capability has a Provider");
        self.verify_current_readiness(capability).await?;
        let session = self.authoritative_session().await?;
        let (canonical_input_json, canonical_input_digest_sha256) =
            Self::canonical_call_input(call)?;
        let resource_scope = fresh_object_resource_scope(&selected_directories);
        let operation_scope = vec![operation.to_string()];
        let risk_tier = Self::capability_risk(capability);
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
        };
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

        let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&self.target_connection_id, chrono::Utc::now())
            .ok_or_else(|| {
                error(
                    AgentErrorKind::TargetOffline,
                    "artifact readiness expired before dispatch",
                    false,
                    true,
                )
            })?;
        let generation = dispatch_id.clone();
        let (action, before_summary, after_intent, verification) = match args {
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
                }),
                "new Word report does not exist in the selected directory".into(),
                "materialize the retained merge preview as one new deterministic macro-free DOCX without overwrite".into(),
                "reopen through the retained parent handle and verify the exact generated DOCX bytes plus SHA-256".into(),
            ),
        };
        let plan = SealedComputerActionPlan {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            work_id: claimed.work_id.to_string(),
            action_request_id: server_call_id.clone(),
            execution_generation: generation.clone(),
            device_id: self.target_device_id.clone(),
            interactive_session_incarnation: readiness.readiness.interactive_session_incarnation,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::FileSystem,
                version: desk_diagnose_core::device_assistant::FILE_ARTIFACT_ADAPTER_VERSION.into(),
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
        plan.validate().map_err(|validation_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to seal artifact plan: {validation_error}"),
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
                "target device binding changed before artifact dispatch",
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
            inner: serde_json::to_value(&plan).map_err(|encode_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to encode artifact plan: {encode_error}"),
                    false,
                    false,
                )
            })?,
            authz,
        };
        let frame = SignalingModel::new_request(
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
        })?;
        let mut frame = frame;
        frame.request_id = generation.clone();
        let text = serde_json::to_string(&frame).map_err(|encode_error| {
            error(
                AgentErrorKind::TransportError,
                format!("failed to encode artifact frame: {encode_error}"),
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
                "duplicate artifact execution generation",
                false,
                false,
            ));
        }
        if target.session.write().await.text(text).await.is_err() {
            global_computer_action_pending().cancel(&generation);
            store
                .mark_dispatch_outcome_unknown(&dispatch_id, &server_call_id, 1, now_unix_ms)
                .await
                .map_err(|db_error| {
                    error(
                        AgentErrorKind::Internal,
                        format!("failed to persist artifact unknown outcome: {db_error}"),
                        false,
                        false,
                    )
                })?;
            return Ok(ExecOutcome::Unknown(
                desk_diagnose_core::session::ActionIdentity::new(
                    prepared.work_id,
                    server_call_id,
                    generation,
                    desk_diagnose_core::session::WorkKind::ComputerAction,
                ),
            ));
        }
        let completion = match tokio::time::timeout(self.timeout, completion_rx).await {
            Ok(Ok(Ok(completion))) => completion,
            _ => {
                global_computer_action_pending().cancel(&generation);
                store
                    .mark_dispatch_outcome_unknown(&dispatch_id, &server_call_id, 1, now_unix_ms)
                    .await
                    .map_err(|db_error| {
                        error(
                            AgentErrorKind::Internal,
                            format!("failed to persist artifact unknown outcome: {db_error}"),
                            false,
                            false,
                        )
                    })?;
                return Ok(ExecOutcome::Unknown(
                    desk_diagnose_core::session::ActionIdentity::new(
                        prepared.work_id,
                        server_call_id,
                        generation,
                        desk_diagnose_core::session::WorkKind::ComputerAction,
                    ),
                ));
            }
        };
        let verified = completion.result == ComputerActionResultClass::Verified
            && completion
                .facts
                .iter()
                .all(|fact| fact.changed && fact.verified);
        let output = serde_json::to_string(&completion).map_err(|encode_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to encode artifact result: {encode_error}"),
                false,
                false,
            )
        })?;
        let completion_record = CapabilityDispatchCompletion {
            dispatch_id: dispatch_id.clone(),
            call_id: server_call_id.clone(),
            generation: 1,
            outcome: if verified {
                CapabilityDispatchOutcome::Succeeded
            } else {
                CapabilityDispatchOutcome::Failed
            },
            result_digest_sha256: format!("{:x}", Sha256::digest(output.as_bytes())),
        };
        store
            .record_dispatch_completion(&completion_record, now_unix_ms)
            .await
            .map_err(|db_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to persist artifact completion: {db_error}"),
                    false,
                    false,
                )
            })?;
        if verified {
            Ok(ExecOutcome::Executed {
                output: ToolRunOutput {
                    content: output,
                    image_data_url: None,
                },
                event_id: None,
            })
        } else {
            Err(error(
                AgentErrorKind::InvalidInput,
                completion
                    .message
                    .unwrap_or_else(|| "artifact Provider did not verify the change".into()),
                false,
                true,
            ))
        }
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        if call.name == WEB_FETCH_TOOL_NAME {
            let validated = validate_fetch_call(call, &self.current_user_message)?;
            return fetch_public_web_page(validated).await;
        }
        if call.name == WEB_SEARCH_TOOL_NAME {
            let validated = validate_search_call(call, &self.current_user_message)?;
            return search_public_web(validated).await;
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
        if capability == desk_agent_protocol::Capability::FileMetadataRead {
            if self.selected_file_roots.is_empty() {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "no active file attachment was selected for this turn",
                    false,
                    true,
                ));
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
                    ));
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
                ));
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
                    "select at least one spreadsheet file before inspecting it",
                    false,
                    true,
                ));
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
                    "select at least one spreadsheet file before previewing a merge",
                    false,
                    true,
                ));
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
                ));
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
                ));
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
        ) {
            return Err(error(
                AgentErrorKind::UnsupportedCapability,
                "Device Assistant may only invoke selected read-only observations",
                false,
                true,
            ));
        }
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
                expires_at: None,
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
            ));
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
            ));
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
                ));
            }
            Err(_) => {
                self.pending.cancel(&request_id);
                return Err(error(
                    AgentErrorKind::Timeout,
                    "timed out waiting for the remote observation",
                    true,
                    true,
                ));
            }
        };
        match output.outcome {
            AgentOutcome::Ok(value) => Ok(ToolRunOutput {
                content: serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()),
                image_data_url: output.image.map(|image| image.data_url),
            }),
            AgentOutcome::Err(remote_error) => Err(remote_error),
        }
    }
}

#[async_trait(?Send)]
impl ToolSeam for SignalDeviceAssistantTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.authorize_and_invoke(call).await
    }

    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        _ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        if !matches!(
            call.name.as_str(),
            "create_text_artifact_in_selected_directory"
                | "create_workbook_from_merge_preview"
                | "create_formula_workbook_from_merge_preview"
                | "create_word_report_from_merge_preview"
        ) {
            return Ok(ExecOutcome::Rejected {
                reason: Some("this Device Assistant mutation is not enabled".into()),
            });
        }
        self.authorize_and_execute_artifact(call).await
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
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("registered capability has a provider");
        let now = chrono::Utc::now().timestamp_millis();
        let observed_at_unix_ms = u64::try_from(now).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "system clock predates the Unix epoch",
                false,
                false,
            )
        })?;
        let expires_at_unix_ms = observed_at_unix_ms.saturating_add(5 * 60 * 1000);
        let (size_bytes, digest_sha256) = tool_output_fingerprint(output)?;
        let source_object_id = if capability.wire.capability_id
            == desk_diagnose_core::device_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID
        {
            let (_, canonical_input_digest_sha256) = Self::canonical_call_input(call)?;
            Some(format!(
                "external_url_input:sha256:{canonical_input_digest_sha256}"
            ))
        } else {
            Some(format!("{}:{}", self.target_device_id, call.id))
        };
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("tool-result-{}", uuid::Uuid::new_v4()),
            content: ContentRef::EphemeralObservation {
                observation_id: format!("observation-{}", uuid::Uuid::new_v4()),
                size_bytes,
                expires_at_unix_ms,
            },
            provenance: DataProvenance {
                source_provider_id: provider.wire.provider_id.clone(),
                source_tool_name: call.name.clone(),
                source_object_id,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256,
            sensitivity: if capability.wire.capability_id
                == desk_diagnose_core::device_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID
            {
                Sensitivity::Public
            } else if capability.wire.capability_id
                == desk_diagnose_core::device_assistant::DESKTOP_SESSION_CAPABILITY_ID
            {
                Sensitivity::UserContent
            } else {
                Sensitivity::Sensitive
            },
            // Read permission does not imply ExportData. The pre-model
            // authorizer must add one exact destination under an explicit grant.
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(expires_at_unix_ms),
                delete_with_run: true,
            },
        };
        envelope.validate().map_err(|error_value| {
            error(
                AgentErrorKind::Internal,
                format!("failed to envelope read result: {error_value}"),
                false,
                false,
            )
        })?;
        Ok(Some(envelope))
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
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("mutation-result-{}", uuid::Uuid::new_v4()),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("mutation-content-{}", uuid::Uuid::new_v4()),
                sha256: digest_sha256.clone(),
                size_bytes,
                media_type: "text/plain;charset=utf-8".into(),
            },
            provenance: DataProvenance {
                source_provider_id: provider.wire.provider_id.clone(),
                source_tool_name: call.name.clone(),
                source_object_id: Some(format!("{}:{}", self.target_device_id, call.id)),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256,
            sensitivity: Sensitivity::Sensitive,
            // Effect authorization is not ExportData authorization. The model
            // egress projector must authorize the exact resolved destination.
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn provider_risk_keeps_only_bounded_session_metadata_at_r0() {
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let session = registry
            .capability(desk_diagnose_core::device_assistant::DESKTOP_SESSION_CAPABILITY_ID)
            .unwrap();
        let office = registry
            .capability(desk_diagnose_core::device_assistant::OFFICE_DOCUMENT_CAPABILITY_ID)
            .unwrap();
        let file = registry
            .capability(desk_diagnose_core::device_assistant::FILE_METADATA_CAPABILITY_ID)
            .unwrap();
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(session),
            CapabilityRiskTier::R0
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(office),
            CapabilityRiskTier::R1
        );
        assert_eq!(
            SignalDeviceAssistantTools::capability_risk(file),
            CapabilityRiskTier::R1
        );
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
