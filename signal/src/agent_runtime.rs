//! Shared OSS Signal runtime helpers for Device Assistant work.
//!
//! This module intentionally contains no Diagnose product loop or signaling
//! contract. It owns only model metering and the bounded, tool-free follow-up
//! used after a durable background execution completes.

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::agent_loop::{LoopDeps, LoopOutcome, resume_agent_turn};
use desk_diagnose_core::agentic_prompt::build_agentic_system_message;
use desk_diagnose_core::seam::{
    ClaimTurnParams, HeartbeatGuard, LeaseHeartbeat, ModelRequest, ModelSeam, SessionSeam,
    ToolRunOutput, ToolSeam, TurnSink,
};
use desk_diagnose_core::session::{PersistedAgentSession, TriggerOrigin};
use sea_orm::DatabaseConnection;

use crate::ai_usage::{self, AiUsageDelta};
use crate::model_dial::SignalModelSeam;
use crate::model_provider;

const AGENT_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const AUTO_FOLLOW_UP_MAX_STEPS: u32 = 4;

struct SignalStoreHeartbeat {
    store: crate::agent_session_store::SignalAgentSessionStore,
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

/// Best-effort hourly accounting shared by every central model dial.
pub(crate) async fn record_usage(
    db: &DatabaseConnection,
    model_name: &str,
    usage: &desk_diagnose_core::chat::TokenUsage,
) {
    let delta = AiUsageDelta {
        model_name: model_name.to_string(),
        input_tokens: usage.input_tokens.unwrap_or(0),
        output_tokens: usage.output_tokens.unwrap_or(0),
        cache_read_tokens: usage.cache_read_tokens.unwrap_or(0),
        cache_write_tokens: usage.cache_write_tokens.unwrap_or(0),
        request_count: 1,
    };
    let bucket = ai_usage::truncate_to_hour(chrono::Utc::now());
    if let Err(error) = ai_usage::upsert_ai_usage(db, bucket, &delta).await {
        log::warn!("[device-assistant] failed to record model usage: {error}");
    }
}

struct MeteredSignalModel {
    inner: SignalModelSeam,
    db: DatabaseConnection,
    model_name: String,
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for MeteredSignalModel {
    async fn context_policy(
        &self,
        requirements: desk_diagnose_core::model_capability::ModelRequirements,
    ) -> Result<desk_diagnose_core::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(requirements).await
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<desk_diagnose_core::chat::ModelTurn, AgentError> {
        let turn = self.inner.call(request, sink).await?;
        record_usage(&self.db, &self.model_name, &turn.usage).await;
        Ok(turn)
    }
}

struct CompletionOnlyTools;

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
pub async fn resume_completion_turn(
    db: DatabaseConnection,
    session: PersistedAgentSession,
    work_kind: desk_diagnose_core::session::WorkKind,
) -> Result<LoopOutcome, AgentError> {
    let config = model_provider::load(&db).await.map_err(|error| {
        transport_error(format!("failed to load model provider config: {error}"))
    })?;
    let seam = SignalModelSeam::from_config(&config)?;
    let model = MeteredSignalModel {
        inner: seam,
        db: db.clone(),
        model_name: config.model.clone().unwrap_or_default(),
    };
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db);
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
        turn_id: uuid::Uuid::new_v4().to_string(),
        request_id: session.current_request_id,
        connection_id: None,
        trigger_origin: TriggerOrigin::WorkCompletion { kind: work_kind },
        now: clock(),
    };
    let tools = CompletionOnlyTools;
    let registry = Vec::new();
    let deps = LoopDeps {
        session_seam: &sessions,
        model: &model,
        tools: &tools,
        content_safety: desk_diagnose_core::content_safety::ContentSafetyMode::Disabled,
        registry: &registry,
        provider_registry: None,
        capability_inventory: None,
        response_format: desk_diagnose_core::prompt::ResponseFormatSpec::None,
        system_prompt: build_agentic_system_message(None),
        max_steps_per_turn: config.max_steps_per_turn.min(AUTO_FOLLOW_UP_MAX_STEPS),
        max_same_tool_per_turn: config
            .max_same_tool_calls_per_turn
            .min(AUTO_FOLLOW_UP_MAX_STEPS),
        clock: &clock,
        heartbeat: Some(&heartbeat),
    };
    let mut sink = DiscardTurnSink;
    resume_agent_turn(&deps, claim, &mut sink).await
}
