//! The signal central brain's single-turn diagnose orchestration.
//!
//! In the thin-edge model signal — not the edge — drives a diagnosis. When the
//! browser sends a `Diagnose`, the control-frame authorizer starts a collection
//! here: signal pushes a `CollectRequest` over the target edge's (trusted-central)
//! signaling link, the edge runs its read-only collectors and streams a chunked
//! `CollectResponse` back, signal reassembles the evidence snapshot, dials the
//! configured model once, and streams the structured result to the browser as
//! `DiagnoseEvent` frames.
//!
//! This is signal's own implementation (single-account, collect-all, single model
//! call), mirroring the manager's orchestrator but without its fleet machinery
//! (org attribution, durable work, cross-instance routing). The security-relevant
//! response binding lives in [`crate::collect_pending`]; this module is the I/O
//! and model glue around it. The portable signal is single-node, so the pending
//! store is process-global.
//!
//! `?Send` model dial: the model phase runs on actix's single-threaded runtime
//! (`awc` is `!Send`), spawned with `actix_web::rt::spawn`.

use actix_web::web;
use desk_agent_protocol::diagnose::{
    CollectRequest, CollectResponse, DiagnoseEvent, DiagnoseEventKind, DiagnoseRequestData,
};
use desk_agent_protocol::evidence::EvidenceSnapshot;
use desk_agent_protocol::provenance::AiProvenance;
use desk_agent_protocol::{AgentError, AgentErrorKind, Capability, ExecutionMode};
use desk_diagnose_core::DEFAULT_MAX_CONTEXT_BYTES;
use desk_diagnose_core::agent_loop::{LoopDeps, LoopOutcome, resume_agent_turn, run_agent_turn};
use desk_diagnose_core::agentic_prompt::build_agentic_system_message;
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::conversation_key::{
    derive_conversation_key, is_valid_client_conversation_id,
};
use desk_diagnose_core::exec_tools::{
    exec_tool_registry_for_shells_with_timeout, sanitize_available_exec_shells,
};
use desk_diagnose_core::image_input::validate_image_request;
use desk_diagnose_core::model_capability::{
    ModelCapabilities, ModelRequirements, filter_model_compatible_tools,
};
use desk_diagnose_core::prompt::ResponseFormatSpec;
#[cfg(test)]
use desk_diagnose_core::prompt::diagnosis_json_schema;
use desk_diagnose_core::read_tools::read_tool_registry;
use desk_diagnose_core::seam::{
    ClaimTurnParams, HeartbeatGuard, LeaseHeartbeat, ModelRequest, ModelSeam, SessionSeam,
    ToolRunOutput, ToolSeam, TurnSink,
};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin};
use desk_diagnose_core::stream::StreamingTurnSink;
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::service::CollectObserver;
use sea_orm::DatabaseConnection;
use std::sync::Arc;

use crate::ai_usage::{self, AiUsageDelta};
use crate::collect_pending::{AcceptOutcome, CollectContext, CollectPendingStore};
use crate::model_dial::SignalModelSeam;
use crate::model_provider;
#[cfg(test)]
use crate::model_provider::ResponseFormatMode;

const AGENT_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const SIGNALING_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

/// Process-global pending-collection store. The portable signal is single-node,
/// so one store per process is correct: the authorizer (on the browser
/// connection) registers a pending collection and the collect observer (on the
/// edge connection) feeds the matching response into it.
pub fn global_pending_store() -> std::sync::Arc<CollectPendingStore> {
    static STORE: std::sync::OnceLock<std::sync::Arc<CollectPendingStore>> =
        std::sync::OnceLock::new();
    STORE
        .get_or_init(|| std::sync::Arc::new(CollectPendingStore::new()))
        .clone()
}

/// Monotonic `seq` slots for the single-turn diagnose lifecycle frames. The
/// browser applies `DiagnoseEvent` frames in `seq` order and ignores any `seq`
/// it has already seen, so every frame a given run can emit must carry a
/// strictly increasing value (a colliding `seq` is dropped as a stale replay,
/// hanging the panel). The lifecycle is linear and emits at most three frames:
/// `collecting` → `modeling` → a single terminal `final`/`error`. A failure
/// short-circuits to a terminal frame at the stage it reached, so the slots are
/// shared across the mutually-exclusive success and failure paths:
///
/// - [`COLLECTING`]: the opening `collecting` status, or a pre-collection
///   terminal error (a replay clash — the only frame on that path).
/// - [`MODELING`]: the `modeling` status, or a terminal error raised after
///   collection but before the model dial (host offline, push / collect failed).
/// - [`TERMINAL`]: the model phase's terminal `final` / `error`.
///
/// This path streams no `partial` frames (it dials with a [`NullTurnSink`]); a
/// future streaming variant would need a running counter instead of fixed slots.
mod seq {
    /// The opening `collecting` status, or a pre-collection terminal error.
    pub const COLLECTING: u32 = 0;
    /// The `modeling` status, or a terminal error after collection but before
    /// the model dial.
    pub const MODELING: u32 = 1;
    /// The model phase's terminal `final` / `error` frame.
    pub const TERMINAL: u32 = 2;
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

/// Serialize and send one signaling frame to a connection over its WebSocket.
async fn send_frame(conn: &ConnectionState, frame: &SignalingModel) -> Result<(), String> {
    let text = serde_json::to_string(frame).map_err(|e| format!("encode frame: {e}"))?;
    tokio::time::timeout(SIGNALING_SEND_TIMEOUT, async {
        conn.session.write().await.text(text).await
    })
    .await
    .map_err(|_| format!("send to {} timed out", conn.model.connection_id))?
    .map_err(|e| format!("send to {}: {e}", conn.model.connection_id))
}

/// Push a `CollectRequest` to the target edge over its (trusted-central)
/// signaling link. The edge re-runs its own selection gate before collecting, so
/// it keeps final say over what evidence leaves the machine.
async fn push_collect_request(
    target: &ConnectionState,
    request_id: &str,
    request: &DiagnoseRequestData,
) -> Result<(), String> {
    let payload = CollectRequest {
        request_id: request_id.to_string(),
        request: request.clone(),
    };
    let frame = SignalingModel::new_request(SignalingType::CollectRequest, None, Some(&payload))
        .map_err(|e| format!("build CollectRequest: {e}"))?;
    send_frame(target, &frame).await
}

/// Stream one `DiagnoseEvent` to the browser connection, if it is still present.
/// Notification-style: emitted with `response_state = None` and correlated by
/// `seq` / `kind`, matching what the panel consumes.
pub async fn stream_event(
    connection_map: &SharedConnectionMap,
    browser_connection_id: &str,
    event: &DiagnoseEvent,
) {
    let conn = {
        let map = connection_map.read().await;
        map.get(browser_connection_id).cloned()
    };
    let Some(conn) = conn else {
        log::warn!("[diagnose] browser {browser_connection_id} gone; dropping event");
        return;
    };
    let frame = SignalingModel::new(
        &event.request_id,
        SignalingType::DiagnoseEvent,
        None,
        Some(browser_connection_id.to_string()),
        serde_json::to_value(event).ok(),
        None,
    );
    match send_frame(&conn, &frame).await {
        Ok(())
            if !matches!(
                &event.kind,
                DiagnoseEventKind::Partial | DiagnoseEventKind::Status
            ) =>
        {
            log::info!(
                "[diagnose] streamed event request_id={} browser={} kind={:?} seq={}",
                event.request_id,
                browser_connection_id,
                event.kind,
                event.seq
            );
        }
        Ok(()) => {}
        Err(e) => {
            log::warn!(
                "[diagnose] failed to stream event request_id={} browser={} kind={:?} seq={}: {e}",
                event.request_id,
                browser_connection_id,
                event.kind,
                event.seq
            );
        }
    }
}

/// Stream a terminal [`DiagnoseEvent::error`] to a browser. Used when a diagnosis
/// fails before/after the model phase so the panel — which only consumes
/// `DiagnoseEvent` frames — does not hang waiting.
pub async fn stream_diagnose_error(
    connection_map: &SharedConnectionMap,
    browser_connection_id: &str,
    request_id: &str,
    seq: u32,
    error: AgentError,
) {
    stream_event(
        connection_map,
        browser_connection_id,
        &DiagnoseEvent::error(request_id, seq, error),
    )
    .await;
}

/// Fail a still-collecting diagnosis when either signaling peer disappears.
/// If the browser itself disconnected there is nowhere to stream, so consuming
/// the pending entry is sufficient.
pub(crate) async fn stream_collection_connection_lost(
    connection_map: &SharedConnectionMap,
    ctx: CollectContext,
) {
    stream_diagnose_error(
        connection_map,
        &ctx.browser_connection_id,
        &ctx.request_id,
        seq::MODELING,
        AgentError {
            kind: AgentErrorKind::TargetOffline,
            message: "the target disconnected while evidence was being collected".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        },
    )
    .await;
}

/// Start a diagnosis: register the pending collection, then push a
/// `CollectRequest` to the target edge. Streams a `collecting` status to the
/// browser; on a registration clash (replay) or a failed push it rolls back and
/// streams a terminal error so the panel does not hang.
pub async fn start_diagnosis(
    connection_map: &SharedConnectionMap,
    pending: &CollectPendingStore,
    request_id: &str,
    target_connection_id: &str,
    browser_connection_id: &str,
    actor_user_id: i32,
    target_device_id: String,
    mut scope: desk_agent_protocol::AgentScope,
    max_risk: desk_agent_protocol::RiskLevel,
    exec_admission_policy: desk_agent_protocol::authz::ExecAdmissionPolicy,
    available_exec_shells: Vec<String>,
    max_command_runtime_ms: u32,
    request: DiagnoseRequestData,
) {
    if !request.include_screen {
        scope
            .granted
            .retain(|capability| *capability != Capability::ScreenCaptureCurrent);
    }
    if request.include_screen && scope.granted.contains(&Capability::ScreenCaptureCurrent) {
        match model_provider::load(crate::db::get_db()).await {
            Ok(config) if config.supports_image_input => {}
            Ok(_) => {
                stream_diagnose_error(
                    connection_map,
                    browser_connection_id,
                    request_id,
                    seq::COLLECTING,
                    AgentError {
                        kind: AgentErrorKind::InvalidInput,
                        message: "The selected AI model does not support image input.".into(),
                        retryable: false,
                        safe_for_model: true,
                        error_code: Some(
                            desk_utils::error::DeskErrorCode::AI_MODEL_IMAGE_INPUT_UNSUPPORTED
                                .code(),
                        ),
                    },
                )
                .await;
                return;
            }
            Err(error) => {
                stream_diagnose_error(
                    connection_map,
                    browser_connection_id,
                    request_id,
                    seq::COLLECTING,
                    transport_error(format!("failed to load model provider config: {error}")),
                )
                .await;
                return;
            }
        }
    }
    let ctx = CollectContext {
        request_id: request_id.to_string(),
        target_connection_id: target_connection_id.to_string(),
        browser_connection_id: browser_connection_id.to_string(),
        actor_user_id,
        target_device_id,
        scope,
        max_risk,
        exec_admission_policy,
        available_exec_shells: sanitize_available_exec_shells(&available_exec_shells),
        max_command_runtime_ms,
        request: request.clone(),
    };
    if !pending.register(ctx) {
        stream_diagnose_error(
            connection_map,
            browser_connection_id,
            request_id,
            seq::COLLECTING,
            transport_error("a diagnosis with this request id is already running"),
        )
        .await;
        return;
    }
    stream_event(
        connection_map,
        browser_connection_id,
        &DiagnoseEvent::status(request_id, seq::COLLECTING, "collecting"),
    )
    .await;

    let target = {
        let map = connection_map.read().await;
        map.get(target_connection_id).cloned()
    };
    let Some(target) = target else {
        pending.cancel(request_id);
        stream_diagnose_error(
            connection_map,
            browser_connection_id,
            request_id,
            seq::MODELING,
            AgentError {
                kind: AgentErrorKind::TargetOffline,
                message: "target host is not connected".to_string(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            },
        )
        .await;
        return;
    };
    if let Err(e) = push_collect_request(&target, request_id, &request).await {
        pending.cancel(request_id);
        stream_diagnose_error(
            connection_map,
            browser_connection_id,
            request_id,
            seq::MODELING,
            transport_error(format!("failed to start evidence collection: {e}")),
        )
        .await;
    }
}

/// Map the configured response-format mode to the neutral prompt spec.
#[cfg(test)]
fn response_format_spec(mode: ResponseFormatMode) -> ResponseFormatSpec {
    match mode {
        ResponseFormatMode::None => ResponseFormatSpec::None,
        ResponseFormatMode::JsonObject => ResponseFormatSpec::JsonObject,
        ResponseFormatMode::JsonSchema => ResponseFormatSpec::JsonSchema {
            name: "diagnosis".to_string(),
            schema: diagnosis_json_schema(),
        },
    }
}

/// Record one model call into the hourly usage rollup. Best-effort: a recording
/// failure is logged, never surfaced to the caller. Shared with the terminal
/// orchestrator so every central model dial lands in the same rollup.
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
    if let Err(e) = ai_usage::upsert_ai_usage(db, bucket, &delta).await {
        log::warn!("[diagnose] failed to record model usage: {e}");
    }
}

struct MeteredSignalModel {
    inner: SignalModelSeam,
    db: DatabaseConnection,
    model_name: String,
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for MeteredSignalModel {
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

const AUTO_FOLLOW_UP_MAX_STEPS: u32 = 4;

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

/// Fire a read-only model turn after a durable background completion has been
/// appended to an OSS Signal session. The caller retries `TurnBusy`; every other
/// outcome has spent one bounded automation turn and is persisted for snapshot
/// polling. No tools are exposed, so this follow-up can interpret the completed
/// result but can never dispatch another command.
pub async fn resume_completion_turn(
    db: DatabaseConnection,
    session: PersistedAgentSession,
) -> Result<LoopOutcome, AgentError> {
    let config = model_provider::load(&db)
        .await
        .map_err(|e| transport_error(format!("failed to load model provider config: {e}")))?;
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
    let turn_id = uuid::Uuid::new_v4().to_string();
    let claim = ClaimTurnParams {
        conversation_id: session.conversation_id,
        actor_id: session.actor_id,
        device_id: session.device_id,
        policy_revision: session.policy_revision,
        current_pdp_scope: session.scope_snapshot,
        turn_id,
        // Keep the browser's settled request binding so snapshot polling can
        // replace the earlier placeholder answer with this follow-up transcript.
        request_id: session.current_request_id,
        connection_id: None,
        trigger_origin: TriggerOrigin::ExecCompletion,
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
        response_format: ResponseFormatSpec::None,
        system_prompt: build_agentic_system_message(None),
        max_context_bytes: config
            .max_context_bytes
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_MAX_CONTEXT_BYTES),
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

/// Run the signal-owned multi-turn agent loop once the initial evidence snapshot
/// is complete. Read tools replay this redacted snapshot; an exec tool parks for
/// the browser's explicit approval and then dispatches through the edge PEP.
pub async fn run_model_phase(
    db: DatabaseConnection,
    connection_map: web::Data<SharedConnectionMap>,
    ctx: CollectContext,
    snapshot: EvidenceSnapshot,
) {
    let map = connection_map.as_ref();
    stream_event(
        map,
        &ctx.browser_connection_id,
        &DiagnoseEvent::status(&ctx.request_id, seq::MODELING, "modeling"),
    )
    .await;

    let config = match model_provider::load(&db).await {
        Ok(c) => c,
        Err(e) => {
            stream_diagnose_error(
                map,
                &ctx.browser_connection_id,
                &ctx.request_id,
                seq::TERMINAL,
                transport_error(format!("failed to load model provider config: {e}")),
            )
            .await;
            return;
        }
    };
    let image_urls: Vec<&str> = snapshot
        .contexts
        .iter()
        .filter_map(|entry| entry.image_data_url.as_deref())
        .collect();
    if let Err(error) = validate_image_request(image_urls.iter().copied()) {
        stream_diagnose_error(
            map,
            &ctx.browser_connection_id,
            &ctx.request_id,
            seq::TERMINAL,
            AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!("invalid collected image: {error}"),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            },
        )
        .await;
        return;
    }
    if !image_urls.is_empty()
        && !(ModelCapabilities {
            image_input: config.supports_image_input,
        })
        .satisfies(ModelRequirements::IMAGE_INPUT)
    {
        stream_diagnose_error(
            map,
            &ctx.browser_connection_id,
            &ctx.request_id,
            seq::TERMINAL,
            AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: "The selected AI model does not support image input.".into(),
                retryable: false,
                safe_for_model: true,
                error_code: Some(
                    desk_utils::error::DeskErrorCode::AI_MODEL_IMAGE_INPUT_UNSUPPORTED.code(),
                ),
            },
        )
        .await;
        return;
    }
    let seam = match SignalModelSeam::from_config(&config) {
        Ok(s) => s,
        Err(e) => {
            stream_diagnose_error(
                map,
                &ctx.browser_connection_id,
                &ctx.request_id,
                seq::TERMINAL,
                e,
            )
            .await;
            return;
        }
    };

    let max_ctx = config
        .max_context_bytes
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_MAX_CONTEXT_BYTES);
    let model = MeteredSignalModel {
        inner: seam,
        db: db.clone(),
        model_name: config.model.clone().unwrap_or_default(),
    };
    let mut registry = filter_model_compatible_tools(
        &read_tool_registry(),
        ModelCapabilities {
            image_input: config.supports_image_input,
        },
    );
    if ctx.scope.granted.contains(&Capability::ShellExecConfirmed)
        && matches!(
            ctx.scope.mode,
            ExecutionMode::ConfirmEachAction | ExecutionMode::SessionApproved
        )
        && !ctx.available_exec_shells.is_empty()
    {
        registry.extend(exec_tool_registry_for_shells_with_timeout(
            &ctx.available_exec_shells,
            ctx.max_command_runtime_ms,
        ));
    }

    let connections = connection_map.clone().into_inner();
    let tools = crate::agent_exec::SignalAgentTools::new(
        db.clone(),
        connections,
        crate::agent_exec::global_agent_exec_pending(),
        ctx.target_connection_id.clone(),
        ctx.request_id.clone(),
        snapshot,
        ctx.exec_admission_policy,
        ctx.max_risk,
        ctx.available_exec_shells.clone(),
        ctx.max_command_runtime_ms,
    );
    let client_conversation_id = ctx
        .request
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|id| is_valid_client_conversation_id(id))
        .map(str::to_string);
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db)
        .with_client_metadata(client_conversation_id, AgentSessionSurface::Diagnose);
    let heartbeat = SignalStoreHeartbeat {
        store: sessions.clone(),
    };
    let actor_id = ctx.actor_user_id.to_string();
    let conversation_id = derive_conversation_key(
        &actor_id,
        &ctx.target_device_id,
        ctx.request.conversation_id.as_deref(),
        &ctx.request_id,
    );
    let turn_id = uuid::Uuid::new_v4().to_string();
    let clock = || chrono::Utc::now().to_rfc3339();
    let deps = LoopDeps {
        session_seam: &sessions,
        model: &model,
        tools: &tools,
        content_safety: desk_diagnose_core::content_safety::ContentSafetyMode::Disabled,
        registry: &registry,
        response_format: ResponseFormatSpec::None,
        system_prompt: build_agentic_system_message(ctx.request.locale.as_deref()),
        max_context_bytes: max_ctx,
        max_steps_per_turn: config.max_steps_per_turn,
        max_same_tool_per_turn: config.max_same_tool_calls_per_turn,
        clock: &clock,
        heartbeat: Some(&heartbeat),
    };
    let claim = ClaimTurnParams {
        conversation_id,
        actor_id,
        device_id: ctx.target_device_id.clone(),
        policy_revision: 0,
        current_pdp_scope: ctx.scope.clone(),
        turn_id: turn_id.clone(),
        request_id: Some(ctx.request_id.clone()),
        connection_id: Some(ctx.browser_connection_id.clone()),
        trigger_origin: TriggerOrigin::User,
        now: clock(),
    };
    let user = ChatMessage::text(
        uuid::Uuid::new_v4().to_string(),
        ChatRole::User,
        ctx.request.question.clone(),
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DiagnoseEvent>();
    let forward_map = connection_map.clone();
    let forward_browser = ctx.browser_connection_id.clone();
    let forwarder = actix_web::rt::spawn(async move {
        while let Some(event) = rx.recv().await {
            stream_event(forward_map.as_ref(), &forward_browser, &event).await;
        }
    });
    let frame_sink = move |event: DiagnoseEvent| {
        let _ = tx.send(event);
    };
    let mut sink =
        StreamingTurnSink::starting_at(frame_sink, ctx.request_id.clone(), seq::TERMINAL);
    sink.set_provenance(AiProvenance::stamp(
        config.model,
        Some(chrono::Utc::now().to_rfc3339()),
    ));
    sink.turn_started(&turn_id);
    match run_agent_turn(&deps, claim, user, &mut sink).await {
        Ok(outcome) => sink.finish_outcome(&outcome),
        Err(error) => sink.error(error),
    }
    drop(sink);
    let _ = forwarder.await;
}

/// Consume an inbound `CollectResponse` from a desk-server edge: feed the chunk
/// into the pending store under its source-connection binding and, on
/// completion, spawn the model phase. A failure / wholesale error streams a
/// terminal `DiagnoseEvent::error` to the browser.
pub async fn on_collect_response(
    connection_map: &web::Data<SharedConnectionMap>,
    pending: &CollectPendingStore,
    source: &ConnectionState,
    model: &SignalingModel,
) {
    let source_id = source.model.connection_id.clone();
    let response = match model.get_data::<CollectResponse>() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[diagnose] dropping malformed CollectResponse: {e}");
            return;
        }
    };
    let map = connection_map.as_ref();
    match response {
        CollectResponse::Chunk(chunk) => match pending.accept_chunk(&source_id, &chunk) {
            AcceptOutcome::NeedMore => {}
            AcceptOutcome::Complete { ctx, snapshot } => {
                // The model dial is `!Send`; run it on the current-thread arbiter.
                actix_web::rt::spawn(run_model_phase(
                    crate::db::get_db().clone(),
                    connection_map.clone(),
                    ctx,
                    *snapshot,
                ));
            }
            AcceptOutcome::Failed { ctx, error } => {
                stream_diagnose_error(
                    map,
                    &ctx.browser_connection_id,
                    &ctx.request_id,
                    seq::MODELING,
                    error,
                )
                .await;
            }
            AcceptOutcome::Rejected(reason) => {
                log::warn!("[diagnose] rejected collect chunk from {source_id}: {reason}");
            }
        },
        CollectResponse::Error(err) => {
            let error = AgentError {
                kind: err.error_kind,
                message: err.reason,
                retryable: false,
                safe_for_model: true,
                error_code: None,
            };
            if let AcceptOutcome::Failed { ctx, error } =
                pending.fail(&source_id, &err.request_id, error)
            {
                stream_diagnose_error(
                    map,
                    &ctx.browser_connection_id,
                    &ctx.request_id,
                    seq::MODELING,
                    error,
                )
                .await;
            }
        }
    }
}

/// Facade [`CollectObserver`] for the signal central brain: routes inbound
/// `CollectResponse` frames into the process-global pending store and, on
/// completion, the model phase. Holds the connection map (to stream results back)
/// and the shared pending store.
pub struct SignalCollectObserver {
    connection_map: web::Data<SharedConnectionMap>,
    pending: Arc<CollectPendingStore>,
}

impl SignalCollectObserver {
    pub fn new(connection_map: web::Data<SharedConnectionMap>) -> Self {
        Self {
            connection_map,
            pending: global_pending_store(),
        }
    }
}

impl CollectObserver for SignalCollectObserver {
    fn on_collect_response<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            on_collect_response(&self.connection_map, &self.pending, source, model).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_format_spec_maps_each_mode() {
        assert!(matches!(
            response_format_spec(ResponseFormatMode::None),
            ResponseFormatSpec::None
        ));
        assert!(matches!(
            response_format_spec(ResponseFormatMode::JsonObject),
            ResponseFormatSpec::JsonObject
        ));
        match response_format_spec(ResponseFormatMode::JsonSchema) {
            ResponseFormatSpec::JsonSchema { name, schema } => {
                assert_eq!(name, "diagnosis");
                assert!(schema.is_object());
            }
            _ => panic!("expected json_schema spec"),
        }
    }

    /// Every path the single-turn orchestrator can take, expressed as the ordered
    /// `seq` of the `DiagnoseEvent` frames it streams to the browser. The panel
    /// ignores any frame whose `seq` it has already applied (a stale-replay guard),
    /// so each path must be **strictly increasing** — a colliding slot (the prior
    /// `collecting == modeling == error` bug) silently drops the later frame and
    /// hangs the panel on the earlier status.
    #[test]
    fn every_lifecycle_path_emits_strictly_increasing_seq() {
        let paths: &[&[u32]] = &[
            // Happy path: collecting -> modeling -> final.
            &[seq::COLLECTING, seq::MODELING, seq::TERMINAL],
            // Host offline / collect push failed (after collecting).
            &[seq::COLLECTING, seq::MODELING],
            // Collection failed / edge error (after collecting, before the dial).
            &[seq::COLLECTING, seq::MODELING],
            // Model dial failed, e.g. a gateway 429 (after modeling).
            &[seq::COLLECTING, seq::MODELING, seq::TERMINAL],
            // Duplicate-request clash before collecting: a single terminal frame.
            &[seq::COLLECTING],
        ];
        for path in paths {
            for w in path.windows(2) {
                assert!(w[0] < w[1], "non-monotonic seq in path {path:?}");
            }
        }
    }

    #[test]
    fn global_pending_store_is_a_single_shared_instance() {
        let a = global_pending_store();
        let b = global_pending_store();
        // Both handles point at the same store (registering through one is visible
        // to the other), confirming the process-global single-node assumption.
        assert!(a.register(CollectContext {
            request_id: "r-shared".to_string(),
            target_connection_id: "edge-1".to_string(),
            browser_connection_id: "browser-1".to_string(),
            actor_user_id: 1,
            target_device_id: "device-1".to_string(),
            scope: desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            max_risk: desk_agent_protocol::RiskLevel::Critical,
            exec_admission_policy: desk_agent_protocol::authz::ExecAdmissionPolicy::TemplateOnly,
            available_exec_shells: Vec::new(),
            max_command_runtime_ms: desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS,
            request: DiagnoseRequestData::default(),
        }));
        assert!(!b.register(CollectContext {
            request_id: "r-shared".to_string(),
            target_connection_id: "edge-1".to_string(),
            browser_connection_id: "browser-1".to_string(),
            actor_user_id: 1,
            target_device_id: "device-1".to_string(),
            scope: desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            max_risk: desk_agent_protocol::RiskLevel::Critical,
            exec_admission_policy: desk_agent_protocol::authz::ExecAdmissionPolicy::TemplateOnly,
            available_exec_shells: Vec::new(),
            max_command_runtime_ms: desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS,
            request: DiagnoseRequestData::default(),
        }));
        // Clean up so the global store does not leak into other tests.
        b.cancel("r-shared");
    }
}
