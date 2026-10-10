// This backend chain uses both production HTTP dialects, the production agent
// loop, collector, store and query. Isolated session/device seams do not replace
// authenticated REST/UI or PostgreSQL multi-instance acceptance.
use std::{cell::{Cell, RefCell}, sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::Duration};
use desk_agent_protocol::{AgentError, AgentScope, Capability, ExecutionMode};
use desk_diagnose_core::{
    agent_loop::{run_agent_turn, LoopDeps, LoopOutcome},
    chat::{ChatMessage, ChatRole, ModelTurn, ToolCall, ToolSpec},
    content_safety::ContentSafetyMode,
    model_capability::ModelRequirements,
    model_context::PinnedContextPolicy,
    model_observability::{Attribution, ObservationContext,
        ObservationEvent, Origin, Purpose, Surface, Stage, StageOutcome,
        tool::{ToolObservation, ToolObservationSlot}},
    model_profile::{ModelUseCase, WireProtocol},
    prompt::ResponseFormatSpec,
    registry::{RegisteredTool, ToolEffect},
    seam::{ClaimError, ClaimTurnParams, ModelRequest, ModelSeam, SessionSeam, ToolRunOutput,
        ToolOutputFormat, ToolSeam, TurnSink},
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use desk_signal_facade::{model::model_metrics::{ComponentState, MetricsOverview, MetricsQuery,
    MetricsSettings, MetricsSummary}, service::model_metrics::{ResolvedQuery, timestamp,
    collector::{self, AggregateProgress, Collector, MetricsBackend, WriterHealth}}};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct AdapterParts {
    model: Box<dyn ModelSeam>,
    policy: PinnedContextPolicy,
    attribution: Attribution,
}

struct ObservedAdapter {
    parts: AdapterParts,
    collector: Arc<Collector>,
    calls: Cell<u32>,
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for ObservedAdapter {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        self.parts.model.model_output_token_limit(request)
    }

    fn model_input_token_upper_bound(&self, request: &ModelRequest) -> Result<Option<u64>, AgentError> {
        self.parts.model.model_input_token_upper_bound(request)
    }

    fn observation_context(&self, use_case: ModelUseCase, origin: Origin) -> Option<ObservationContext> {
        let next = self.calls.get() + 1;
        self.calls.set(next);
        let mut attribution = self.parts.attribution.clone();
        attribution.purpose = use_case.into();
        attribution.origin = origin;
        self.collector.context(format!("backend-chain-{next}"), now(), attribution)
    }

    async fn context_policy(&self, _: ModelRequirements) -> Result<PinnedContextPolicy, AgentError> {
        Ok(self.parts.policy.clone())
    }

    async fn call(&self, request: ModelRequest, sink: &mut dyn TurnSink) -> Result<ModelTurn, AgentError> {
        self.parts.model.call(request, sink).await
    }
}

struct Session(RefCell<PersistedAgentSession>);

#[async_trait::async_trait(?Send)]
impl SessionSeam for Session {
    async fn claim_turn(&self, params: ClaimTurnParams) -> Result<PersistedAgentSession, ClaimError> {
        let mut session = self.0.borrow().clone();
        let turn_id = params.turn_id.clone();
        session.begin_turn(params.turn_id, params.request_id, params.connection_id,
            params.policy_revision, params.current_pdp_scope, params.now).map_err(|_| ClaimError::Busy)?;
        session.adopt_trigger(params.trigger_origin, &turn_id);
        *self.0.borrow_mut() = session.clone();
        Ok(session)
    }

    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        *self.0.borrow_mut() = session.clone();
        Ok(())
    }
}

#[derive(Default)]
struct ReadDevice {
    reads: Cell<u32>,
    observation: ToolObservationSlot,
}

#[async_trait::async_trait(?Send)]
impl ToolSeam for ReadDevice {
    fn observe_tool_input(&self, call: &ToolCall, observation: ToolObservation) {
        self.observation.set(call, observation);
    }

    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        // This fixture's real preflight consumes the schema-accepted arguments.
        // A generic successful result must not manufacture input acceptance.
        let arguments: Value = serde_json::from_str(&call.arguments_json).unwrap();
        assert_eq!(arguments, json!({"fields":["os"]}));
        self.observation.get(call).stage(Stage::Preflight, StageOutcome::Passed);
        self.reads.set(self.reads.get() + 1);
        Ok(ToolRunOutput { format: ToolOutputFormat::Text,
            content: "private-device-read-reply".into(), ..Default::default() })
    }
}

#[derive(Default)]
struct Output(String);
impl TurnSink for Output {
    fn on_text_delta(&mut self, delta: &str) { self.0.push_str(delta); }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureMode { Healthy, Disabled, WriteUnavailable, MissingEventTable }

struct Backend {
    store: super::super::Store,
    mode: CaptureMode,
    failures: AtomicUsize,
}

#[async_trait::async_trait]
impl MetricsBackend for Backend {
    async fn settings(&self) -> Result<MetricsSettings, ()> { MetricsBackend::settings(&self.store).await }
    async fn persist(&self, events: &[ObservationEvent], received: i64) -> Result<u32, ()> {
        if self.mode == CaptureMode::WriteUnavailable {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(());
        }
        let result = MetricsBackend::persist(&self.store, events, received).await;
        if result.is_err() { self.failures.fetch_add(1, Ordering::Relaxed); }
        result
    }
    async fn aggregate(&self, received: i64) -> Result<AggregateProgress, ()> {
        if self.mode == CaptureMode::WriteUnavailable { return Err(()); }
        MetricsBackend::aggregate(&self.store, received).await
    }
    async fn cleanup(&self, received: i64) -> Result<(), ()> {
        MetricsBackend::cleanup(&self.store, received).await
    }
    async fn report(&self, health: &WriterHealth) -> Result<(), ()> {
        MetricsBackend::report(&self.store, health).await
    }
}

fn now() -> i64 { chrono::Utc::now().timestamp_millis() }

fn scope() -> AgentScope {
    AgentScope { granted: vec![Capability::SystemInfo], mode: ExecutionMode::ReadOnly,
        expires_at: None, policy_name: None }
}

fn count<'a>(summary: &'a MetricsSummary, key: &str) -> Option<&'a str> {
    summary.counts.iter().find(|count| count.key == key).map(|count| count.count.as_str())
}

fn query(started: i64, attribution: &Attribution) -> ResolvedQuery {
    ResolvedQuery::resolve(&MetricsQuery {
        from: Some(timestamp(started - 60_000)), to: Some(timestamp(now() + 60_000)),
        provider_id: Some(attribution.provider_id.clone()), model_id: Some(attribution.model_id.clone()),
        contract_revision: Some(attribution.contract_revision.clone()), ..Default::default()
    }, now() + 60_000).unwrap()
}

fn sse(protocol: WireProtocol, index: usize) -> String {
    let tool = index < 2;
    let arguments = if index == 0 { r#"{"fields":["os","os"]}"# } else { r#"{"fields":["os"]}"# };
    let id = if index == 0 { "bad" } else { "good" };
    match protocol {
        WireProtocol::OpenAiChatCompletions => {
            let delta = if tool { json!({"tool_calls":[{"index":0,"id":id,"type":"function",
                "function":{"name":"sysinfo","arguments":arguments}}]}) }
                else { json!({"content":"corrected"}) };
            let chunk = json!({"id":format!("response-{index}"),"object":"chat.completion.chunk",
                "choices":[{"index":0,"delta":delta,"finish_reason":if tool {"tool_calls"} else {"stop"}}],
                "usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}});
            format!(": heartbeat\n\ndata: {chunk}\n\ndata: [DONE]\n\n")
        }
        WireProtocol::AnthropicMessages => {
            let block = if tool { json!({"type":"tool_use","id":id,"name":"sysinfo","input":{}}) }
                else { json!({"type":"text","text":""}) };
            let delta = if tool { json!({"type":"input_json_delta","partial_json":arguments}) }
                else { json!({"type":"text_delta","text":"corrected"}) };
            let facts = [
                json!({"type":"message_start","message":{"id":format!("response-{index}"),
                    "type":"message","role":"assistant","model":"metrics-model","content":[],
                    "stop_reason":null,"usage":{"input_tokens":11,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":block}),
                json!({"type":"content_block_delta","index":0,"delta":delta}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}},
                    "usage":{"output_tokens":3}}),
                json!({"type":"message_stop"}),
            ];
            let mut output = ": heartbeat\n\n".to_string();
            for fact in facts { output.push_str(&format!("event: {}\ndata: {fact}\n\n", fact["type"].as_str().unwrap())); }
            output
        }
        WireProtocol::OpenAiResponses => panic!("unsupported production dialect"),
    }
}

async fn provider(protocol: WireProtocol) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut requests = Vec::new();
            for index in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut received = Vec::new();
                let (end, length) = loop {
                    let mut bytes = [0; 4096];
                    let read = socket.read(&mut bytes).await.unwrap();
                    assert!(read > 0 && received.len() < 1_048_576);
                    received.extend_from_slice(&bytes[..read]);
                    if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
                        let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:"))
                            .expect("bounded HTTP request").trim().parse().unwrap();
                        if received.len() >= end + 4 + length { break (end + 4, length); }
                    }
                };
                requests.push(serde_json::from_slice(&received[end..end + length]).unwrap());
                let body = sse(protocol, index);
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            requests
        }).await.expect("all original model requests reached the fixture")
    });
    (format!("http://{address}/v1"), task)
}

#[derive(Debug, PartialEq)]
struct BusinessResult { outcome: LoopOutcome, reads: u32, stream: String, wire: Vec<Value> }

async fn execute(protocol: WireProtocol, mode: CaptureMode) -> BusinessResult {
    let started = now();
    let store = super::fixture_store(started - 3_600_000).await;
    if mode == CaptureMode::Disabled {
        let mut settings = store.load_settings().await.unwrap();
        settings.enabled = false;
        store.save_settings(settings, started).await.unwrap().unwrap();
    }
    let (collector, receiver) = Collector::channel();
    let backend = Arc::new(Backend { store: store.clone(), mode, failures: AtomicUsize::new(0) });
    let writer = tokio::spawn(collector::run(collector.clone(), receiver, backend.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(collector.state(), ComponentState::Ready | ComponentState::Disabled) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("background configuration became available");
    if mode == CaptureMode::MissingEventTable {
        use sea_orm::ConnectionTrait;
        store.db.execute_raw(sea_orm::Statement::from_string(store.db.get_database_backend(),
            "DROP TABLE model_metric_event".to_string())).await.unwrap();
    }
    let (url, http) = provider(protocol).await;
    let model = ObservedAdapter { parts: adapter(&url, protocol), collector: collector.clone(), calls: Cell::new(0) };
    let mut seeded = PersistedAgentSession::new("chain-conversation", "owner", "device", 1,
        scope(), "2026-10-08T00:00:00Z");
    seeded.surface = AgentSessionSurface::AiAssistant;
    seeded.input_revision = 1;
    seeded.latest_input_seq = 1;
    seeded.focus_epoch.input_revision = 1;
    let session = Session(RefCell::new(seeded));
    let device = ReadDevice::default();
    let registry = [RegisteredTool {
        required_capability: Capability::SystemInfo, effect: ToolEffect::ReadOnly,
        spec: ToolSpec { name: "sysinfo".into(), description: "Read OS metadata".into(),
            parameters_schema: json!({"type":"object","additionalProperties":false,"required":["fields"],
                "properties":{"fields":{"type":"array","minItems":1,"uniqueItems":true,
                    "items":{"type":"string","enum":["os","cpu"]}}}}) },
    }];
    let clock = || "2026-10-08T00:00:01Z".to_string();
    let deps = LoopDeps {
        session_seam: &session, model: &model, tools: &device, content_safety: ContentSafetyMode::Disabled,
        registry: &registry, provider_registry: None, capability_inventory: None,
        capability_permission_candidates: &[], capability_catalog_metrics: None,
        permission_continuation_exact_tools: &[], response_format: ResponseFormatSpec::None,
        system_prompt: desk_diagnose_core::agentic_prompt::build_agentic_system_message(None),
        response_locale: None, interactive_user_home: None, interactive_user_home_incarnation: None,
        max_steps_per_turn: desk_diagnose_core::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: desk_diagnose_core::MAX_SAME_TOOL_PER_TURN, clock: &clock, heartbeat: None,
    };
    let claim = ClaimTurnParams {
        conversation_id: "chain-conversation".into(), actor_id: "owner".into(), device_id: "device".into(),
        policy_revision: 1, current_pdp_scope: scope(), turn_id: "turn-1".into(), request_id: Some("request-1".into()),
        connection_id: Some("connection-1".into()), trigger_origin: TriggerOrigin::User, now: clock(),
    };
    let mut output = Output::default();
    let visibility_start = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(5), run_agent_turn(&deps, claim,
        ChatMessage::text("owner-input", ChatRole::User, "private-owner-request-metrics-test"), &mut output))
        .await.expect("metrics failure cannot stall the original loop").unwrap();
    let wire = http.await.unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("corrected".into()));
    assert_eq!(wire.len(), 3);
    assert_eq!(device.reads.get(), 1);
    assert!(wire.iter().all(|body| !body.to_string().contains("backend-chain-")),
        "observation identities must never enter provider request bodies");
    let mut cohort = model.parts.attribution.clone();
    cohort.contract_revision = format!("{}.{}", desk_diagnose_core::model_observability::DEFINITION_VERSION,
        session.0.borrow().capability_disclosure.definition_revision);
    let range = query(started, &cohort);
    if mode == CaptureMode::Healthy {
        let mut last_sample = String::new();
        let overview: MetricsOverview = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let overview = store.overview(&range, now()).await.unwrap();
                let status = store.status(now()).await.unwrap();
                if count(&overview.summary, "correction_accepted") == Some("1")
                    && status.backlog.as_deref() == Some("0") && status.last_aggregated.is_some() {
                    break overview;
                }
                last_sample = format!("counts={:?}; state={:?}; backlog={:?}; writer_failures={}", overview.summary.counts,
                    status.state, status.backlog, backend.failures.load(Ordering::Relaxed));
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap_or_else(|_| panic!("actual queue, aggregation and query became visible: {last_sample}"));
        assert!(visibility_start.elapsed() < Duration::from_secs(5));
        for (key, value) in [("calls","3"), ("attempts","3"), ("returned","3"),
            ("tools","2"), ("format_rejected","1"), ("input_rejected","1"),
            ("input_accepted","1"), ("correction_linked","1"), ("correction_accepted","1")] {
            assert_eq!(count(&overview.summary, key), Some(value), "{key}");
        }
        for (key, numerator, denominator) in [("request_failure","0","3"),
            ("tool_format_failure","1","2"), ("next_input_acceptance","1","1")] {
            let rate = overview.summary.rates.iter().find(|rate| rate.key == key).unwrap();
            assert_eq!((rate.numerator.as_deref(), rate.denominator.as_deref()), (Some(numerator), Some(denominator)), "{key}");
        }
        let calls = store.calls(&range, now()).await.unwrap();
        assert_eq!(calls.records.len(), 3);
        let mut tool_range = range.clone();
        tool_range.tool = Some("sysinfo".into());
        let tools = store.calls(&tool_range, now()).await.unwrap();
        assert_eq!(tools.records.len(), 2);
        let original = tools.records.iter().find(|tool| tool.input_conclusion.as_deref() == Some("rejected")).unwrap();
        let detail = store.call_detail(&original.id).await.unwrap().unwrap();
        let group = detail.call.correction_group.as_ref().unwrap();
        assert_eq!(group.outcome, "input_accepted");
        assert_eq!(group.linked_attempts, "1");
        let accepted = tools.records.iter().find(|tool| tool.input_conclusion.as_deref() == Some("accepted")).unwrap();
        assert_eq!(accepted.correction_of.as_deref(), Some(original.id.as_str()));
        let serialized = serde_json::to_string(&(overview, calls, tools, detail)).unwrap();
        for private in ["private-owner-request-metrics-test", "private-device-read-reply", "fixture-secret",
            r#"{"fields":["os","os"]}"#, "http://127.0.0.1"] { assert!(!serialized.contains(private)); }
        super::assert_resource_accounting(&store).await;
    } else if matches!(mode, CaptureMode::WriteUnavailable | CaptureMode::MissingEventTable) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while backend.failures.load(Ordering::Relaxed) == 0 { tokio::task::yield_now().await; }
        }).await.expect("the actual writer attempted and failed");
        assert!(store.calls(&range, now()).await.unwrap().records.is_empty());
    } else {
        assert!(!collector.enabled());
        assert!(store.calls(&range, now()).await.unwrap().records.is_empty());
    }
    collector.shutdown().await;
    tokio::time::timeout(Duration::from_secs(8), writer).await.unwrap().unwrap();
    BusinessResult { outcome, reads: device.reads.get(), stream: output.0, wire }
}

#[actix_web::test]
async fn real_agent_http_collector_store_query_preserves_business_for_both_protocols_and_writer_failure() {
    for protocol in [WireProtocol::OpenAiChatCompletions, WireProtocol::AnthropicMessages] {
        let healthy = execute(protocol, CaptureMode::Healthy).await;
        let disabled = execute(protocol, CaptureMode::Disabled).await;
        let failed = execute(protocol, CaptureMode::WriteUnavailable).await;
        let missing = execute(protocol, CaptureMode::MissingEventTable).await;
        assert_eq!(healthy, disabled, "turn, actual read, streamed answer and full HTTP bodies");
        assert_eq!(healthy, failed, "failed observation must not add model attempts or change content");
        assert_eq!(healthy, missing, "a missing metrics table must not affect the original model loop");
    }
}
