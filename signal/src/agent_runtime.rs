//! Shared OSS Signal runtime helpers for AI Assistant work.
//!
//! This module intentionally contains no Diagnose product loop or signaling
//! contract. It owns only model metering and the bounded, tool-free follow-up
//! used after a durable background execution completes.

use crate::config::connection::DatabaseConnection;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::agent_loop::{LoopDeps, LoopOutcome, resume_agent_turn};
use desk_diagnose_core::agentic_prompt::build_agentic_system_message;
use desk_diagnose_core::seam::{
    ClaimTurnParams, HeartbeatGuard, LeaseHeartbeat, ModelRequest, ModelSeam, SessionSeam,
    ToolRunOutput, ToolSeam, TurnSink,
};
use desk_diagnose_core::session::{
    AgentSessionSurface, PersistedAgentSession, TriggerOrigin, WorkKind,
};
use desk_utils::error::DeskErrorCode;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sha2::{Digest, Sha256};

use crate::model_dial::SignalModelSeam;
use crate::model_provider;

const AGENT_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const AUTO_FOLLOW_UP_MAX_STEPS: u32 = 4;

pub(crate) struct SignalStoreHeartbeat {
    pub(crate) store: crate::agent_session_store::SignalAgentSessionStore,
}

impl LeaseHeartbeat for SignalStoreHeartbeat {
    fn start(&self, conversation_id: String, lease_token: u64) -> Box<dyn HeartbeatGuard> {
        let store = self.store.clone();
        let handle = actix_web::rt::spawn(async move {
            loop {
                tokio::time::sleep(AGENT_HEARTBEAT_INTERVAL).await;
                let now = chrono::Utc::now().to_rfc3339();
                if store
                    .heartbeat(&conversation_id, lease_token, &now)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Box::new(SignalHeartbeatGuard(handle))
    }
}

struct SignalHeartbeatGuard(actix_web::rt::task::JoinHandle<()>);
impl HeartbeatGuard for SignalHeartbeatGuard {}
impl Drop for SignalHeartbeatGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub(crate) fn record_metered(
    context: Option<&desk_diagnose_core::model_observability::ObservationContext>,
) {
    if let Some(context) = context {
        context.metered(chrono::Utc::now().timestamp_millis());
    }
}

struct MeteredSignalModel {
    inner: SignalModelSeam,
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for MeteredSignalModel {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        self.inner.model_output_token_limit(request)
    }
    fn model_input_token_upper_bound(
        &self,
        request: &ModelRequest,
    ) -> Result<Option<u64>, AgentError> {
        self.inner.model_input_token_upper_bound(request)
    }

    fn observation_context(
        &self,
        use_case: desk_diagnose_core::model_profile::ModelUseCase,
        origin: desk_diagnose_core::model_observability::Origin,
    ) -> Option<desk_diagnose_core::model_observability::ObservationContext> {
        self.inner.observation_context(use_case, origin)
    }
    fn context_compression_provenance(
        &self,
        turn_id: &str,
        created_at: &str,
    ) -> Result<desk_diagnose_core::model_context::CompressorProvenanceV1, AgentError> {
        self.inner
            .context_compression_provenance(turn_id, created_at)
    }
    async fn context_policy(
        &self,
        requirements: desk_diagnose_core::model_capability::ModelRequirements,
    ) -> Result<desk_diagnose_core::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(requirements).await
    }

    fn on_model_request_projected(
        &self,
        metrics: desk_diagnose_core::seam::ModelRequestProjectionMetrics,
    ) {
        log::debug!(
            "[ai-assistant] completion projection static_bytes={} runtime_bytes={} definition_revision={} messages={} message_json_bytes={} tools={} tool_json_bytes={} conversation_messages={} session_snapshot_bytes={}",
            metrics.static_instruction_bytes,
            metrics.runtime_context_bytes,
            metrics.definition_revision,
            metrics.message_count,
            metrics.message_json_bytes,
            metrics.advertised_tool_count,
            metrics.advertised_tool_json_bytes,
            metrics.conversation_message_count,
            metrics.session_snapshot_json_bytes,
        );
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<desk_diagnose_core::chat::ModelTurn, AgentError> {
        let mut request = request;
        if request.observation.is_none() {
            request.observation = self.observation_context(
                request.use_case,
                desk_diagnose_core::model_observability::Origin::WorkCompletion,
            );
        }
        let observation = request.observation.clone();
        let turn = self.inner.call(request, sink).await?;
        record_metered(observation.as_ref());
        Ok(turn)
    }
}

struct CompletionOnlyTools;

fn export_denied() -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "The completed result is saved, but its original model export authorization is no longer available.".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

struct CompletionModel {
    command_completion: bool,
    inner: crate::assistant_model::MeteredModel,
    run_id: String,
    actor_id: String,
    device_id: String,
    event_id: String,
    export: crate::capability_grant_store::computer_export::ComputerExportContext,
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for CompletionModel {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        self.inner.model_output_token_limit(request)
    }
    fn model_input_token_upper_bound(
        &self,
        request: &ModelRequest,
    ) -> Result<Option<u64>, AgentError> {
        self.inner.model_input_token_upper_bound(request)
    }

    fn observation_context(
        &self,
        use_case: desk_diagnose_core::model_profile::ModelUseCase,
        origin: desk_diagnose_core::model_observability::Origin,
    ) -> Option<desk_diagnose_core::model_observability::ObservationContext> {
        self.inner.observation_context(use_case, origin)
    }

    fn context_compression_provenance(
        &self,
        turn_id: &str,
        created_at: &str,
    ) -> Result<desk_diagnose_core::model_context::CompressorProvenanceV1, AgentError> {
        self.inner
            .context_compression_provenance(turn_id, created_at)
    }
    fn command_completion_event_id(&self) -> Option<&str> {
        self.command_completion.then_some(self.event_id.as_str())
    }

    fn model_egress_policy(
        &self,
    ) -> Result<Option<desk_diagnose_core::model_egress::ModelEgressPolicy>, AgentError> {
        self.inner.model_egress_policy()
    }

    async fn context_policy(
        &self,
        requirements: desk_diagnose_core::model_capability::ModelRequirements,
    ) -> Result<desk_diagnose_core::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(requirements).await
    }

    fn on_model_request_projected(
        &self,
        metrics: desk_diagnose_core::seam::ModelRequestProjectionMetrics,
    ) {
        self.inner.on_model_request_projected(metrics);
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<desk_diagnose_core::chat::ModelTurn, AgentError> {
        // The initial context was loaded before claiming the turn. Recheck it
        // immediately before every model call so newer input or a detach cannot
        // reuse the old result's export selection through the claim race.
        use crate::entity::agent_session;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&self.run_id))
            .filter(agent_session::Column::ActorId.eq(&self.actor_id))
            .filter(agent_session::Column::DeviceId.eq(&self.device_id))
            .one(&self.inner.db)
            .await
            .map_err(|_| export_denied())?
            .ok_or_else(export_denied)?;
        let session =
            PersistedAgentSession::decode_json(&row.state_json).map_err(|_| export_denied())?;
        if session.version != row.version {
            return Err(export_denied());
        }
        let current_config = model_provider::load(&self.inner.db)
            .await
            .map_err(|_| export_denied())?;
        if current_config
            .destination_identity()
            .map_err(|_| export_denied())?
            != self.inner.destination
        {
            return Err(export_denied());
        }
        let export =
            crate::capability_grant_store::SignalCapabilityGrantStore::new(self.inner.db.clone())
                .completion_export(&session, &self.event_id, &self.inner.destination)
                .await
                .map_err(|_| export_denied())?;
        if export != self.export {
            return Err(export_denied());
        }
        let child_creation = crate::agent_subagent_store::SubAgentStore::new(self.inner.db.clone())
            .child_creation_context(&session)
            .await
            .map_err(|_| export_denied())?;
        if child_creation.is_some()
            && !crate::agent_subagent_store::SubAgentStore::new(self.inner.db.clone())
                .child_resume_available(&session)
                .await
                .map_err(|_| export_denied())?
        {
            return Err(export_denied());
        }
        let request = if session
            .pending_auto_triggers
            .iter()
            .any(|trigger| trigger.event_id == self.event_id && trigger.kind == WorkKind::AgentExec)
        {
            match child_creation.as_ref() {
                Some(creation) => desk_diagnose_core::command_completion::project_child_request(
                    request,
                    &session,
                    &self.event_id,
                    creation,
                )?,
                None => desk_diagnose_core::command_completion::project_request(
                    request,
                    &session,
                    &self.event_id,
                )?,
            }
        } else {
            request
        };
        let request = self
            .inner
            .model_egress_policy()?
            .ok_or_else(export_denied)?
            .authorize_request(request)
            .map_err(|error| {
                log::warn!("[ai-assistant] command completion model egress denied: {error}");
                error.agent_error()
            })?
            .request;
        // A historical replay/retention filter may omit the original tool
        // group. Never call that a reaction to a result the model cannot see.
        if !request
            .messages
            .iter()
            .any(|message| message.message_id == self.event_id)
        {
            return Err(export_denied());
        }
        self.inner.call(request, sink).await
    }
}

#[async_trait::async_trait(?Send)]
impl ToolSeam for CompletionOnlyTools {
    async fn run_read(
        &self,
        _call: &desk_diagnose_core::chat::ToolCall,
    ) -> Result<ToolRunOutput, AgentError> {
        Err(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "tools are not available in a completion follow-up".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
    }
}

struct DiscardTurnSink;

impl TurnSink for DiscardTurnSink {
    fn on_text_delta(&mut self, _delta: &str) {}
}

/// Run one bounded, tool-free model turn after a durable execution completion.
pub fn resume_completion_turn(
    db: DatabaseConnection,
    session: PersistedAgentSession,
    work_kind: desk_diagnose_core::session::WorkKind,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<LoopOutcome, AgentError>>>> {
    desk_diagnose_core::future::boxed(move || resume_completion_turn_inner(db, session, work_kind))
}

async fn resume_completion_turn_inner(
    db: DatabaseConnection,
    session: PersistedAgentSession,
    work_kind: desk_diagnose_core::session::WorkKind,
) -> Result<LoopOutcome, AgentError> {
    if session.surface == AgentSessionSurface::AiAssistant
        && !crate::ai_assistant_gate::global_ai_assistant_gate().is_enabled()
    {
        return Err(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "AI Assistant is disabled on this device".into(),
            retryable: false,
            safe_for_model: true,
            error_code: Some(DeskErrorCode::FEATURE_UNAVAILABLE.code()),
        });
    }
    let child_store = crate::agent_subagent_store::SubAgentStore::new(db.clone());
    let child_creation = child_store
        .child_creation_context(&session)
        .await
        .map_err(|_| export_denied())?;
    if child_creation.is_some()
        && (work_kind != WorkKind::AgentExec
            || !child_store
                .child_resume_available(&session)
                .await
                .map_err(|_| export_denied())?)
    {
        return Ok(LoopOutcome::TurnBusy);
    }
    let child_completion_event = if child_creation.is_some() {
        Some(
            session
                .pending_auto_triggers
                .iter()
                .find(|pending| {
                    pending.kind == WorkKind::AgentExec && pending.chain_id == session.chain_id
                })
                .ok_or_else(export_denied)?
                .event_id
                .clone(),
        )
    } else {
        None
    };
    let response_locale = session.response_locale.clone();
    let config = model_provider::load(&db).await.map_err(|error| {
        transport_error(format!("failed to load model provider config: {error}"))
    })?;
    let seam = SignalModelSeam::from_config(&config)?.with_context_db(db.clone());
    let turn_id = uuid::Uuid::new_v4().to_string();
    let model: Box<dyn ModelSeam> = if session.surface == AgentSessionSurface::AiAssistant {
        // Legacy completions lacking an original export selection stay visible
        // to the owner, but cannot mint new permission from an execution grant.
        let pending = session
            .pending_auto_triggers
            .iter()
            .find(|pending| pending.kind == work_kind && pending.chain_id == session.chain_id)
            .filter(|_| matches!(work_kind, WorkKind::ComputerAction | WorkKind::AgentExec))
            .ok_or_else(export_denied)?;
        let destination = config.destination_identity().map_err(|_| export_denied())?;
        if child_creation
            .as_ref()
            .is_some_and(|creation| creation.source.model_destination != destination)
        {
            return Err(export_denied());
        }
        let export = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
            .completion_export(&session, &pending.event_id, &destination)
            .await
            .map_err(|_| export_denied())?;
        Box::new(CompletionModel {
            command_completion: pending.kind == WorkKind::AgentExec,
            inner: crate::assistant_model::MeteredModel {
                fresh_task: None,
                inner: seam,
                db: db.clone(),
                destination,
                selected_source_tools: export.selected_source_tools.clone(),
                export_authorization_id: format!(
                    "completion-export-{:x}",
                    Sha256::digest(
                        format!("{}:{turn_id}", export.export_authorization_id).as_bytes()
                    )
                ),
                permission_resume: false,
                completed_compression_receipt: std::cell::RefCell::new(None),
                model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
            },
            run_id: session.conversation_id.clone(),
            actor_id: session.actor_id.clone(),
            device_id: session.device_id.clone(),
            event_id: pending.event_id.clone(),
            export,
        })
    } else {
        Box::new(MeteredSignalModel { inner: seam })
    };
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(session.client_conversation_id.clone(), session.surface);
    let heartbeat = SignalStoreHeartbeat {
        store: sessions.clone(),
    };
    let clock = || chrono::Utc::now().to_rfc3339();
    let claim = ClaimTurnParams {
        conversation_id: session.conversation_id,
        actor_id: session.actor_id,
        device_id: session.device_id,
        policy_revision: session.policy_revision,
        current_pdp_scope: session.scope_snapshot,
        turn_id,
        request_id: session.current_request_id,
        connection_id: None,
        trigger_origin: TriggerOrigin::WorkCompletion { kind: work_kind },
        now: clock(),
    };
    let tools = CompletionOnlyTools;
    let registry = Vec::new();
    let deps = LoopDeps {
        session_seam: &sessions,
        model: model.as_ref(),
        tools: &tools,
        content_safety: desk_diagnose_core::content_safety::ContentSafetyMode::Disabled,
        registry: &registry,
        provider_registry: None,
        capability_inventory: None,
        capability_permission_candidates: &[],
        capability_catalog_metrics: None,
        permission_continuation_exact_tools: &[],
        response_format: desk_diagnose_core::prompt::ResponseFormatSpec::None,
        system_prompt: build_agentic_system_message(None),
        response_locale,
        interactive_user_home: None,
        interactive_user_home_incarnation: None,
        max_steps_per_turn: config.max_steps_per_turn.min(AUTO_FOLLOW_UP_MAX_STEPS),
        max_same_tool_per_turn: config
            .max_same_tool_calls_per_turn
            .min(AUTO_FOLLOW_UP_MAX_STEPS),
        clock: &clock,
        heartbeat: Some(&heartbeat),
    };
    let mut sink = DiscardTurnSink;
    match child_completion_event {
        Some(event_id) => {
            let destination = config.destination_identity().map_err(|_| export_denied())?;
            match child_store
                .claim_child_completion(&claim, &destination, &event_id)
                .await
                .map_err(|_| export_denied())?
            {
                Some(claimed) => {
                    desk_diagnose_core::agent_loop::run_preclaimed_child_completion(
                        &deps, claimed, &mut sink,
                    )
                    .await
                }
                None => Ok(LoopOutcome::TurnBusy),
            }
        }
        None => resume_agent_turn(&deps, claim, &mut sink).await,
    }
}
