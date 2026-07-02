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
    CollectRequest, CollectResponse, DiagnoseEvent, DiagnoseRequestData,
};
use desk_agent_protocol::evidence::EvidenceSnapshot;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::DEFAULT_MAX_CONTEXT_BYTES;
use desk_diagnose_core::parser::parse_diagnosis;
use desk_diagnose_core::prompt::{ResponseFormatSpec, build_messages, diagnosis_json_schema};
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::service::CollectObserver;
use sea_orm::DatabaseConnection;
use std::sync::Arc;

use crate::ai_usage::{self, AiUsageDelta};
use crate::collect_pending::{AcceptOutcome, CollectContext, CollectPendingStore};
use crate::model_dial::SignalModelSeam;
use crate::model_provider::{self, ResponseFormatMode};

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
    conn.session
        .write()
        .await
        .text(text)
        .await
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
    if let Err(e) = send_frame(&conn, &frame).await {
        log::warn!("[diagnose] failed to stream event to {browser_connection_id}: {e}");
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
    request: DiagnoseRequestData,
) {
    let ctx = CollectContext {
        request_id: request_id.to_string(),
        target_connection_id: target_connection_id.to_string(),
        browser_connection_id: browser_connection_id.to_string(),
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

/// Run the model phase once the evidence snapshot is complete: build the prompt,
/// dial the configured provider, parse the structured diagnosis, and stream it to
/// the browser. Any failure streams a terminal error instead.
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
    let messages = build_messages(
        &ctx.request.question,
        &snapshot,
        max_ctx,
        ctx.request.locale.as_deref(),
        &[],
    );
    let request = ModelRequest::text_only(messages, response_format_spec(config.response_format));

    let mut sink = NullTurnSink;
    let turn = match seam.call(request, &mut sink).await {
        Ok(t) => t,
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

    record_usage(
        &db,
        config.model.as_deref().unwrap_or_default(),
        &turn.usage,
    )
    .await;

    let (mut diagnosis, _outcome) = parse_diagnosis(&turn.text);
    // The orchestrator stamps the authoritative collected-capability list (the
    // parser leaves it empty).
    diagnosis.collected = snapshot
        .contexts
        .iter()
        .map(|c| c.capability.clone())
        .collect();
    stream_event(
        map,
        &ctx.browser_connection_id,
        &DiagnoseEvent::final_result(&ctx.request_id, seq::TERMINAL, diagnosis),
    )
    .await;
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
            request: DiagnoseRequestData::default(),
        }));
        assert!(!b.register(CollectContext {
            request_id: "r-shared".to_string(),
            target_connection_id: "edge-1".to_string(),
            browser_connection_id: "browser-1".to_string(),
            request: DiagnoseRequestData::default(),
        }));
        // Clean up so the global store does not leak into other tests.
        b.cancel("r-shared");
    }
}
