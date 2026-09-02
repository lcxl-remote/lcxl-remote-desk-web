use super::*;
use crate::chat::{ChatRole, ModelTurn, StopReason, ToolCall, ToolCallRef, ToolSpec};
use crate::model_profile::WireProtocol;
use crate::prompt::ResponseFormatSpec;
use crate::replay::{ProviderResponseMeta, ReplayDisposition, SourceContextKey};
use crate::seam::{NullTurnSink, ToolRunOutput, TurnSink, WaitOutcome};
use crate::session::PersistedAgentSession;
use async_trait::async_trait;
use desk_agent_protocol::data_lineage::DataProvenance;
use desk_agent_protocol::{AgentScope, Capability, ExecutionMode};
use std::cell::RefCell;
use std::rc::Rc;

mod background_receipts;
mod egress;
mod original_results;
mod version_handoff;

#[test]
fn selected_object_lineage_keeps_explicit_sources_without_later_context_expansion() {
    use crate::object_context::{
        ObjectContextBuild, ObjectContextMutation, build_object_context_mutation,
    };
    use desk_agent_protocol::{
        computer_use::{ObjectKind, ObjectRef},
        data_lineage::DestinationIdentity,
        device_assistant::{
            DeviceAssistantObjectContextOperation, DeviceAssistantObjectContextUpdate,
        },
    };
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let mut session = PersistedAgentSession::new(
        "run",
        "actor",
        "device",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-08-31T00:00:00Z",
    );
    session.adopt_client_metadata(
        Some("client"),
        crate::session::AgentSessionSurface::DeviceAssistant,
    );
    for id in ["selected", "unselected"] {
        let update = DeviceAssistantObjectContextUpdate {
            conversation_id: "client".into(),
            client_request_id: id.into(),
            operation: DeviceAssistantObjectContextOperation::AttachFile {
                object_ref: ObjectRef {
                    token: id.into(),
                    snapshot_id: "snapshot".into(),
                    object_kind: ObjectKind::File,
                    expires_at: "2030-01-01T00:00:00Z".into(),
                },
                display_summary: "file".into(),
            },
        };
        let ObjectContextMutation::Attach(object) = build_object_context_mutation(
            &update,
            ObjectContextBuild {
                actor_id: "actor",
                device_id: "device",
                destination: &destination,
                now_unix_ms: 1,
                attachment_id: id,
                observation_id: id,
            },
        )
        .unwrap() else {
            panic!("attachment");
        };
        session.context_attachments.push(object);
    }
    let user = crate::model_message_labels::model_bound_user_message(
        "user".into(),
        "read selected file".into(),
        destination,
    )
    .unwrap();
    let user_source = user.data_envelope.as_ref().unwrap().envelope_id.clone();
    session.conversation.push(user);
    let call = ToolCall {
        id: "read".into(),
        name: "read_selected_text_file".into(),
        arguments_json: "{}".into(),
    };
    for explicit in [
        None,
        Some("envelope-selected"),
        Some("another-authoritative-source"),
    ] {
        let mut envelope = session.context_attachments[0].envelope.clone();
        envelope.envelope_id = "result".into();
        envelope.provenance.source_envelope_ids =
            explicit.into_iter().map(str::to_string).collect();
        let mut envelope = Some(envelope);
        bind_tool_input_envelopes(&session, &call, &mut envelope).unwrap();
        let ids = envelope.unwrap().provenance.source_envelope_ids;
        assert!(ids.contains(&user_source));
        if let Some(source) = explicit {
            assert!(ids.iter().any(|id| id == source));
            assert!(!ids.iter().any(|id| id == "envelope-unselected"));
        } else {
            assert!(ids.iter().any(|id| id == "envelope-selected"));
            assert!(ids.iter().any(|id| id == "envelope-unselected"));
        }
    }
}

/// An in-memory session store: one session, claimed via the pure transition.
#[derive(Default)]
struct MemSession {
    inner: RefCell<Option<PersistedAgentSession>>,
    latest_revision: Rc<RefCell<Option<u64>>>,
    superseded_settles: Rc<RefCell<u32>>,
    saves: RefCell<usize>,
    fail_save_at: Option<usize>,
    fail_save_with_message_id: Option<&'static str>,
    supersede_on_save_failure: bool,
}
#[async_trait(?Send)]
impl SessionSeam for MemSession {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut slot = self.inner.borrow_mut();
        let mut session = slot.take().unwrap_or_else(|| {
            PersistedAgentSession::new(
                params.conversation_id.clone(),
                params.actor_id.clone(),
                params.device_id.clone(),
                params.policy_revision,
                params.current_pdp_scope.clone(),
                params.now.clone(),
            )
        });
        let trigger_origin = params.trigger_origin;
        let turn_id = params.turn_id.clone();
        session
            .begin_turn(
                params.turn_id,
                params.request_id,
                params.connection_id,
                params.policy_revision,
                params.current_pdp_scope,
                params.now,
            )
            .map_err(|_| ClaimError::Busy)?;
        session.adopt_trigger(trigger_origin, &turn_id);
        *slot = Some(session.clone());
        Ok(session)
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        *self.saves.borrow_mut() += 1;
        if self.fail_save_at == Some(*self.saves.borrow())
            || self.fail_save_with_message_id.is_some_and(|id| {
                session
                    .conversation
                    .iter()
                    .any(|message| message.message_id == id)
            })
        {
            if self.supersede_on_save_failure {
                *self.latest_revision.borrow_mut() = Some(session.input_revision + 1);
            }
            return Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "synthetic save failure".into(),
                retryable: true,
                safe_for_model: false,
                error_code: None,
            });
        }
        *self.inner.borrow_mut() = Some(session.clone());
        Ok(())
    }

    async fn latest_input_revision(
        &self,
        _conversation_id: &str,
    ) -> Result<Option<u64>, AgentError> {
        Ok(*self.latest_revision.borrow())
    }

    async fn settle_superseded(
        &self,
        stale_session: &PersistedAgentSession,
        _now: &str,
    ) -> Result<bool, AgentError> {
        *self.superseded_settles.borrow_mut() += 1;
        *self.inner.borrow_mut() = Some(stale_session.clone());
        Ok(true)
    }
}

/// A scripted model: returns the queued turns in order, recording each request.
struct ScriptModel {
    turns: RefCell<std::collections::VecDeque<ModelTurn>>,
    requests: Rc<RefCell<Vec<ModelRequest>>>,
}

/// A checkpoint-capable scripted model used to prove that compression is an
/// inline provider call, not a model→tool step or a user-visible stream.
struct CompressionScriptModel {
    turns: RefCell<std::collections::VecDeque<Result<ModelTurn, AgentError>>>,
    requests: Rc<RefCell<Vec<ModelRequest>>>,
    audits: Rc<RefCell<Vec<crate::seam::ContextCompressionAuditOutcome>>>,
    source: SourceContextKey,
}

struct NoopHeartbeatGuard;
impl crate::seam::HeartbeatGuard for NoopHeartbeatGuard {}

struct UnhealthyHeartbeat;
impl crate::seam::LeaseHeartbeat for UnhealthyHeartbeat {
    fn start(
        &self,
        _conversation_id: String,
        _lease_token: u64,
    ) -> Box<dyn crate::seam::HeartbeatGuard> {
        Box::new(NoopHeartbeatGuard)
    }

    fn is_healthy(&self) -> bool {
        false
    }
}

#[async_trait(?Send)]
impl ModelSeam for CompressionScriptModel {
    async fn context_policy(
        &self,
        _requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        crate::model_context::PinnedContextPolicy::checkpoint_summary(
            self.source.clone(),
            1,
            crate::MIN_MODEL_CONTEXT_BYTES * 4,
            1,
        )
        .map_err(model_context_error)
    }

    fn context_compression_provenance(
        &self,
        turn_id: &str,
        created_at: &str,
    ) -> Result<crate::model_context::CompressorProvenanceV1, AgentError> {
        Ok(crate::model_context::CompressorProvenanceV1 {
            source_context_key: self.source.as_str().to_string(),
            provider_identity_sha256: "a".repeat(64),
            model_identity_sha256: "b".repeat(64),
            connection_revision: 1,
            model_profile_revision: 1,
            prompt_version: crate::model_context::CONTEXT_SUMMARY_PROMPT_VERSION.into(),
            schema_version: crate::model_context::CONTEXT_SUMMARY_SCHEMA_VERSION,
            provider_call_key: "c".repeat(64),
            created_at: created_at.into(),
            created_turn_id: turn_id.into(),
        })
    }

    async fn audit_context_compression(
        &self,
        outcome: crate::seam::ContextCompressionAuditOutcome,
    ) {
        self.audits.borrow_mut().push(outcome);
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        self.requests.borrow_mut().push(request);
        let turn = self
            .turns
            .borrow_mut()
            .pop_front()
            .expect("a scripted compression/main turn")?;
        sink.on_text_delta(&turn.text);
        Ok(turn)
    }
}
#[async_trait(?Send)]
impl ModelSeam for ScriptModel {
    async fn context_policy(
        &self,
        _requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        crate::model_context::PinnedContextPolicy::window(
            SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test"),
            1,
            crate::MIN_MODEL_CONTEXT_BYTES,
        )
        .map_err(|error| AgentError {
            kind: desk_agent_protocol::AgentErrorKind::Internal,
            message: error.to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        self.requests.borrow_mut().push(request);
        let turn = self
            .turns
            .borrow_mut()
            .pop_front()
            .expect("a scripted turn");
        sink.on_text_delta(&turn.text);
        Ok(turn)
    }
}

struct SupersedingModel {
    latest_revision: Rc<RefCell<Option<u64>>>,
}

#[async_trait(?Send)]
impl ModelSeam for SupersedingModel {
    async fn context_policy(
        &self,
        _requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        crate::model_context::PinnedContextPolicy::window(
            SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test"),
            1,
            crate::MIN_MODEL_CONTEXT_BYTES,
        )
        .map_err(model_context_error)
    }

    async fn call(
        &self,
        _request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        *self.latest_revision.borrow_mut() = Some(1);
        let turn = answer("stale answer must be discarded");
        sink.on_text_delta(&turn.text);
        Ok(turn)
    }
}

/// A read-tool seam recording the calls it ran.
struct RecordingTools {
    calls: Rc<RefCell<Vec<String>>>,
    reply: String,
}

struct BackgroundReadTools {
    version_seen: std::cell::Cell<bool>,
    reads: Rc<RefCell<Vec<String>>>,
}

#[async_trait(?Send)]
impl ToolSeam for BackgroundReadTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.reads.borrow_mut().push(call.name.clone());
        Ok(ToolRunOutput {
            content: format!("{}: ok", call.name),
            image_data_url: None,
        })
    }

    fn read_requires_version(&self, call: &ToolCall) -> bool {
        call.name == "long_read"
    }

    async fn run_read_versioned(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
        version: Option<&crate::action_version::ActionVersion>,
    ) -> crate::seam::ReadCompletion {
        if call.name != "long_read" {
            return crate::seam::ReadCompletion {
                outcome: self
                    .run_read(call)
                    .await
                    .map(|output| crate::seam::ReadOutcome {
                        output,
                        ok: true,
                        event_id: None,
                        data_envelope: None,
                        background_task: None,
                    }),
                version_advance: None,
            };
        }
        let version = version.expect("adaptive remote read needs a persisted action fence");
        assert_eq!(version.tool_call_id, call.id);
        assert_eq!(ctx.assistant_turn_fence.as_ref(), Some(&version.turn_fence));
        self.version_seen.set(true);
        crate::seam::ReadCompletion {
            outcome: Ok(crate::seam::ReadOutcome {
                output: ToolRunOutput {
                    content: crate::chat::background_task_running_result("read-task"),
                    image_data_url: None,
                },
                ok: true,
                event_id: None,
                data_envelope: None,
                background_task: Some(crate::session::ActionIdentity::new(
                    17,
                    "read-task",
                    "read-execution",
                    crate::session::WorkKind::ComputerAction,
                )),
            }),
            version_advance: None,
        }
    }
}
#[async_trait(?Send)]
impl ToolSeam for RecordingTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.calls.borrow_mut().push(call.name.clone());
        Ok(ToolRunOutput {
            content: format!("{}: {}", call.name, self.reply),
            image_data_url: None,
        })
    }
}

fn read_tool(name: &str, cap: Capability) -> RegisteredTool {
    RegisteredTool {
        spec: ToolSpec {
            name: name.into(),
            description: "read".into(),
            parameters_schema: serde_json::json!({"type":"object"}),
        },
        required_capability: cap,
        effect: ToolEffect::ReadOnly,
    }
}

fn scope() -> AgentScope {
    AgentScope {
        granted: vec![Capability::SystemInfo, Capability::LogRecent],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    }
}

fn claim() -> ClaimTurnParams {
    ClaimTurnParams {
        conversation_id: "conv".into(),
        actor_id: "actor".into(),
        device_id: "device".into(),
        policy_revision: 1,
        current_pdp_scope: scope(),
        turn_id: "turn-1".into(),
        request_id: Some("req".into()),
        connection_id: Some("conn".into()),
        trigger_origin: crate::session::TriggerOrigin::User,
        now: "2026-06-20T00:00:00Z".into(),
    }
}

#[test]
fn compression_failure_errors_are_closed_and_provider_failures_are_classified() {
    use crate::seam::ContextCompressionFailureKind as Kind;

    let categories = [
        Kind::InputTooLarge,
        Kind::ProviderRejected,
        Kind::ProviderTimeout,
        Kind::Truncated,
        Kind::InvalidSchema,
        Kind::UnsafeOutput,
        Kind::SummaryTooLarge,
        Kind::ProtectedStateTooLarge,
        Kind::ProtectedReplayUnsafe,
        Kind::StaleContext,
        Kind::UnsupportedEndpoint,
        Kind::InvalidEffectiveBudget,
        Kind::AttemptExhausted,
    ];
    let names = categories
        .iter()
        .map(|kind| kind.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(names.len(), categories.len());
    let error = context_compression_error(Kind::InvalidSchema);
    assert_eq!(
        error.message,
        "model context compression failed: invalid_schema"
    );
    assert!(!error.message.contains("provider-secret"));

    let provider_error = |kind, message: &str| AgentError {
        kind,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    };
    assert_eq!(
        compression_failure_for_provider_error(&provider_error(
            desk_agent_protocol::AgentErrorKind::TransportError,
            "provider stream made no upstream progress"
        )),
        Kind::ProviderTimeout
    );
    assert_eq!(
        compression_failure_for_provider_error(&provider_error(
            desk_agent_protocol::AgentErrorKind::TransportError,
            "invalid output limit: manual budget exceeds cap"
        )),
        Kind::InvalidEffectiveBudget
    );
    assert_eq!(
        compression_failure_for_provider_error(&provider_error(
            desk_agent_protocol::AgentErrorKind::TransportError,
            "wire protocol is not implemented"
        )),
        Kind::UnsupportedEndpoint
    );
}

fn answer(text: &str) -> ModelTurn {
    ModelTurn {
        text: text.into(),
        stop_reason: StopReason::EndTurn,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::EndTurn),
        ..Default::default()
    }
}

fn tool_use(id: &str, name: &str) -> ModelTurn {
    tool_use_args(id, name, "{}")
}

fn tool_meta() -> ProviderResponseMeta {
    let source_context_key =
        SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test");
    ProviderResponseMeta {
        stop_reason: StopReason::ToolUse,
        replay: Some(ReplayDisposition::NotRequired { source_context_key }),
        ..Default::default()
    }
}

fn tool_use_args(id: &str, name: &str, args: &str) -> ModelTurn {
    ModelTurn {
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments_json: args.into(),
        }],
        provider_meta: tool_meta(),
        ..Default::default()
    }
}

struct Collector(Rc<RefCell<String>>);
impl TurnSink for Collector {
    fn on_text_delta(&mut self, delta: &str) {
        self.0.borrow_mut().push_str(delta);
    }
}

fn deps<'a>(
    sess: &'a MemSession,
    model: &'a dyn ModelSeam,
    tools: &'a dyn ToolSeam,
    registry: &'a [RegisteredTool],
    clock: &'a dyn Fn() -> String,
) -> LoopDeps<'a> {
    LoopDeps {
        session_seam: sess,
        model,
        tools,
        content_safety: crate::content_safety::ContentSafetyMode::Disabled,
        registry,
        provider_registry: None,
        capability_inventory: None,
        permission_continuation_exact_tools: &[],
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock,
        heartbeat: None,
    }
}

/// A turn that answers immediately: no tools advertised get called, the
/// assistant text is appended, and the turn settles to Idle.
#[tokio::test]
async fn answers_without_tools() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("all good")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut out = String::new();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "how is it?");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    out.push_str(sink.0.borrow().as_str());

    assert_eq!(outcome, LoopOutcome::Answered("all good".into()));
    assert!(tools.calls.borrow().is_empty());
    assert_eq!(out, "all good");
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert_eq!(s.turn_state, TurnState::Idle);
    // user + assistant.
    assert_eq!(s.conversation.len(), 2);
    // The model was offered the granted read tool.
    assert_eq!(model.requests.borrow()[0].tools.len(), 1);
}

#[tokio::test]
async fn device_assistant_request_ends_with_server_input_watermark() {
    let mut seeded = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    seeded.input_revision = 3;
    seeded.latest_input_seq = 3;
    let sess = MemSession {
        inner: RefCell::new(Some(seeded)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".to_string();
    run_agent_turn(
        &deps(&sess, &model, &tools, &[], &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "latest request"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    let requests = requests.borrow();
    let marker = requests[0].messages.last().unwrap();
    assert_eq!(marker.role, ChatRole::SystemEvent);
    assert!(marker.text.contains("input_revision=3"));
    assert!(marker.text.contains("newest user message"));
}

#[tokio::test]
async fn device_assistant_user_followup_reprojects_latest_browser_page_ref() {
    use desk_agent_protocol::browser_control::{
        BROWSER_CONTROL_SCHEMA_VERSION, BrowserActionOutcome, BrowserActionResult,
        BrowserAdapterRef, BrowserEngineKind, BrowserOrigin, BrowserOriginKind, BrowserPageRef,
    };
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary, Sensitivity,
    };

    let mut seeded = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    let page = BrowserPageRef {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        adapter: BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeExtension,
            device_id: "device".into(),
            os_session_id: "session-1".into(),
            browser_major_version: 151,
            browser_version: "151.0.0.0".into(),
            adapter_id: "lcxl-browser-extension".into(),
            adapter_version: "0.1.0".into(),
            profile_incarnation: "profile-1".into(),
            connection_revision: 3,
        },
        page_id: "gmail-page-after-compression".into(),
        page_incarnation: "gmail-document-1".into(),
        origin: BrowserOrigin {
            kind: BrowserOriginKind::Https,
            host_ascii: "mail.google.com".into(),
            port: 443,
        },
        document_revision: 1,
        url_sha256: "b".repeat(64),
        observed_at_unix_ms: 100,
    };
    let result = BrowserActionResult {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        call_id: "browser-call-followup".into(),
        outcome: BrowserActionOutcome::PageOpened,
        page: page.clone(),
        snapshot: None,
        form_readback: Vec::new(),
        completed_at_unix_ms: 101,
    };
    let completion_text = serde_json::json!({
        "work_id": "13",
        "action_request_id": "browser-call-followup",
        "execution_generation": "generation-browser-call-followup",
        "result": "verified",
        "facts": [{
            "index": 0,
            "changed": true,
            "verified": true,
            "summary": "browser action completed with bounded semantic read-back"
        }],
        "message": "browser adapter returned a typed, page-bound result",
        "output": {"kind": "browser", "value": result}
    })
    .to_string();
    let mut tool_result = ChatMessage::untrusted_output(
        "browser-background-result-followup",
        "browser-call-followup",
        "browser-call-followup",
        completion_text.clone(),
    );
    tool_result.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "browser-result-envelope-followup".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "browser-result-blob-followup".into(),
            sha256: format!("{:x}", Sha256::digest(completion_text.as_bytes())),
            size_bytes: completion_text.len() as u64,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "browser.page.open".into(),
            source_tool_name: "browser_open_page".into(),
            source_object_id: Some("browser-call-followup".into()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: format!("{:x}", Sha256::digest(completion_text.as_bytes())),
        sensitivity: Sensitivity::Sensitive,
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    });
    seeded.conversation.push(ChatMessage::assistant_tool_calls(
        "browser-request-followup",
        "",
        vec![ToolCallRef {
            id: "browser-call-followup".into(),
            name: "browser_open_page".into(),
            arguments_json: r#"{"target":{"url":"https://mail.google.com/mail/u/0/"}}"#.into(),
        }],
    ));
    seeded.conversation.push(ChatMessage::tool_result(
        "browser-dispatched-followup",
        "browser-call-followup",
        serde_json::json!({
            "status": "background_running",
            "background_task_id": "browser-call-followup"
        })
        .to_string(),
    ));
    seeded.conversation.push(tool_result);
    let sess = MemSession {
        inner: RefCell::new(Some(seeded)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut followup = ChatMessage::text("u", ChatRole::User, "continue the Gmail task");
    let followup_digest = format!("{:x}", Sha256::digest(followup.text.as_bytes()));
    followup.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "followup-envelope".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "followup-content".into(),
            sha256: followup_digest.clone(),
            size_bytes: followup.text.len() as u64,
            media_type: "text/plain".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "test-user".into(),
            source_tool_name: "send-message".into(),
            source_object_id: Some("u".into()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: followup_digest,
        sensitivity: Sensitivity::UserContent,
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        },
    });

    run_agent_turn(
        &deps(&sess, &model, &tools, &[], &clock),
        claim(),
        followup,
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    let requests = requests.borrow();
    let projection = requests[0]
        .messages
        .iter()
        .find(|message| message.text.contains("CURRENT REUSABLE PROVIDER RESULTS"))
        .expect("ordinary user follow-up must receive the bounded provider result registry");
    assert!(projection.text.contains("gmail-page-after-compression"));
    assert!(
        projection
            .text
            .contains("\"page_reference_prerequisite_present\":true")
    );
    assert!(
        projection
            .text
            .contains("do not claim that no BrowserPageRef exists")
    );
    assert!(!projection.text.contains("raw page title"));
    assert!(
        projection
            .data_envelope
            .as_ref()
            .unwrap()
            .provenance
            .source_envelope_ids
            .contains(&"browser-result-envelope-followup".to_string())
    );
}

#[tokio::test]
async fn device_assistant_followup_batch_repeats_exact_latest_input_at_recency_edge() {
    let mut seeded = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    let mut latest = ChatMessage::text("u2", ChatRole::User, "latest correction");
    let digest = format!("{:x}", Sha256::digest(latest.text.as_bytes()));
    latest.data_envelope = Some(DataEnvelope {
        schema_version: desk_agent_protocol::data_lineage::DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "latest-user-envelope".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "latest-user-content".into(),
            sha256: digest.clone(),
            size_bytes: latest.text.len() as u64,
            media_type: "text/plain".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "test-user".into(),
            source_tool_name: "send-message".into(),
            source_object_id: Some("u2".into()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: digest,
        sensitivity: desk_agent_protocol::data_lineage::Sensitivity::UserContent,
        allowed_destinations: Vec::new(),
        retention: desk_agent_protocol::data_lineage::RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        },
    });
    seeded.conversation = vec![
        ChatMessage::text("u1", ChatRole::User, "old request"),
        latest.clone(),
    ];
    seeded.input_revision = 2;
    seeded.latest_input_seq = 2;
    let sess = MemSession {
        inner: RefCell::new(Some(seeded)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".to_string();
    run_agent_turn(
        &deps(&sess, &model, &tools, &[], &clock),
        claim(),
        latest.clone(),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    let requests = requests.borrow();
    let messages = &requests[0].messages;
    assert_eq!(messages[messages.len() - 2].role, ChatRole::SystemEvent);
    assert_eq!(messages.last().unwrap().role, ChatRole::User);
    assert_eq!(messages.last().unwrap().text, "latest correction");
    assert!(
        messages
            .last()
            .unwrap()
            .message_id
            .starts_with("runtime-latest-input-")
    );
    let projected = messages.last().unwrap().data_envelope.as_ref().unwrap();
    assert_ne!(projected.envelope_id, "latest-user-envelope");
    assert_eq!(
        projected.digest_sha256,
        latest.data_envelope.unwrap().digest_sha256
    );
    assert_eq!(
        projected.provenance.source_envelope_ids,
        vec!["latest-user-envelope"]
    );
}

#[tokio::test]
async fn newer_input_after_model_call_discards_stale_answer_and_settles_once() {
    let sess = MemSession::default();
    let model = SupersedingModel {
        latest_revision: sess.latest_revision.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &[], &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "first request"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::Superseded {
            previous_input_revision: 0,
            current_input_revision: 1,
        }
    );
    assert_eq!(*sess.superseded_settles.borrow(), 1);
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(
        stored
            .conversation
            .iter()
            .all(|message| message.text != "stale answer must be discarded")
    );
}

#[tokio::test]
async fn input_at_first_or_final_save_settles_but_unrelated_save_failure_does_not() {
    for (fail_save_at, model_calls) in [(1, 0), (3, 1)] {
        for supersede in [false, true] {
            let sess = MemSession {
                fail_save_at: Some(fail_save_at),
                supersede_on_save_failure: supersede,
                ..Default::default()
            };
            let requests = Rc::new(RefCell::new(vec![]));
            let model = ScriptModel {
                turns: RefCell::new([answer("answer")].into()),
                requests: requests.clone(),
            };
            let tools = RecordingTools {
                calls: Rc::new(RefCell::new(vec![])),
                reply: "unused".into(),
            };
            let clock = || "2026-06-20T00:00:01Z".to_string();
            let events = Rc::new(RefCell::new(vec![]));
            let outcome = run_agent_turn(
                &deps(&sess, &model, &tools, &[], &clock),
                claim(),
                ChatMessage::text("u", ChatRole::User, "question"),
                &mut SafetyEventLog(events.clone()),
            )
            .await;
            if supersede {
                assert!(matches!(outcome, Ok(LoopOutcome::Superseded { .. })));
            } else {
                assert!(outcome.is_err());
            }
            assert_eq!(*sess.superseded_settles.borrow(), u32::from(supersede));
            assert_eq!(
                events
                    .borrow()
                    .iter()
                    .filter(|event| event.starts_with("retracted:"))
                    .count(),
                usize::from(supersede)
            );
            assert_eq!(requests.borrow().len(), model_calls);
        }
    }
}

#[tokio::test]
async fn older_request_that_loses_preclaim_race_never_calls_model_or_handles_newer_input() {
    let first = ChatMessage::text("u1", ChatRole::User, "first request");
    let second = ChatMessage::text("u2", ChatRole::User, "newer request");
    let mut seeded = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    seeded.conversation = vec![first.clone(), second];
    seeded.latest_input_seq = 2;
    seeded.input_revision = 2;
    let sess = MemSession {
        inner: RefCell::new(Some(seeded)),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new(std::collections::VecDeque::new()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &[], &clock),
        claim(),
        first,
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::Superseded {
            previous_input_revision: 1,
            current_input_revision: 2,
        }
    );
    assert!(model.requests.borrow().is_empty());
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.handled_input_seq, 0);
    assert_eq!(stored.turn_state, TurnState::Idle);
}

/// The internal projection tool updates only advisory session UX state. It does
/// not cross the ToolSeam and the model can continue to a normal final answer.
#[tokio::test]
async fn task_status_tool_updates_projection_without_dispatch() {
    let sess = MemSession::default();
    let mut initial = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    initial.latest_input_seq = 1;
    initial.input_revision = 1;
    *sess.inner.borrow_mut() = Some(initial);
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args(
                    "status-call",
                    crate::task_status_tools::UPDATE_TASK_STATUS_TOOL_NAME,
                    r#"{"items":[{"item_id":"inspect","description":"Inspect workbook","status":"in_progress"}]}"#,
                ),
                answer("working"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "must not run".into(),
    };
    let registry = crate::task_status_tools::task_status_tool_registry();
    let clock = || "2026-06-20T00:00:01Z".to_string();

    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &registry, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "inspect and summarize"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("working".into()));
    assert!(tools.calls.borrow().is_empty());
    let stored = sess.inner.borrow();
    let projection = stored
        .as_ref()
        .unwrap()
        .task_status_projection
        .as_ref()
        .unwrap();
    assert_eq!(projection.revision, 1);
    assert_eq!(projection.items[0].item_id, "inspect");
    assert_eq!(stored.as_ref().unwrap().last_event_seq, 1);
}

/// Permission planning persists a request and pauses. It never calls ToolSeam,
/// and the persisted object is not a grant or dispatch instruction.
#[tokio::test]
async fn permission_planning_records_request_without_dispatch_or_grant() {
    let sess = MemSession::default();
    let mut initial = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    initial.latest_input_seq = 1;
    initial.input_revision = 1;
    *sess.inner.borrow_mut() = Some(initial);
    let model = ScriptModel {
        turns: RefCell::new(
            [tool_use_args(
                "permission-call",
                crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                r#"{"items":[{"item_id":"inspect","provider_id":"desktop.session","tool_name":"inspect_desktop_session","expected_effect":"read_device","resource_scope":["target:device"],"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"Inspect the target requested by the user"}]}"#,
            )]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "must not run".into(),
    };
    let providers = crate::device_assistant::device_assistant_provider_registry();
    let mut registry = vec![
        providers
            .capability(crate::device_assistant::DESKTOP_SESSION_CAPABILITY_ID)
            .unwrap()
            .registered_tool(),
    ];
    registry.extend(crate::permission_tools::permission_planning_tool_registry());
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: "desktop.session".into(),
        capability_id: crate::device_assistant::DESKTOP_SESSION_CAPABILITY_ID.into(),
        tool_name: "inspect_desktop_session".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut loop_deps = deps(&sess, &model, &tools, &registry, &clock);
    loop_deps.provider_registry = Some(&providers);
    loop_deps.capability_inventory = Some(&inventory);

    let outcome = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("u", ChatRole::User, "inspect it"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    let LoopOutcome::PermissionRequested { request_id } = outcome else {
        panic!("permission planning must pause the turn");
    };
    assert!(request_id.starts_with("permission-request-"));
    assert!(tools.calls.borrow().is_empty());
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.permission_requests.len(), 1);
    assert_eq!(
        stored.permission_requests[0].state,
        crate::dynamic_run::PermissionRequestState::Pending
    );
    assert_eq!(stored.last_event_seq, 1);
    assert_eq!(stored.handled_input_seq, 1);
    let encoded = serde_json::to_string(&stored.permission_requests[0]).unwrap();
    assert!(!encoded.contains("grant_id"));
    assert!(!encoded.contains("dispatch"));
}

#[tokio::test]
async fn permission_planning_reuses_settled_equivalent_request_without_new_pending_item() {
    let providers = crate::device_assistant::device_assistant_provider_registry();
    let prior_call = ToolCall {
        id: "prior-call".into(),
        name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
        arguments_json: r#"{"items":[{"item_id":"inspect-a","provider_id":"desktop.session","tool_name":"inspect_desktop_session","expected_effect":"read_device","suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"first wording"}]}"#.into(),
    };
    let mut prior = crate::permission_tools::build_permission_request(
        &prior_call,
        &providers,
        "permission-prior".into(),
        1,
        "2026-06-20T00:00:00Z".into(),
    )
    .unwrap();
    prior.state = crate::dynamic_run::PermissionRequestState::Denied;

    let sess = MemSession::default();
    let mut initial = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    initial.latest_input_seq = 1;
    initial.input_revision = 1;
    initial.add_permission_request(prior).unwrap();
    *sess.inner.borrow_mut() = Some(initial);

    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args(
                    "duplicate-permission-call",
                    crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                    r#"{"items":[{"item_id":"inspect-b","provider_id":"desktop.session","tool_name":"inspect_desktop_session","expected_effect":"read_device","suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"reworded duplicate"}]}"#,
                ),
                answer("I will adapt to the recorded denial."),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "must not run".into(),
    };
    let mut registry = vec![
        providers
            .capability(crate::device_assistant::DESKTOP_SESSION_CAPABILITY_ID)
            .unwrap()
            .registered_tool(),
    ];
    registry.extend(crate::permission_tools::permission_planning_tool_registry());
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: "desktop.session".into(),
        capability_id: crate::device_assistant::DESKTOP_SESSION_CAPABILITY_ID.into(),
        tool_name: "inspect_desktop_session".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut loop_deps = deps(&sess, &model, &tools, &registry, &clock);
    loop_deps.provider_registry = Some(&providers);
    loop_deps.capability_inventory = Some(&inventory);

    let outcome = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("u", ChatRole::User, "continue after the decision"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::Answered("I will adapt to the recorded denial.".into())
    );
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.permission_requests.len(), 1);
    assert_eq!(stored.last_event_seq, 0);
    assert!(stored.conversation.iter().any(|message| {
        message.role == ChatRole::Tool
            && message.text.contains("existing_permission_request")
            && message.text.contains("denied")
    }));
}

#[tokio::test]
async fn compression_precedes_main_call_and_counts_tokens_but_not_steps() {
    let sess = MemSession::default();
    let mut existing = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-19T00:00:00Z",
    );
    existing.conversation = vec![
        ChatMessage::text("old-a", ChatRole::User, "a".repeat(5000)),
        ChatMessage::text("old-b", ChatRole::User, "b".repeat(6000)),
    ];
    *sess.inner.borrow_mut() = Some(existing);

    let source =
        SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test");
    let summary = serde_json::json!({
        "goals": [{"text": "preserve the earlier goal", "source_message_ids": ["old-a"]}],
        "historical_constraints": [], "reported_observations": [],
        "completed_actions": [], "unresolved_questions": [], "next_steps": [],
        "important_identifiers": [], "omitted_evidence": []
    })
    .to_string();
    let compression_turn = ModelTurn {
        text: summary,
        stop_reason: StopReason::EndTurn,
        usage: crate::chat::TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        },
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::EndTurn),
        ..Default::default()
    };
    let mut main_turn = answer("done");
    main_turn.usage = crate::chat::TokenUsage {
        input_tokens: Some(3),
        output_tokens: Some(2),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(Vec::new()));
    let audits = Rc::new(RefCell::new(Vec::new()));
    let model = CompressionScriptModel {
        turns: RefCell::new([Ok(compression_turn), Ok(main_turn)].into()),
        requests: requests.clone(),
        audits: audits.clone(),
        source,
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [
                Ok(safety_verdict(
                    desk_agent_protocol::content_safety::ContentSafetyDecision::Allow,
                    desk_agent_protocol::content_safety::ContentSafetyStage::Output,
                )),
                Ok(safety_verdict(
                    desk_agent_protocol::content_safety::ContentSafetyDecision::Allow,
                    desk_agent_protocol::content_safety::ContentSafetyStage::Output,
                )),
            ]
            .into(),
        ),
        ..Default::default()
    };
    let mut loop_deps = deps(&sess, &model, &tools, &reg, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let current = ChatMessage::text("current", ChatRole::User, "c".repeat(7000));
    let outcome = run_agent_turn(&loop_deps, claim(), current, &mut sink)
        .await
        .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    let requests = requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].use_case,
        crate::model_profile::ModelUseCase::ContextCompression
    );
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].tool_choice, crate::chat::ToolChoice::None);
    assert_ne!(
        requests[1].use_case,
        crate::model_profile::ModelUseCase::ContextCompression
    );
    assert_eq!(requests[1].tools.len(), 1);
    drop(requests);

    let reviews = safety.model_turn_requests.borrow();
    assert_eq!(
        reviews.len(),
        2,
        "candidate summary and main answer are reviewed"
    );
    assert!(reviews[0].text.contains("preserve the earlier goal"));
    assert!(!reviews[0].text.contains(&"a".repeat(100)));
    assert!(!reviews[0].text.contains(&"b".repeat(100)));
    assert!(reviews[0].tool_calls.is_empty());
    assert_eq!(reviews[1].text, "done");
    drop(reviews);

    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.current_turn_steps, 1);
    assert_eq!(stored.current_turn_tokens.input_tokens, Some(13));
    assert_eq!(stored.current_turn_tokens.output_tokens, Some(7));
    assert!(stored.context_notices.iter().any(|notice| {
        notice.kind == crate::model_context::ContextNoticeKind::Compacted
            && notice.checkpoint_generation == Some(1)
    }));
    assert_eq!(sink.0.borrow().as_str(), "done");
    let audits = audits.borrow();
    assert_eq!(audits.len(), 1);
    let crate::seam::ContextCompressionAuditOutcome::Committed {
        context,
        usage,
        summary_context_cost,
        final_context_cost,
    } = &audits[0]
    else {
        panic!("successful compression must emit a committed audit outcome");
    };
    assert_eq!(context.generation, 1);
    assert_eq!(context.covered_from_message_id, "old-a");
    assert!(context.covered_message_count >= 1);
    let safety_audit = context
        .safety
        .as_ref()
        .expect("the independently frozen safety receiver must be correlated");
    assert_eq!(safety_audit.provider_identity_sha256, "a".repeat(64));
    assert_eq!(safety_audit.model_identity_sha256, "b".repeat(64));
    assert_eq!(safety_audit.connection_revision, 3);
    assert_eq!(usage.tokens.input_tokens, Some(10));
    assert!(*summary_context_cost > 0);
    assert!(*final_context_cost > 0);
}

#[tokio::test]
async fn checkpoint_survives_main_call_failure_and_is_reused_by_the_next_turn() {
    use desk_agent_protocol::AgentErrorKind;

    let sess = MemSession::default();
    let mut existing = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-19T00:00:00Z",
    );
    existing.conversation = vec![
        ChatMessage::text("old-a", ChatRole::User, "a".repeat(5000)),
        ChatMessage::text("old-b", ChatRole::User, "b".repeat(6000)),
    ];
    *sess.inner.borrow_mut() = Some(existing);

    let source =
        SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test");
    let compression_turn = ModelTurn {
        text: serde_json::json!({
            "goals": [{"text": "preserve the earlier goal", "source_message_ids": ["old-a"]}],
            "historical_constraints": [], "reported_observations": [],
            "completed_actions": [], "unresolved_questions": [], "next_steps": [],
            "important_identifiers": [], "omitted_evidence": []
        })
        .to_string(),
        stop_reason: StopReason::EndTurn,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::EndTurn),
        ..Default::default()
    };
    let failed_requests = Rc::new(RefCell::new(Vec::new()));
    let first_model = CompressionScriptModel {
        turns: RefCell::new(
            [
                Ok(compression_turn),
                Err(AgentError {
                    kind: AgentErrorKind::TransportError,
                    message: "main provider unavailable".into(),
                    retryable: true,
                    safe_for_model: false,
                    error_code: None,
                }),
            ]
            .into(),
        ),
        requests: failed_requests.clone(),
        audits: Rc::new(RefCell::new(Vec::new())),
        source: source.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    let error = run_agent_turn(
        &deps(&sess, &first_model, &tools, &registry, &clock),
        claim(),
        ChatMessage::text("current", ChatRole::User, "c".repeat(7000)),
        &mut sink,
    )
    .await
    .expect_err("the main provider call is scripted to fail");
    assert_eq!(error.kind, AgentErrorKind::TransportError);
    assert_eq!(failed_requests.borrow().len(), 2);

    let checkpoint_generation = sess
        .inner
        .borrow()
        .as_ref()
        .unwrap()
        .model_context_state
        .entries
        .iter()
        .find_map(|entry| {
            entry
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.v1().generation)
        });
    assert_eq!(checkpoint_generation, Some(1));

    let reused_requests = Rc::new(RefCell::new(Vec::new()));
    let second_model = CompressionScriptModel {
        turns: RefCell::new([Ok(answer("recovered"))].into()),
        requests: reused_requests.clone(),
        audits: Rc::new(RefCell::new(Vec::new())),
        source,
    };
    let mut second_claim = claim();
    second_claim.turn_id = "turn-2".into();
    second_claim.request_id = Some("req-2".into());
    let outcome = run_agent_turn(
        &deps(&sess, &second_model, &tools, &registry, &clock),
        second_claim,
        ChatMessage::text("follow-up", ChatRole::User, "please continue"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("recovered".into()));
    let requests = reused_requests.borrow();
    assert_eq!(
        requests.len(),
        1,
        "checkpoint reuse must avoid a second compression call"
    );
    assert_eq!(
        requests[0].use_case,
        crate::model_profile::ModelUseCase::Agent
    );
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.message_id.starts_with("checkpoint:")),
        "the retried turn must receive the committed checkpoint projection"
    );
}

#[tokio::test]
async fn a_second_compression_need_in_the_same_turn_fails_without_redial() {
    let sess = MemSession::default();
    let mut existing = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-19T00:00:00Z",
    );
    existing.conversation = vec![
        ChatMessage::text("old-a", ChatRole::User, "a".repeat(9000)),
        ChatMessage::text("middle", ChatRole::User, "m".repeat(6000)),
    ];
    *sess.inner.borrow_mut() = Some(existing);

    let source =
        SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test");
    let compression_turn = ModelTurn {
        text: serde_json::json!({
            "goals": [{"text": "old goal", "source_message_ids": ["old-a"]}],
            "historical_constraints": [], "reported_observations": [],
            "completed_actions": [], "unresolved_questions": [], "next_steps": [],
            "important_identifiers": [], "omitted_evidence": []
        })
        .to_string(),
        stop_reason: StopReason::EndTurn,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::EndTurn),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(Vec::new()));
    let audits = Rc::new(RefCell::new(Vec::new()));
    let model = CompressionScriptModel {
        turns: RefCell::new([Ok(compression_turn), Ok(tool_use("call-1", "sysinfo"))].into()),
        requests: requests.clone(),
        audits: audits.clone(),
        source,
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".repeat(8000),
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let error = run_agent_turn(
        &deps(&sess, &model, &tools, &registry, &clock),
        claim(),
        ChatMessage::text("current", ChatRole::User, "c".repeat(3000)),
        &mut sink,
    )
    .await
    .expect_err("a second compression need must fail closed");

    assert_eq!(
        error.message,
        "model context compression failed: attempt_exhausted"
    );
    assert_eq!(
        requests.borrow().len(),
        2,
        "only the first compression and main tool call may reach the provider"
    );
    assert_eq!(tools.calls.borrow().as_slice(), ["sysinfo"]);
    assert!(matches!(
        audits.borrow().last(),
        Some(crate::seam::ContextCompressionAuditOutcome::Failed {
            kind: crate::seam::ContextCompressionFailureKind::AttemptExhausted,
            usage: None,
            ..
        })
    ));
}

#[tokio::test]
async fn rejected_compression_summary_records_usage_but_no_checkpoint_or_notice() {
    let sess = MemSession::default();
    let mut existing = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-19T00:00:00Z",
    );
    existing.conversation = vec![
        ChatMessage::text("old-a", ChatRole::User, "a".repeat(5000)),
        ChatMessage::text("old-b", ChatRole::User, "b".repeat(6000)),
    ];
    *sess.inner.borrow_mut() = Some(existing);

    let source =
        SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test");
    let summary = serde_json::json!({
        "goals": [{"text": "preserve the earlier goal", "source_message_ids": ["old-a"]}],
        "historical_constraints": [], "reported_observations": [],
        "completed_actions": [], "unresolved_questions": [], "next_steps": [],
        "important_identifiers": [], "omitted_evidence": []
    })
    .to_string();
    let compression_turn = ModelTurn {
        text: summary,
        stop_reason: StopReason::EndTurn,
        usage: crate::chat::TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        },
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::EndTurn),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(Vec::new()));
    let audits = Rc::new(RefCell::new(Vec::new()));
    let model = CompressionScriptModel {
        turns: RefCell::new([Ok(compression_turn)].into()),
        requests: requests.clone(),
        audits: audits.clone(),
        source,
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [Ok(safety_verdict(
                desk_agent_protocol::content_safety::ContentSafetyDecision::Block,
                desk_agent_protocol::content_safety::ContentSafetyStage::Output,
            ))]
            .into(),
        ),
        ..Default::default()
    };
    let mut loop_deps = deps(&sess, &model, &tools, &reg, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let error = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("current", ChatRole::User, "c".repeat(7000)),
        &mut sink,
    )
    .await
    .expect_err("a rejected candidate summary must fail the turn");

    assert_eq!(
        error.error_code,
        Some(desk_utils::error::DeskErrorCode::AI_CONTEXT_COMPRESSION_FAILED.code())
    );
    assert_eq!(requests.borrow().len(), 1, "the main model is never called");
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.turn_state, TurnState::Failed);
    assert_eq!(stored.current_turn_steps, 0);
    assert_eq!(stored.current_turn_tokens.input_tokens, Some(10));
    assert_eq!(stored.current_turn_tokens.output_tokens, Some(5));
    assert!(
        stored
            .model_context_state
            .entries
            .iter()
            .all(|entry| entry.checkpoint.is_none())
    );
    assert!(
        stored
            .context_notices
            .iter()
            .all(|notice| notice.kind != crate::model_context::ContextNoticeKind::Compacted)
    );
    assert!(
        sink.0.borrow().is_empty(),
        "compression output is never streamed"
    );
    let audits = audits.borrow();
    assert_eq!(audits.len(), 1);
    let crate::seam::ContextCompressionAuditOutcome::Failed {
        context,
        usage,
        kind,
    } = &audits[0]
    else {
        panic!("rejected compression must emit a failed audit outcome");
    };
    assert!(context.is_some());
    assert_eq!(
        usage.as_ref().and_then(|usage| usage.tokens.input_tokens),
        Some(10)
    );
    assert_eq!(
        *kind,
        crate::seam::ContextCompressionFailureKind::UnsafeOutput
    );
    assert_eq!(
        error.message,
        "model context compression failed: unsafe_output"
    );
}

#[tokio::test]
async fn unhealthy_lease_prevents_compression_dial_and_checkpoint_commit() {
    let sess = MemSession::default();
    let mut existing = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-19T00:00:00Z",
    );
    existing.conversation = vec![
        ChatMessage::text("old-a", ChatRole::User, "a".repeat(5000)),
        ChatMessage::text("old-b", ChatRole::User, "b".repeat(6000)),
    ];
    *sess.inner.borrow_mut() = Some(existing);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let audits = Rc::new(RefCell::new(Vec::new()));
    let model = CompressionScriptModel {
        turns: RefCell::new(std::collections::VecDeque::new()),
        requests: requests.clone(),
        audits: audits.clone(),
        source: SourceContextKey::derive(
            WireProtocol::OpenAiChatCompletions,
            "test",
            "test",
            "test",
        ),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let heartbeat = UnhealthyHeartbeat;
    let mut loop_deps = deps(&sess, &model, &tools, &registry, &clock);
    loop_deps.heartbeat = Some(&heartbeat);
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    let error = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("current", ChatRole::User, "c".repeat(7000)),
        &mut sink,
    )
    .await
    .expect_err("an unhealthy lease must fail before provider I/O");

    assert_eq!(
        error.message,
        "model context compression failed: stale_context"
    );
    assert!(requests.borrow().is_empty());
    assert_eq!(audits.borrow().len(), 1);
    assert!(matches!(
        audits.borrow()[0],
        crate::seam::ContextCompressionAuditOutcome::Failed {
            context: None,
            usage: None,
            kind: crate::seam::ContextCompressionFailureKind::StaleContext,
        }
    ));
    let stored = sess.inner.borrow();
    assert!(
        stored
            .as_ref()
            .unwrap()
            .model_context_state
            .entries
            .is_empty()
    );
}

/// A tool turn followed by an answer: the read tool runs, its result is
/// appended, and the second model call sees user+assistant+tool+...
#[tokio::test]
async fn runs_read_tool_then_answers() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "sysinfo"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "ok".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let deps = LoopDeps {
        max_steps_per_turn: crate::MAX_SAME_TOOL_PER_TURN + 2,
        ..deps(&sess, &model, &tools, &reg, &clock)
    };
    let outcome = run_agent_turn(&deps, claim(), user, &mut sink)
        .await
        .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*tools.calls.borrow(), vec!["sysinfo"]);
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // user, assistant(tool_calls), tool result, assistant(answer).
    assert_eq!(s.conversation.len(), 4);
    assert_eq!(s.conversation[1].role, ChatRole::Assistant);
    assert_eq!(s.conversation[1].tool_calls.len(), 1);
    assert_eq!(s.conversation[2].role, ChatRole::Tool);
    assert_eq!(s.conversation[2].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(s.current_turn_steps, 2);
}

/// A failed selected read still produces model-visible data. The information-
/// flow seam must therefore label its error text before the next model call.
#[tokio::test]
async fn failed_read_tool_result_is_offered_for_data_envelope_labeling() {
    struct FailingRead {
        envelope_inputs: Rc<RefCell<Vec<String>>>,
        safe_for_model: bool,
    }

    #[async_trait(?Send)]
    impl ToolSeam for FailingRead {
        async fn run_read(&self, _call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            Err(AgentError {
                kind: desk_agent_protocol::AgentErrorKind::TransportError,
                message: if self.safe_for_model {
                    "display is not selected"
                } else {
                    "PRIVATE_BACKEND_FAILURE"
                }
                .into(),
                retryable: false,
                safe_for_model: self.safe_for_model,
                error_code: None,
            })
        }

        fn read_data_envelope(
            &self,
            _call: &ToolCall,
            output: &ToolRunOutput,
        ) -> Result<Option<desk_agent_protocol::data_lineage::DataEnvelope>, AgentError> {
            self.envelope_inputs
                .borrow_mut()
                .push(output.content.clone());
            Ok(None)
        }
    }

    for safe_for_model in [true, false] {
        let sess = MemSession::default();
        let requests = Rc::new(RefCell::new(vec![]));
        let model = ScriptModel {
            turns: RefCell::new([tool_use("c1", "sysinfo"), answer("done")].into()),
            requests: Rc::clone(&requests),
        };
        let envelope_inputs = Rc::new(RefCell::new(vec![]));
        let tools = FailingRead {
            envelope_inputs: Rc::clone(&envelope_inputs),
            safe_for_model,
        };
        let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
        let clock = || "t".to_string();
        let mut sink = Collector(Rc::new(RefCell::new(String::new())));

        let outcome = run_agent_turn(
            &deps(&sess, &model, &tools, &registry, &clock),
            claim(),
            ChatMessage::text("u", ChatRole::User, "q"),
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(outcome, LoopOutcome::Answered("done".into()));
        let expected = if safe_for_model {
            "tool error: display is not selected"
        } else {
            "tool error: the tool could not complete"
        };
        assert_eq!(envelope_inputs.borrow().as_slice(), [expected]);
        assert!(
            requests.borrow()[1]
                .messages
                .iter()
                .any(|message| message.role == ChatRole::Tool && message.text == expected)
        );
        assert!(
            !serde_json::to_string(&sess.inner.borrow().as_ref().unwrap().conversation)
                .unwrap()
                .contains("PRIVATE_BACKEND_FAILURE")
        );
    }
}

/// A model that names a tool it was never shown gets an error tool-result (the
/// conversation stays well-formed) rather than the call being executed.
#[tokio::test]
async fn unexposed_tool_call_becomes_error_result() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "ungranted"), answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    // Registry has a tool the scope does NOT grant.
    let reg = vec![read_tool("ungranted", Capability::ContainerList)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("ok".into()));
    assert!(
        tools.calls.borrow().is_empty(),
        "no read ran for an unexposed tool"
    );
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert_eq!(s.conversation[2].role, ChatRole::Tool);
    assert!(s.conversation[2].text.contains("not available"));
}

/// The per-turn step budget stops a model that keeps calling tools forever.
/// Three distinct tools are cycled so the same-tool cap never trips first.
#[tokio::test]
async fn step_budget_circuit_breaks() {
    let sess = MemSession::default();
    let names = ["sysinfo", "logs", "ports"];
    let turns: std::collections::VecDeque<_> = (0..crate::MAX_STEPS_PER_TURN + 5)
        .map(|i| tool_use(&format!("c{i}"), names[i as usize % names.len()]))
        .collect();
    let model = ScriptModel {
        turns: RefCell::new(turns),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![
        read_tool("sysinfo", Capability::SystemInfo),
        read_tool("logs", Capability::LogRecent),
        read_tool("ports", Capability::NetworkPorts),
    ];
    // A scope granting all three so each tool is exposed.
    let mut params = claim();
    params.current_pdp_scope = AgentScope {
        granted: vec![
            Capability::SystemInfo,
            Capability::LogRecent,
            Capability::NetworkPorts,
        ],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        params,
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget)
    );
    let s = sess.inner.borrow();
    assert_eq!(
        s.as_ref().unwrap().current_turn_steps,
        crate::MAX_STEPS_PER_TURN
    );
}

/// A tighter per-turn budget (the terminal copilot uses 2) circuit-breaks
/// sooner than the diagnose default, proving `LoopDeps.max_steps_per_turn`
/// is honored per call.
#[tokio::test]
async fn tight_step_budget_circuit_breaks_at_two() {
    const COPILOT_MAX_STEPS: u32 = 2;
    let sess = MemSession::default();
    let names = ["sysinfo", "logs", "ports"];
    let turns: std::collections::VecDeque<_> = (0..COPILOT_MAX_STEPS + 5)
        .map(|i| tool_use(&format!("c{i}"), names[i as usize % names.len()]))
        .collect();
    let model = ScriptModel {
        turns: RefCell::new(turns),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![
        read_tool("sysinfo", Capability::SystemInfo),
        read_tool("logs", Capability::LogRecent),
        read_tool("ports", Capability::NetworkPorts),
    ];
    let mut params = claim();
    params.current_pdp_scope = AgentScope {
        granted: vec![
            Capability::SystemInfo,
            Capability::LogRecent,
            Capability::NetworkPorts,
        ],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let deps = LoopDeps {
        max_steps_per_turn: COPILOT_MAX_STEPS,
        ..deps(&sess, &model, &tools, &reg, &clock)
    };
    let outcome = run_agent_turn(&deps, params, user, &mut sink)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget)
    );
    let s = sess.inner.borrow();
    assert_eq!(s.as_ref().unwrap().current_turn_steps, COPILOT_MAX_STEPS);
}

/// Repeatedly calling the *same* tool trips the same-tool cap before the step
/// budget.
#[tokio::test]
async fn same_tool_repeat_circuit_breaks() {
    let sess = MemSession::default();
    let mut turns: std::collections::VecDeque<_> = (0..=crate::MAX_SAME_TOOL_PER_TURN)
        .map(|i| tool_use(&format!("c{i}"), "sysinfo"))
        .collect();
    turns.push_back(answer("continued"));
    let model = ScriptModel {
        turns: RefCell::new(turns),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let deps = LoopDeps {
        max_steps_per_turn: crate::MAX_SAME_TOOL_PER_TURN + 2,
        ..deps(&sess, &model, &tools, &reg, &clock)
    };
    let outcome = run_agent_turn(&deps, claim(), user, &mut sink)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::CircuitBreak(CircuitBreakReason::SameToolRepeat)
    );
    // The cap is enforced after the (MAX_SAME_TOOL_PER_TURN + 1)-th call.
    assert_eq!(
        tools.calls.borrow().len(),
        crate::MAX_SAME_TOOL_PER_TURN as usize
    );

    // The skipped over-limit call receives a synthetic result, leaving a valid
    // conversation that a user can continue in the next turn.
    {
        let persisted = sess.inner.borrow();
        let last = persisted
            .as_ref()
            .unwrap()
            .conversation
            .last()
            .expect("synthetic result persisted");
        assert_eq!(last.role, ChatRole::Tool);
        let expected_call_id = format!("c{}", crate::MAX_SAME_TOOL_PER_TURN);
        assert_eq!(
            last.tool_call_id.as_deref(),
            Some(expected_call_id.as_str())
        );
    }

    let mut continuation = claim();
    continuation.turn_id = "turn-continued".into();
    let continued = run_agent_turn(
        &deps,
        continuation,
        ChatMessage::text("u2", ChatRole::User, "continue"),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(continued, LoopOutcome::Answered("continued".into()));
}

/// A protocol violation (EndTurn carrying tool calls) is surfaced and settles
/// the turn to Failed.
#[tokio::test]
async fn protocol_error_fails_turn() {
    let sess = MemSession::default();
    let bad = ModelTurn {
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![ToolCall {
            id: "c".into(),
            name: "sysinfo".into(),
            arguments_json: "{}".into(),
        }],
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([bad].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::ProtocolError(_)));
    assert_eq!(
        sess.inner.borrow().as_ref().unwrap().turn_state,
        TurnState::Failed
    );
}

/// A second turn cannot start while one is running (busy), and a follow-up
/// from a different subject is rejected.
#[tokio::test]
async fn busy_and_subject_guards() {
    // Busy: pre-seed a Running session.
    let sess = MemSession::default();
    {
        let mut s = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t");
        s.begin_turn("prior", None, None, 1, scope(), "t").unwrap();
        *sess.inner.borrow_mut() = Some(s);
    }
    let model = ScriptModel {
        turns: RefCell::new([answer("x")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::TurnBusy);
}

/// The model request is assembled as [system prompt] + conversation: the first
/// message is the agentic system prompt and the user message follows it.
#[tokio::test]
async fn prepends_system_prompt() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    let reqs = model.requests.borrow();
    let msgs = &reqs[0].messages;
    assert_eq!(msgs[0].role, ChatRole::System);
    assert!(msgs[0].text.contains("untrusted DATA"));
    assert_eq!(msgs[1].role, ChatRole::User);
    assert_eq!(msgs[1].text, "q");
}

/// Two sequential turns over the same session continue one conversation: the
/// second turn's model call sees the first turn's user + assistant history
/// followed by the new user message (§9 multi-turn continuation).
#[tokio::test]
async fn follow_up_turn_continues_conversation() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("first"), answer("second")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u1", ChatRole::User, "q1"),
        &mut sink,
    )
    .await
    .unwrap();
    // Second turn: a distinct turn id so minted message ids do not collide.
    let mut c2 = claim();
    c2.turn_id = "turn-2".into();
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        c2,
        ChatMessage::text("u2", ChatRole::User, "q2"),
        &mut sink,
    )
    .await
    .unwrap();
    let reqs = model.requests.borrow();
    let second = &reqs[1].messages;
    let roles: Vec<_> = second.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            ChatRole::System,
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::User
        ]
    );
    assert_eq!(second[1].text, "q1");
    assert_eq!(second[3].text, "q2");
    assert_eq!(
        sess.inner.borrow().as_ref().unwrap().conversation.len(),
        4,
        "both turns persisted in one conversation"
    );
}

/// A tight context budget trims old history out of the model request while the
/// system prompt (prepended on top, not counted) and the newest message stay.
#[tokio::test]
async fn trims_history_to_budget() {
    let sess = MemSession::default();
    {
        let mut s = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t");
        s.conversation.push(ChatMessage::text(
            "old1",
            ChatRole::User,
            "x".repeat(50_000),
        ));
        s.conversation.push(ChatMessage::text(
            "old2",
            ChatRole::Assistant,
            "y".repeat(50_000),
        ));
        *sess.inner.borrow_mut() = Some(s);
    }
    let model = ScriptModel {
        turns: RefCell::new([answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let d = deps(&sess, &model, &tools, &reg, &clock);
    run_agent_turn(
        &d,
        claim(),
        ChatMessage::text("u", ChatRole::User, "recent"),
        &mut sink,
    )
    .await
    .unwrap();
    let reqs = model.requests.borrow();
    let msgs = &reqs[0].messages;
    assert_eq!(msgs[0].role, ChatRole::System);
    assert!(
        msgs.iter().all(|m| !m.text.contains(&"x".repeat(100))),
        "the large old user message was trimmed out"
    );
    assert!(
        msgs.iter().any(|m| m.text == "recent"),
        "the newest message is kept"
    );
}

// ---------------------------- Mutating path ----------------------------

/// A tool seam that scripts mutating outcomes and records read + exec calls.
struct ScriptedTools {
    exec_fences: RefCell<Vec<Option<crate::action_turn_fence::AssistantTurnFence>>>,
    reads: Rc<RefCell<Vec<String>>>,
    execs: RefCell<std::collections::VecDeque<ExecOutcome>>,
    exec_calls: Rc<RefCell<Vec<String>>>,
    acks: Rc<RefCell<Vec<String>>>,
    waits: RefCell<std::collections::VecDeque<WaitOutcome>>,
    wait_calls: Rc<RefCell<Vec<String>>>,
    mutation_envelope_inputs: Rc<RefCell<Vec<String>>>,
    mutation_envelopes: RefCell<std::collections::VecDeque<Option<DataEnvelope>>>,
}
#[async_trait(?Send)]
impl ToolSeam for ScriptedTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.reads.borrow_mut().push(call.name.clone());
        Ok(ToolRunOutput {
            content: format!("{}: ok", call.name),
            image_data_url: None,
        })
    }
    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        self.exec_fences
            .borrow_mut()
            .push(ctx.assistant_turn_fence.clone());
        self.exec_calls.borrow_mut().push(call.id.clone());
        Ok(self
            .execs
            .borrow_mut()
            .pop_front()
            .expect("a scripted exec outcome"))
    }
    fn mutating_data_envelope(
        &self,
        _call: &ToolCall,
        output: &ToolRunOutput,
    ) -> Result<Option<desk_agent_protocol::data_lineage::DataEnvelope>, AgentError> {
        self.mutation_envelope_inputs
            .borrow_mut()
            .push(output.content.clone());
        Ok(self
            .mutation_envelopes
            .borrow_mut()
            .pop_front()
            .unwrap_or(None))
    }
    async fn ack_delivery(&self, event_id: &str) -> Result<(), AgentError> {
        self.acks.borrow_mut().push(event_id.to_string());
        Ok(())
    }
    async fn wait_for_task(
        &self,
        exec_request_id: &str,
        _execution_id: &str,
    ) -> Result<WaitOutcome, AgentError> {
        self.wait_calls
            .borrow_mut()
            .push(exec_request_id.to_string());
        Ok(self
            .waits
            .borrow_mut()
            .pop_front()
            .expect("a scripted wait outcome"))
    }
}

fn mutating_tool(name: &str, cap: Capability) -> RegisteredTool {
    RegisteredTool {
        spec: ToolSpec {
            name: name.into(),
            description: "exec".into(),
            parameters_schema: serde_json::json!({"type":"object"}),
        },
        required_capability: cap,
        effect: ToolEffect::Mutating,
    }
}

/// A scope that exposes the mutating exec tool: grants its capability and runs
/// at `ConfirmEachAction`.
fn exec_scope() -> AgentScope {
    AgentScope {
        granted: vec![Capability::ShellExecConfirmed, Capability::SystemInfo],
        mode: ExecutionMode::ConfirmEachAction,
        expires_at: None,
        policy_name: None,
    }
}

fn exec_claim() -> ClaimTurnParams {
    let mut c = claim();
    c.current_pdp_scope = exec_scope();
    c
}

fn tools(execs: Vec<ExecOutcome>) -> ScriptedTools {
    tools_with_waits(execs, vec![])
}

fn tools_with_waits(execs: Vec<ExecOutcome>, waits: Vec<WaitOutcome>) -> ScriptedTools {
    ScriptedTools {
        exec_fences: RefCell::new(vec![]),
        reads: Rc::new(RefCell::new(vec![])),
        execs: RefCell::new(execs.into()),
        exec_calls: Rc::new(RefCell::new(vec![])),
        acks: Rc::new(RefCell::new(vec![])),
        waits: RefCell::new(waits.into()),
        wait_calls: Rc::new(RefCell::new(vec![])),
        mutation_envelope_inputs: Rc::new(RefCell::new(vec![])),
        mutation_envelopes: RefCell::new(std::collections::VecDeque::new()),
    }
}

fn exec_deps<'a>(
    sess: &'a MemSession,
    model: &'a ScriptModel,
    scripted: &'a ScriptedTools,
    registry: &'a [RegisteredTool],
    clock: &'a dyn Fn() -> String,
) -> LoopDeps<'a> {
    LoopDeps {
        session_seam: sess,
        model,
        tools: scripted,
        content_safety: crate::content_safety::ContentSafetyMode::Disabled,
        registry,
        provider_registry: None,
        capability_inventory: None,
        permission_continuation_exact_tools: &[],
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock,
        heartbeat: None,
    }
}

/// A mutating tool that the operator approves runs to a known result; its
/// result is appended and the turn settles to Idle with no in-flight execution.
#[tokio::test]
async fn mutating_executes_then_answers() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        data_envelope: None,
        output: ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        },
        event_id: None,
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*scripted.exec_calls.borrow(), vec!["c1"]);
    assert_eq!(scripted.exec_fences.borrow().as_slice(), &[None]);
    assert_eq!(
        scripted.mutation_envelope_inputs.borrow().as_slice(),
        ["exit_code=0"]
    );
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // user, assistant(tool_calls), tool result, assistant(answer).
    assert_eq!(s.conversation.len(), 4);
    assert_eq!(s.conversation[2].role, ChatRole::Tool);
    assert_eq!(s.conversation[2].text, "exit_code=0");
    assert_eq!(s.execution_state, ExecutionState::None);
    assert_eq!(s.turn_state, TurnState::Idle);
}

/// When a permission decision carries a recoverable exact input, the first
/// resumed model request exposes the approved mutation but not observation.
/// After that mutation is proposed, normal read tools return so the model can
/// verify the outcome in the same turn.
#[tokio::test]
async fn exact_permission_resume_hides_reobservation_until_mutation_is_proposed() {
    use crate::session::{AgentSessionSurface, TriggerOrigin};

    let sess = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exact_action"),
                tool_use("c2", "inspect"),
                answer("verified"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        data_envelope: None,
        output: ToolRunOutput {
            content: "action completed".into(),
            image_data_url: None,
        },
        event_id: None,
    }]);
    let registry = vec![
        mutating_tool("exact_action", Capability::ShellExecConfirmed),
        read_tool("inspect", Capability::SystemInfo),
    ];
    let exact_tools = vec!["exact_action".to_string()];
    let clock = || "2026-08-30T12:00:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, exec_scope(), "t0");
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    seeded.input_revision = 1;
    seeded.latest_input_seq = 1;
    seeded.conversation.push(ChatMessage::text(
        "owner-requirement",
        ChatRole::User,
        "inspect, preview, execute, then verify",
    ));
    *sess.inner.borrow_mut() = Some(seeded);

    let mut resume_claim = exec_claim();
    resume_claim.trigger_origin = TriggerOrigin::PermissionDecision;
    let mut loop_deps = exec_deps(&sess, &model, &scripted, &registry, &clock);
    loop_deps.permission_continuation_exact_tools = &exact_tools;
    let outcome = resume_agent_turn_after_permission(
        &loop_deps,
        resume_claim,
        ChatMessage::text(
            "permission-decision",
            ChatRole::User,
            "trusted permission decision bridge",
        ),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("verified".into()));
    let fence = scripted.exec_fences.borrow()[0].clone().unwrap();
    assert_eq!(fence.input_revision, 1);
    assert_eq!(
        fence.lease_token,
        sess.inner.borrow().as_ref().unwrap().lease_token
    );
    assert_eq!(
        (fence.actor_id.as_str(), fence.device_id.as_str()),
        ("actor", "device")
    );
    assert_eq!(
        fence.turn_id,
        sess.inner
            .borrow()
            .as_ref()
            .unwrap()
            .current_turn_id
            .clone()
            .unwrap()
    );

    let requests = requests.borrow();
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["exact_action"]
    );
    assert!(
        requests[1].tools.iter().any(|tool| tool.name == "inspect"),
        "observation must be restored after the approved mutation is proposed"
    );
    assert_eq!(*scripted.reads.borrow(), vec!["inspect"]);
}

#[tokio::test]
async fn exact_permission_resume_retries_one_precommit_protocol_error() {
    use crate::session::{AgentSessionSurface, TriggerOrigin};

    let sess = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let invalid = ModelTurn {
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![ToolCall {
            id: "invalid".into(),
            name: "exact_action".into(),
            arguments_json: "{}".into(),
        }],
        provider_meta: ProviderResponseMeta {
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        },
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([invalid, tool_use("c1", "exact_action"), answer("done")].into()),
        requests: requests.clone(),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        data_envelope: None,
        output: ToolRunOutput {
            content: "action completed".into(),
            image_data_url: None,
        },
        event_id: None,
    }]);
    let registry = vec![mutating_tool(
        "exact_action",
        Capability::ShellExecConfirmed,
    )];
    let exact_tools = vec!["exact_action".to_string()];
    let clock = || "2026-08-30T12:00:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, exec_scope(), "t0");
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    seeded.input_revision = 1;
    seeded.latest_input_seq = 1;
    seeded.conversation.push(ChatMessage::text(
        "owner-requirement",
        ChatRole::User,
        "execute the approved action",
    ));
    *sess.inner.borrow_mut() = Some(seeded);

    let mut resume_claim = exec_claim();
    resume_claim.trigger_origin = TriggerOrigin::PermissionDecision;
    let mut loop_deps = exec_deps(&sess, &model, &scripted, &registry, &clock);
    loop_deps.permission_continuation_exact_tools = &exact_tools;
    let outcome = resume_agent_turn_after_permission(
        &loop_deps,
        resume_claim,
        ChatMessage::text(
            "permission-decision",
            ChatRole::User,
            "trusted permission decision bridge",
        ),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*scripted.exec_calls.borrow(), vec!["c1"]);
    let requests = requests.borrow();
    assert_eq!(requests.len(), 3);
    let recovery = requests[1].messages.last().unwrap();
    assert_eq!(recovery.role, ChatRole::SystemEvent);
    assert!(recovery.text.contains("no trailing characters"));
    assert!(recovery.text.contains("approved_exact_input byte-for-byte"));
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| message.text.contains("no trailing characters")),
        "the permission protocol recovery marker must disappear after the approved action is proposed"
    );
}

#[tokio::test]
async fn exact_permission_resume_retries_one_answer_without_invoking_the_approved_tool() {
    use crate::session::{AgentSessionSurface, TriggerOrigin};

    let sess = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                answer("I should call the approved tool now."),
                tool_use("c1", "exact_action"),
                answer("done"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        data_envelope: None,
        output: ToolRunOutput {
            content: "action completed".into(),
            image_data_url: None,
        },
        event_id: None,
    }]);
    let registry = vec![mutating_tool(
        "exact_action",
        Capability::ShellExecConfirmed,
    )];
    let exact_tools = vec!["exact_action".to_string()];
    let clock = || "2026-08-30T12:00:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, exec_scope(), "t0");
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    seeded.input_revision = 1;
    seeded.latest_input_seq = 1;
    seeded.conversation.push(ChatMessage::text(
        "owner-requirement",
        ChatRole::User,
        "execute the approved action",
    ));
    *sess.inner.borrow_mut() = Some(seeded);

    let mut resume_claim = exec_claim();
    resume_claim.trigger_origin = TriggerOrigin::PermissionDecision;
    let mut loop_deps = exec_deps(&sess, &model, &scripted, &registry, &clock);
    loop_deps.permission_continuation_exact_tools = &exact_tools;
    let outcome = resume_agent_turn_after_permission(
        &loop_deps,
        resume_claim,
        ChatMessage::text(
            "permission-decision",
            ChatRole::User,
            "trusted permission decision bridge",
        ),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*scripted.exec_calls.borrow(), vec!["c1"]);
    let requests = requests.borrow();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .messages
            .last()
            .unwrap()
            .text
            .contains("Return exactly one exposed tool call now")
    );
    assert!(
        sess.inner
            .borrow()
            .as_ref()
            .unwrap()
            .conversation
            .iter()
            .all(|message| message.text != "I should call the approved tool now.")
    );
}

#[tokio::test]
async fn exact_permissioned_read_clears_the_continuation_checkpoint_after_the_call() {
    use crate::session::{AgentSessionSurface, TriggerOrigin};

    let sess = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exact_read"),
                tool_use("c2", "inspect"),
                answer("verified"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let calls = Rc::new(RefCell::new(vec![]));
    let recording = RecordingTools {
        calls: calls.clone(),
        reply: "ok".into(),
    };
    let registry = vec![
        read_tool("exact_read", Capability::SystemInfo),
        read_tool("inspect", Capability::LogRecent),
    ];
    let exact_tools = vec!["exact_read".to_string()];
    let clock = || "2026-08-30T12:00:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    seeded.input_revision = 1;
    seeded.latest_input_seq = 1;
    seeded.conversation.push(ChatMessage::text(
        "owner-requirement",
        ChatRole::User,
        "run the approved read and then inspect",
    ));
    *sess.inner.borrow_mut() = Some(seeded);

    let mut resume_claim = claim();
    resume_claim.trigger_origin = TriggerOrigin::PermissionDecision;
    let mut loop_deps = deps(&sess, &model, &recording, &registry, &clock);
    loop_deps.permission_continuation_exact_tools = &exact_tools;
    let outcome = resume_agent_turn_after_permission(
        &loop_deps,
        resume_claim,
        ChatMessage::text(
            "permission-decision",
            ChatRole::User,
            "trusted permission decision bridge",
        ),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("verified".into()));
    assert_eq!(*calls.borrow(), vec!["exact_read", "inspect"]);
    let requests = requests.borrow();
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["exact_read"]
    );
    assert!(requests[1].tools.iter().any(|tool| tool.name == "inspect"));
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|message| !message.text.contains("PERMISSION CONTINUATION CHECKPOINT"))
    );
}

#[tokio::test]
async fn device_assistant_retries_one_malformed_permission_plan_after_tool_result() {
    use crate::session::AgentSessionSurface;
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DestinationIdentity, RetentionBoundary, Sensitivity,
    };

    let mut seeded = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    seeded.input_revision = 1;
    seeded.latest_input_seq = 1;
    let sess = MemSession {
        inner: RefCell::new(Some(seeded)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("read", "sysinfo"),
                tool_use_args(
                    "malformed-plan",
                    crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                    "{\"items\":[",
                ),
                answer("recovered"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "read completed".into(),
    };
    let registry = vec![
        read_tool("sysinfo", Capability::SystemInfo),
        RegisteredTool {
            spec: ToolSpec {
                name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
                description: "request permission".into(),
                parameters_schema: serde_json::json!({"type": "object"}),
            },
            required_capability: Capability::SystemInfo,
            effect: ToolEffect::RunProjection,
        },
    ];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let mut user = ChatMessage::text("user", ChatRole::User, "inspect, then request permission");
    let user_digest = format!("{:x}", Sha256::digest(user.text.as_bytes()));
    user.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "current-user-envelope".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "current-user-content".into(),
            sha256: user_digest.clone(),
            size_bytes: user.text.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "test-user".into(),
            source_tool_name: "send-message".into(),
            source_object_id: Some("user".into()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: user_digest,
        sensitivity: Sensitivity::UserContent,
        allowed_destinations: vec![destination.clone()],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        },
    });
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &registry, &clock),
        claim(),
        user,
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("recovered".into()));
    assert_eq!(*tools.calls.borrow(), vec!["sysinfo"]);
    let requests = requests.borrow();
    assert_eq!(requests.len(), 3);
    let marker = requests[2].messages.last().unwrap();
    assert_eq!(marker.role, ChatRole::SystemEvent);
    assert!(marker.text.contains("invalid JSON arguments"));
    assert!(
        marker
            .text
            .contains("do not repeat the preceding Provider action")
    );
    let marker_envelope = marker.data_envelope.as_ref().unwrap();
    assert_eq!(marker_envelope.allowed_destinations, vec![destination]);
    assert_eq!(
        marker_envelope.provenance.source_envelope_ids,
        vec!["current-user-envelope"]
    );
}

#[tokio::test]
async fn durable_dispatch_refusal_does_not_claim_owner_rejection_or_execution() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("not run")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::NotExecuted {
        reason: "input changed".into(),
    }]);
    let registry = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &registry, &|| "now".into()),
        exec_claim(),
        ChatMessage::text("user", ChatRole::User, "do it"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let snapshot = sess.inner.borrow();
    let snapshot = snapshot.as_ref().unwrap();
    let result = snapshot
        .conversation
        .iter()
        .find(|message| message.role == ChatRole::Tool)
        .unwrap();
    assert_eq!(result.text, "not executed: input changed");
    assert!(!result.text.contains("operator") && !result.text.contains("cancelled"));
    assert_eq!(snapshot.execution_state, ExecutionState::None);
}

/// An automation turn ([`TriggerOrigin::ExecCompletion`]) never runs a mutating
/// tool: the exec tool is not advertised (layer 1), and a model that names it
/// anyway is answered with "not available" without the tool seam being called —
/// so a completion cannot self-trigger a new command. The read tool still works.
#[tokio::test]
async fn automation_turn_cannot_start_a_new_command() {
    use crate::session::TriggerOrigin;
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exec_command"),
                tool_use("c2", "read_sys"),
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    // The seam would panic if asked to execute (no scripted outcomes), proving
    // it is never reached for the mutating call.
    let scripted = tools(vec![]);
    let reg = vec![
        mutating_tool("exec_command", Capability::ShellExecConfirmed),
        read_tool("read_sys", Capability::SystemInfo),
    ];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let mut claim = exec_claim();
    claim.trigger_origin = TriggerOrigin::ExecCompletion;
    let user = ChatMessage::text("u", ChatRole::User, "the prior command finished");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        claim,
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    // The tool seam was never asked to execute anything.
    assert!(scripted.exec_calls.borrow().is_empty());

    // Layer 1: the first model call did not advertise the mutating tool, but did
    // advertise the read tool.
    let reqs = model.requests.borrow();
    let first: Vec<_> = reqs[0].tools.iter().map(|t| t.name.clone()).collect();
    assert!(
        !first.contains(&"exec_command".to_string()),
        "an automation turn must not be offered a mutating tool"
    );
    assert!(first.contains(&"read_sys".to_string()));

    // The exec call was rejected as not available (the seam was never reached).
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    let rejected = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert!(rejected.text.contains("not available"));
    assert_eq!(s.execution_state, ExecutionState::None);
}

/// When the model reacts to a request that contained a pending auto-trigger's
/// completion message, the loop drops that pending entry (the model handled it,
/// so no automation turn should fire) — but leaves a pending entry whose
/// completion the model never saw.
#[tokio::test]
async fn reacting_to_a_completion_clears_its_pending_trigger() {
    use crate::session::PendingAutoTrigger;
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("acknowledged")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![]);
    let reg: Vec<RegisteredTool> = vec![];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    // Seed a session whose conversation already carries a completion message
    // (id "done-1") plus a pending entry keyed on it, and a second pending entry
    // ("done-absent") whose message is not in the conversation.
    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    seeded.conversation.push(ChatMessage::untrusted_output(
        "done-1",
        "call-1",
        "task-1",
        "exit_code=0",
    ));
    seeded.add_pending_auto_trigger(PendingAutoTrigger {
        work_id: 1,
        kind: crate::session::WorkKind::AgentExec,
        execution_id: "e1".into(),
        tool_call_id: "c1".into(),
        event_id: "done-1".into(),
        chain_id: "chain".into(),
        resolution_org_id: None,
        since: "t0".into(),
    });
    seeded.add_pending_auto_trigger(PendingAutoTrigger {
        work_id: 2,
        kind: crate::session::WorkKind::AgentExec,
        execution_id: "e2".into(),
        tool_call_id: "c2".into(),
        event_id: "done-absent".into(),
        chain_id: "chain".into(),
        resolution_org_id: None,
        since: "t0".into(),
    });
    *sess.inner.borrow_mut() = Some(seeded);

    let user = ChatMessage::text("u", ChatRole::User, "what happened?");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("acknowledged".into()));

    let s = sess.inner.borrow();
    let pending = &s.as_ref().unwrap().pending_auto_triggers;
    // "done-1" was in the request the model answered, so it is drained; the
    // entry the model never saw survives.
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event_id, "done-absent");
}

/// An automation resume appends no message: it runs against the completion
/// already at the tail of the conversation, the model sees it in the request,
/// and reacting drains its pending entry.
#[tokio::test]
async fn generic_work_completion_resume_runs_against_the_existing_tail_without_appending() {
    use crate::session::{PendingAutoTrigger, TriggerOrigin, WorkKind};
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("looked at it")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![]);
    let reg: Vec<RegisteredTool> = vec![];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    seeded.conversation.push(ChatMessage::untrusted_output(
        "done-1",
        "call-1",
        "task-1",
        "exit_code=0",
    ));
    seeded.add_pending_auto_trigger(PendingAutoTrigger {
        work_id: 1,
        kind: WorkKind::OfficePatch,
        execution_id: "e1".into(),
        tool_call_id: "c1".into(),
        event_id: "done-1".into(),
        chain_id: "chain".into(),
        resolution_org_id: None,
        since: "t0".into(),
    });
    let convo_len = seeded.conversation.len();
    *sess.inner.borrow_mut() = Some(seeded);

    let mut claim = claim();
    claim.trigger_origin = TriggerOrigin::WorkCompletion {
        kind: WorkKind::OfficePatch,
    };
    let outcome = resume_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        claim,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("looked at it".into()));

    // The model saw the completion in its request.
    let reqs = model.requests.borrow();
    assert!(
        reqs[0].messages.iter().any(|m| m.message_id == "done-1"),
        "the resumed turn puts the completion in the model request"
    );

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // No user message was appended — only the assistant answer grew the tail.
    assert_eq!(s.conversation.len(), convo_len + 1);
    assert_eq!(s.conversation.last().unwrap().text, "looked at it");
    // The pending entry the model reacted to is drained.
    assert!(s.pending_auto_triggers.is_empty());
}

/// A permission resume puts a server-authored checkpoint at the recency edge
/// so the model consumes an active exact grant instead of repeating the read /
/// preview that created its ephemeral object reference.
#[tokio::test]
async fn permission_resume_places_authorization_checkpoint_at_request_tail() {
    use crate::session::{AgentSessionSurface, TriggerOrigin};

    let sess = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([answer("continued")].into()),
        requests: requests.clone(),
    };
    let scripted = tools(vec![]);
    let registry: Vec<RegisteredTool> = vec![];
    let clock = || "2026-08-30T12:00:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    seeded.surface = AgentSessionSurface::DeviceAssistant;
    seeded.conversation.push(ChatMessage::text(
        "owner-requirement",
        ChatRole::User,
        "inspect, preview, then run the exact action",
    ));
    *sess.inner.borrow_mut() = Some(seeded);

    let mut resume_claim = claim();
    resume_claim.trigger_origin = TriggerOrigin::PermissionDecision;
    let outcome = resume_agent_turn_after_permission(
        &deps(&sess, &model, &scripted, &registry, &clock),
        resume_claim,
        ChatMessage::text(
            "permission-decision",
            ChatRole::User,
            "trusted permission decision bridge",
        ),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("continued".into()));

    let requests = requests.borrow();
    let tail = requests[0].messages.last().unwrap();
    assert!(
        tail.text
            .starts_with("PERMISSION CONTINUATION CHECKPOINT (server authoritative)")
    );
    assert!(tail.text.contains("call that tool now"));
    assert!(tail.text.contains("Do not inspect again"));
    assert!(
        tail.message_id
            .starts_with("runtime-permission-continuation-")
    );
}

/// An unknown-outcome execution closes the conversation with a placeholder tool
/// result, records `OutcomeUnknown`, and hides the mutating tool from the next
/// model call (only read-only follow-up); the late result reconciles it later.
#[tokio::test]
async fn mutating_unknown_closes_with_placeholder_and_hides_mutating() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exec_command"),
                tool_use("c2", "read_sys"),
                answer("status"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Unknown(
        crate::session::ActionIdentity::agent_exec(5, "r1", "e1"),
    )]);
    let reg = vec![
        mutating_tool("exec_command", Capability::ShellExecConfirmed),
        read_tool("read_sys", Capability::SystemInfo),
    ];
    let clock = || "2026-06-20T00:00:09Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("status".into()));
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // The placeholder is recorded with the unknown outcome.
    match &s.execution_state {
        ExecutionState::OutcomeUnknown {
            action,
            placeholder_message_id,
            ..
        } => {
            assert_eq!(action.execution_id, "e1");
            let ph = s
                .conversation
                .iter()
                .find(|m| &m.message_id == placeholder_message_id)
                .unwrap();
            assert_eq!(ph.tool_call_id.as_deref(), Some("c1"));
            assert!(ph.text.contains("outcome unknown"));
        }
        other => panic!("expected OutcomeUnknown, got {other:?}"),
    }
    // The follow-up model call did not advertise the mutating tool (no new
    // mutation while an outcome is unknown), but kept the read tool.
    let reqs = model.requests.borrow();
    let follow_up: Vec<_> = reqs[1].tools.iter().map(|t| t.name.clone()).collect();
    assert!(!follow_up.contains(&"exec_command".to_string()));
    assert!(follow_up.contains(&"read_sys".to_string()));
    // The first model call DID advertise the mutating tool.
    let first: Vec<_> = reqs[0].tools.iter().map(|t| t.name.clone()).collect();
    assert!(first.contains(&"exec_command".to_string()));
}

/// A dispatched-to-background outcome closes the tool call with a task-id result
/// and records `Executing`; the conversation is not degraded (a result is
/// coming) but no second mutation is offered until it completes.
#[tokio::test]
async fn mutating_dispatched_closes_with_task_id_and_hides_mutating() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exec_command"),
                tool_use("c2", "read_sys"),
                answer("status"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Dispatched(
        crate::session::ActionIdentity::agent_exec(8, "exec_task9", "e9"),
    )]);
    let reg = vec![
        mutating_tool("exec_command", Capability::ShellExecConfirmed),
        read_tool("read_sys", Capability::SystemInfo),
    ];
    let clock = || "2026-06-20T00:00:09Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "run a long job");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("status".into()));
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // The dispatch is recorded as an outstanding execution, not an unknown one.
    match &s.execution_state {
        ExecutionState::Executing { action } => {
            assert_eq!(action.work_id, 8);
            assert_eq!(action.execution_id, "e9");
            assert_eq!(action.action_request_id, "exec_task9");
        }
        other => panic!("expected Executing, got {other:?}"),
    }
    // The tool call is closed with a task-id result naming the running task.
    let closed = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert_eq!(
        closed.background_task_id.as_deref(),
        Some("exec_task9"),
        "the UI correlation id is persisted separately from model output"
    );
    let dispatch: serde_json::Value = serde_json::from_str(&closed.text).unwrap();
    assert_eq!(dispatch["status"], "background_running");
    assert_eq!(dispatch["background_task_id"], "exec_task9");
    // A dispatched task leaves its completion delivery for the publisher — the
    // foreground never acks it.
    assert!(scripted.acks.borrow().is_empty());
    // The follow-up model call did not advertise the mutating tool (no second
    // mutation while one is running) but kept the read tool.
    let reqs = model.requests.borrow();
    let replayed_dispatch = reqs[1]
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("c1"))
        .expect("the model follow-up sees the dispatch result");
    let replayed_json: serde_json::Value = serde_json::from_str(&replayed_dispatch.text).unwrap();
    assert_eq!(replayed_json["background_task_id"], "exec_task9");
    let follow_up: Vec<_> = reqs[1].tools.iter().map(|t| t.name.clone()).collect();
    assert!(!follow_up.contains(&"exec_command".to_string()));
    assert!(follow_up.contains(&"read_sys".to_string()));
}

/// Seed a session sitting on a dispatched background task, ready for a follow-up
/// turn that waits on it.
fn seeded_executing() -> MemSession {
    let mut s = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        exec_scope(),
        "2026-06-20T00:00:00Z",
    );
    s.execution_state = ExecutionState::Executing {
        action: crate::session::ActionIdentity::agent_exec(8, "exec_task9", "e9"),
    };
    MemSession {
        inner: RefCell::new(Some(s)),
        ..Default::default()
    }
}

/// The registry offered while a task is in flight: the exec tool (hidden by the
/// exposure matrix while `Executing`) plus the wait tool.
fn wait_reg() -> Vec<RegisteredTool> {
    let mut reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    reg.extend(crate::wait_tools::wait_tool_registry());
    reg
}

/// A `wait_for_task` that completes clears `Executing`, keys the result on the
/// delivery id (so a racing publisher dedups), and acks the delivery so the
/// publisher stands down.
#[tokio::test]
async fn wait_for_task_completes_clears_executing_and_acks() {
    let sess = seeded_executing();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("it finished"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(
        vec![],
        vec![WaitOutcome::Completed {
            output: ToolRunOutput {
                content: "exit_code=0".into(),
                image_data_url: None,
            },
            event_id: Some("work:8:done".into()),
        }],
    );
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "is it done?");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert_eq!(
        s.execution_state,
        ExecutionState::None,
        "the awaited task settled; mutation is allowed again"
    );
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert_eq!(result.message_id, "work:8:done", "keyed on the delivery id");
    assert_eq!(result.text, "exit_code=0");
    assert_eq!(
        *scripted.wait_calls.borrow(),
        vec!["exec_task9".to_string()]
    );
    assert_eq!(*scripted.acks.borrow(), vec!["work:8:done".to_string()]);
}

/// A `wait_for_task` that times out with the task still running leaves it in
/// flight (`Executing`) and does not ack any delivery.
#[tokio::test]
async fn wait_for_task_still_running_keeps_the_task() {
    let sess = seeded_executing();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("still going"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(vec![], vec![WaitOutcome::StillRunning]);
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "is it done?");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert!(
        matches!(s.execution_state, ExecutionState::Executing { .. }),
        "the task is still running"
    );
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert_eq!(result.background_task_id.as_deref(), Some("exec_task9"));
    let dispatch: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(dispatch["status"], "background_running");
    assert_eq!(dispatch["background_task_id"], "exec_task9");
    assert!(scripted.acks.borrow().is_empty());
}

/// A `wait_for_task` whose task became unknown degrades to `OutcomeUnknown`,
/// anchored on this call's own result so a late real result can reconcile it.
#[tokio::test]
async fn wait_for_task_unknown_degrades_to_outcome_unknown() {
    let sess = seeded_executing();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("its outcome is unknown"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(vec![], vec![WaitOutcome::Unknown]);
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "is it done?");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    match &s.execution_state {
        ExecutionState::OutcomeUnknown {
            action,
            placeholder_message_id,
            ..
        } => {
            assert_eq!(action.execution_id, "e9");
            // The placeholder anchors on this wait call's own result message.
            let anchor = s
                .conversation
                .iter()
                .find(|m| &m.message_id == placeholder_message_id)
                .unwrap();
            assert_eq!(anchor.tool_call_id.as_deref(), Some("c2"));
        }
        other => panic!("expected OutcomeUnknown, got {other:?}"),
    }
    assert!(scripted.acks.borrow().is_empty());
}

/// An executed result carrying a stable delivery id keys the tool-result
/// message on that id, so a late completion delivery of the same result dedups
/// against it instead of appending a duplicate.
#[tokio::test]
async fn executed_keys_the_result_message_on_the_delivery_id() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        data_envelope: None,
        output: ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        },
        event_id: Some("work:8:done".into()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert_eq!(result.message_id, "work:8:done");
    assert_eq!(result.text, "exit_code=0");
    // The foreground path acked the delivery (post-save) so the publisher stands
    // down.
    assert_eq!(*scripted.acks.borrow(), vec!["work:8:done".to_string()]);
}

/// Two mutating calls in one turn run serially: a rejection halts the rest, so
/// the second is skipped (not executed) but still gets a tool result.
#[tokio::test]
async fn mutating_rejected_skips_remaining_in_turn() {
    let sess = MemSession::default();
    let mut two = ModelTurn {
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![
            ToolCall {
                id: "c1".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            },
            ToolCall {
                id: "c2".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            },
        ],
        provider_meta: tool_meta(),
        ..Default::default()
    };
    two.provider_meta.data_envelope = Some(DataEnvelope {
        schema_version: desk_agent_protocol::data_lineage::DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "model-output-two-mutations".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "model-output-two-mutations-content".into(),
            sha256: "a".repeat(64),
            size_bytes: 1,
            media_type: "application/json".into(),
        },
        provenance: desk_agent_protocol::data_lineage::DataProvenance {
            source_provider_id: "external-model".into(),
            source_tool_name: "model-response".into(),
            source_object_id: None,
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: "a".repeat(64),
        sensitivity: desk_agent_protocol::data_lineage::Sensitivity::Sensitive,
        allowed_destinations: vec![
            desk_agent_protocol::data_lineage::DestinationIdentity::LocalArtifact {
                workspace_id: "test".into(),
            },
        ],
        retention: desk_agent_protocol::data_lineage::RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        },
    });
    let model = ScriptModel {
        turns: RefCell::new([two, answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Rejected {
        reason: Some("not now".into()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart both");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("ok".into()));
    // Only the first call was attempted; the second was skipped.
    assert_eq!(*scripted.exec_calls.borrow(), vec!["c1"]);
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // user, assistant(2 calls), rejected result(c1), skipped result(c2), answer.
    assert_eq!(s.conversation.len(), 5);
    assert_eq!(s.conversation[2].tool_call_id.as_deref(), Some("c1"));
    assert!(s.conversation[2].text.contains("rejected"));
    assert_eq!(s.conversation[3].tool_call_id.as_deref(), Some("c2"));
    assert!(s.conversation[3].text.contains("not executed"));
    let skipped_envelope = s.conversation[3]
        .data_envelope
        .as_ref()
        .expect("a skipped tool result stays model-egress labeled");
    assert_eq!(
        skipped_envelope.provenance.source_tool_name,
        "halted_tool_call"
    );
    assert_eq!(
        skipped_envelope.provenance.source_envelope_ids,
        vec!["model-output-two-mutations"]
    );
    assert_eq!(s.execution_state, ExecutionState::None);
}

/// A command cancelled before it dispatched closes the call with a truthful
/// "cancelled" result (not "rejected"), leaves the execution machine clean, and
/// halts the rest of the turn.
#[tokio::test]
async fn mutating_cancelled_before_dispatch_closes_truthfully() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Cancelled {
        reason: Some("operator stopped it".into()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert!(result.text.contains("cancelled"));
    assert!(!result.text.contains("rejected"));
    assert_eq!(
        s.execution_state,
        ExecutionState::None,
        "a never-dispatched cancel leaves the machine clean"
    );
}

/// A backend transport error from the mutating seam (not model-safe) fails the
/// turn rather than becoming a tool result.
#[tokio::test]
async fn mutating_backend_error_fails_turn() {
    struct FailingExec;
    #[async_trait(?Send)]
    impl ToolSeam for FailingExec {
        async fn run_read(&self, _call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            unreachable!("no read in this test")
        }
        async fn confirm_and_exec(
            &self,
            _call: &ToolCall,
            _ctx: &ExecContext,
        ) -> Result<ExecOutcome, AgentError> {
            Err(AgentError {
                kind: desk_agent_protocol::AgentErrorKind::Internal,
                message: "db down".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })
        }
    }
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let failing = FailingExec;
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "do it");
    let deps = LoopDeps {
        session_seam: &sess,
        model: &model,
        tools: &failing,
        content_safety: crate::content_safety::ContentSafetyMode::Disabled,
        registry: &reg,
        provider_registry: None,
        capability_inventory: None,
        permission_continuation_exact_tools: &[],
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock: &clock,
        heartbeat: None,
    };
    let err = run_agent_turn(&deps, exec_claim(), user, &mut sink)
        .await
        .unwrap_err();
    assert_eq!(err.kind, desk_agent_protocol::AgentErrorKind::Internal);
    // The turn settled to Failed.
    assert_eq!(
        sess.inner.borrow().as_ref().unwrap().turn_state,
        TurnState::Failed
    );
}

/// A model-safe pre-dispatch error may explicitly allow the model to correct its
/// arguments. The error result is included in the next request, and a corrected
/// command can execute in the same user turn.
#[tokio::test]
async fn retryable_mutating_error_returns_to_model_for_correction() {
    struct RetryableThenExecuted {
        calls: RefCell<u32>,
    }
    #[async_trait(?Send)]
    impl ToolSeam for RetryableThenExecuted {
        async fn run_read(&self, _call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            unreachable!("no read in this test")
        }

        async fn confirm_and_exec(
            &self,
            _call: &ToolCall,
            _ctx: &ExecContext,
        ) -> Result<ExecOutcome, AgentError> {
            let mut calls = self.calls.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                Err(AgentError {
                    kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
                    message: r#"{"error_code":"unsupported_exec_shell","requested_shell":"bash","available_shells":["powershell"],"retryable":true}"#.into(),
                    retryable: true,
                    safe_for_model: true,
                    error_code: Some(
                        desk_utils::error::DeskErrorCode::AI_EXEC_SHELL_UNSUPPORTED.code(),
                    ),
                })
            } else {
                Ok(ExecOutcome::Executed {
                    data_envelope: None,
                    output: ToolRunOutput {
                        content: "exit_code=0".into(),
                        image_data_url: None,
                    },
                    event_id: None,
                })
            }
        }
    }

    let sess = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args(
                    "c1",
                    "exec_command",
                    r#"{"command":"sleep 1","shell":"bash"}"#,
                ),
                tool_use_args(
                    "c2",
                    "exec_command",
                    r#"{"command":"Start-Sleep 1","shell":"powershell"}"#,
                ),
                answer("done"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = RetryableThenExecuted {
        calls: RefCell::new(0),
    };
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "sleep briefly");
    let deps = LoopDeps {
        session_seam: &sess,
        model: &model,
        tools: &tools,
        content_safety: crate::content_safety::ContentSafetyMode::Disabled,
        registry: &reg,
        provider_registry: None,
        capability_inventory: None,
        permission_continuation_exact_tools: &[],
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock: &clock,
        heartbeat: None,
    };

    let outcome = run_agent_turn(&deps, exec_claim(), user, &mut sink)
        .await
        .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*tools.calls.borrow(), 2);
    assert_eq!(requests.borrow().len(), 3);
    assert!(
        requests.borrow()[1]
            .messages
            .iter()
            .any(|message| message.text.contains("unsupported_exec_shell")),
        "the correction step must see the structured shell error"
    );
}

// ---------------------------- Streaming lifecycle ----------------------------

/// A sink that records every lifecycle event in order (text deltas excluded so
/// the assertions key on the structured events).
struct EventLog(Rc<RefCell<Vec<String>>>);
impl TurnSink for EventLog {
    fn on_text_delta(&mut self, _delta: &str) {}
    fn on_partial_committed(&mut self) {
        self.0.borrow_mut().push("committed".into());
    }
    fn on_turn_retracted(
        &mut self,
        _reason: desk_agent_protocol::content_safety::StreamRetractionReason,
        _error: Option<desk_agent_protocol::AgentError>,
    ) {
        self.0.borrow_mut().push("retracted".into());
    }
    fn on_tool_started(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        self.0
            .borrow_mut()
            .push(format!("started:{tool_name}:{call_id}:{arguments_json}"));
    }
    fn on_awaiting_approval(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        self.0
            .borrow_mut()
            .push(format!("approval:{tool_name}:{call_id}:{arguments_json}"));
    }
    fn on_tool_finished(
        &mut self,
        call_id: &str,
        ok: bool,
        output: &str,
        background_task_id: Option<&str>,
    ) {
        let background = background_task_id
            .map(|id| format!(":{id}"))
            .unwrap_or_default();
        self.0
            .borrow_mut()
            .push(format!("finished:{call_id}:{ok}:{output}{background}"));
    }
    fn on_answer_committed(&mut self, text: &str) {
        self.0.borrow_mut().push(format!("answer:{text}"));
    }
    fn on_turn_discarded(&mut self) {
        self.0.borrow_mut().push("discarded".into());
    }
}

/// A read-tool turn emits start → finish(ok) → answer events in order.
#[tokio::test]
async fn streams_read_tool_lifecycle_events() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "sysinfo"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "ok".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let user = ChatMessage::text("u", ChatRole::User, "q");
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(
        *log.borrow(),
        vec![
            "started:sysinfo:c1:{}".to_string(),
            "finished:c1:true:sysinfo: ok".to_string(),
            "answer:done".to_string(),
        ]
    );
}

#[tokio::test]
async fn adaptive_read_closes_with_background_identity_and_keeps_followup_reads_available() {
    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    seeded.surface = crate::session::AgentSessionSurface::DeviceAssistant;
    seeded.input_revision = 1;
    seeded.latest_input_seq = 1;
    let sess = MemSession {
        inner: RefCell::new(Some(seeded)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "long_read"),
                tool_use("c2", "read_sys"),
                answer("status"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = BackgroundReadTools {
        version_seen: std::cell::Cell::new(false),
        reads: Rc::new(RefCell::new(vec![])),
    };
    let registry = vec![
        read_tool("long_read", Capability::SystemInfo),
        read_tool("read_sys", Capability::LogRecent),
    ];
    let clock = || "2026-08-31T00:00:00Z".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &registry, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "wait for it"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("status".into()));
    assert!(tools.version_seen.get());
    assert_eq!(*tools.reads.borrow(), vec!["read_sys"]);
    let saved = sess.inner.borrow();
    let saved = saved.as_ref().unwrap();
    assert!(matches!(
        &saved.execution_state,
        ExecutionState::Executing { action }
            if action.work_id == 17
                && action.action_request_id == "read-task"
                && action.execution_id == "read-execution"
    ));
    let result = saved
        .conversation
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert_eq!(result.background_task_id.as_deref(), Some("read-task"));
    assert!(
        requests.borrow()[1]
            .tools
            .iter()
            .any(|tool| tool.name == "read_sys")
    );
    assert!(
        log.borrow().iter().any(|event| {
            event.starts_with("finished:c1:true:") && event.ends_with(":read-task")
        })
    );
}

/// A mutating turn emits an awaiting-approval event (not a read start) before
/// the result, then finish(ok) and the answer.
#[tokio::test]
async fn streams_awaiting_approval_event() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        data_envelope: None,
        output: ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        },
        event_id: None,
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let user = ChatMessage::text("u", ChatRole::User, "restart");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(
        *log.borrow(),
        vec![
            "approval:exec_command:c1:{}".to_string(),
            "finished:c1:true:exit_code=0".to_string(),
            "answer:ok".to_string(),
        ]
    );
}

/// A repeatedly truncated turn is discarded and stops after one bounded retry.
#[tokio::test]
async fn streams_discarded_on_truncated_turn() {
    let sess = MemSession::default();
    let truncated = || ModelTurn {
        text: "half".into(),
        stop_reason: StopReason::MaxTokens,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::MaxTokens),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([truncated(), truncated()].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Truncated);
    assert_eq!(
        *log.borrow(),
        vec!["discarded".to_string(), "discarded".to_string()]
    );
}

#[tokio::test]
async fn truncated_turn_retries_once_with_server_recovery_notice() {
    let sess = MemSession::default();
    let truncated = ModelTurn {
        text: "partial planning that must not be committed".into(),
        stop_reason: StopReason::MaxTokens,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::MaxTokens),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([truncated, answer("recovered concisely")].into()),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "q"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("recovered concisely".into()));
    assert_eq!(requests.borrow().len(), 2);
    assert!(
        requests.borrow()[1]
            .messages
            .iter()
            .any(|message| message.text.contains("output-token limit"))
    );
    assert!(
        requests.borrow()[1]
            .messages
            .iter()
            .any(|message| message.text.contains("minimum valid exposed tool call"))
    );
    assert_eq!(
        *log.borrow(),
        vec![
            "discarded".to_string(),
            "answer:recovered concisely".to_string()
        ]
    );
}

#[tokio::test]
async fn reasoning_only_max_tokens_reports_runtime_budget_configuration_error() {
    let sess = MemSession::default();
    let truncated = ModelTurn {
        stop_reason: StopReason::MaxTokens,
        provider_meta: ProviderResponseMeta {
            reasoning_observed: true,
            reasoning_tokens: Some(8192),
            ..ProviderResponseMeta::without_reasoning(StopReason::MaxTokens)
        },
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([truncated].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let error = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "q"),
        &mut sink,
    )
    .await
    .unwrap_err();

    assert_eq!(error.kind, AgentErrorKind::OutputLimitExceeded);
    assert!(!error.retryable);
    assert!(error.message.contains("runtime_max_output_tokens"));
    assert_eq!(*log.borrow(), vec!["discarded".to_string()]);
}

#[tokio::test]
async fn reasoning_only_end_turn_retries_once_with_server_recovery_notice() {
    let sess = MemSession::default();
    let reasoning_only = ModelTurn {
        stop_reason: StopReason::EndTurn,
        provider_meta: ProviderResponseMeta {
            reasoning_observed: true,
            reasoning_tokens: Some(128),
            ..ProviderResponseMeta::without_reasoning(StopReason::EndTurn)
        },
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([reasoning_only, answer("recovered")].into()),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "q"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("recovered".into()));
    assert_eq!(requests.borrow().len(), 2);
    assert!(
        requests.borrow()[1]
            .messages
            .iter()
            .any(|message| message.text.contains("RUNTIME RECOVERY NOTICE"))
    );
    assert_eq!(
        *log.borrow(),
        vec!["discarded".to_string(), "answer:recovered".to_string()]
    );
}

#[tokio::test]
async fn repeated_reasoning_only_end_turn_fails_after_one_bounded_retry() {
    let sess = MemSession::default();
    let reasoning_only = || ModelTurn {
        stop_reason: StopReason::EndTurn,
        provider_meta: ProviderResponseMeta {
            reasoning_observed: true,
            reasoning_tokens: Some(128),
            ..ProviderResponseMeta::without_reasoning(StopReason::EndTurn)
        },
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([reasoning_only(), reasoning_only()].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let error = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "q"),
        &mut sink,
    )
    .await
    .unwrap_err();

    assert_eq!(error.kind, AgentErrorKind::Internal);
    assert!(!error.retryable);
    assert!(error.message.contains("bounded automatic recovery"));
    assert_eq!(
        *log.borrow(),
        vec!["discarded".to_string(), "discarded".to_string()]
    );
}

#[tokio::test]
async fn empty_end_turn_without_reasoning_also_uses_bounded_recovery() {
    let sess = MemSession::default();
    let empty = ModelTurn {
        stop_reason: StopReason::EndTurn,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::EndTurn),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([empty, answer("recovered")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "q"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("recovered".into()));
}

#[tokio::test]
async fn context_window_stop_is_not_reported_as_output_truncation() {
    let sess = MemSession::default();
    let stopped = ModelTurn {
        stop_reason: StopReason::ContextWindowExceeded,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::ContextWindowExceeded),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([stopped].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u", ChatRole::User, "q"),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::ContextWindowExceeded);
    assert_eq!(*log.borrow(), vec!["discarded".to_string()]);
}

// ---------------------------- Content safety ----------------------------

#[derive(Default)]
struct SafetyScript {
    model_turn_results: RefCell<
        std::collections::VecDeque<
            Result<crate::content_safety::SafetyVerdict, desk_agent_protocol::AgentError>,
        >,
    >,
    image_results: RefCell<
        std::collections::VecDeque<
            Result<crate::content_safety::SafetyVerdict, desk_agent_protocol::AgentError>,
        >,
    >,
    model_turn_requests: Rc<RefCell<Vec<crate::content_safety::SafetyModelTurn>>>,
    image_requests: Rc<RefCell<Vec<crate::content_safety::SafetyImage>>>,
}

#[async_trait(?Send)]
impl crate::content_safety::ContentSafetySeam for SafetyScript {
    async fn check_input(
        &self,
        _request: crate::content_safety::SafetyInput,
    ) -> Result<crate::content_safety::SafetyVerdict, desk_agent_protocol::AgentError> {
        panic!("the shared loop never performs the manager input-stage check")
    }

    async fn check_model_turn(
        &self,
        request: crate::content_safety::SafetyModelTurn,
    ) -> Result<crate::content_safety::SafetyVerdict, desk_agent_protocol::AgentError> {
        self.model_turn_requests.borrow_mut().push(request);
        self.model_turn_results
            .borrow_mut()
            .pop_front()
            .expect("a scripted model-turn safety result")
    }

    async fn check_image(
        &self,
        request: crate::content_safety::SafetyImage,
    ) -> Result<crate::content_safety::SafetyVerdict, desk_agent_protocol::AgentError> {
        self.image_requests.borrow_mut().push(request);
        self.image_results
            .borrow_mut()
            .pop_front()
            .expect("a scripted image safety result")
    }
}

fn safety_verdict(
    decision: desk_agent_protocol::content_safety::ContentSafetyDecision,
    stage: desk_agent_protocol::content_safety::ContentSafetyStage,
) -> crate::content_safety::SafetyVerdict {
    use desk_agent_protocol::content_safety::ContentSafetyDecision;
    if decision == ContentSafetyDecision::Allow {
        return crate::content_safety::SafetyVerdict {
            decision,
            categories: Vec::new(),
            stages: Vec::new(),
            policy_version: crate::content_safety::CONTENT_SAFETY_PROMPT_VERSION.into(),
        };
    }
    crate::content_safety::SafetyVerdict {
        decision,
        categories: vec![
            desk_agent_protocol::content_safety::ContentSafetyCategory::ViolentWrongdoing,
        ],
        stages: vec![stage],
        policy_version: crate::content_safety::CONTENT_SAFETY_PROMPT_VERSION.into(),
    }
}

fn safety_context() -> crate::content_safety::SafetyContext {
    crate::content_safety::SafetyContext {
        surface: desk_agent_protocol::content_safety::ContentSafetySurface::AssistantAnswer,
        original_allowed_intent: "diagnose the device".into(),
        policy_revision: 7,
        safety_model_id: "safety-model".into(),
        safety_provider_identity_sha256: "a".repeat(64),
        safety_model_identity_sha256: "b".repeat(64),
        safety_connection_revision: 3,
        safety_model_profile_revision: 4,
        safety_prompt_version: crate::content_safety::CONTENT_SAFETY_PROMPT_VERSION.into(),
    }
}

struct SafetyEventLog(Rc<RefCell<Vec<String>>>);

impl TurnSink for SafetyEventLog {
    fn on_text_delta(&mut self, delta: &str) {
        self.0.borrow_mut().push(format!("partial:{delta}"));
    }

    fn on_partial_committed(&mut self) {
        self.0.borrow_mut().push("committed".into());
    }

    fn on_turn_retracted(
        &mut self,
        reason: desk_agent_protocol::content_safety::StreamRetractionReason,
        error: Option<desk_agent_protocol::AgentError>,
    ) {
        let code = error.and_then(|value| value.error_code);
        self.0
            .borrow_mut()
            .push(format!("retracted:{reason:?}:{code:?}"));
    }

    fn on_tool_started(&mut self, tool_name: &str, call_id: &str, _arguments_json: &str) {
        self.0
            .borrow_mut()
            .push(format!("started:{tool_name}:{call_id}"));
    }

    fn on_tool_finished(
        &mut self,
        call_id: &str,
        ok: bool,
        _output: &str,
        _background_task_id: Option<&str>,
    ) {
        self.0.borrow_mut().push(format!("finished:{call_id}:{ok}"));
    }

    fn on_answer_committed(&mut self, text: &str) {
        self.0.borrow_mut().push(format!("answer:{text}"));
    }
}

#[tokio::test]
async fn enforced_model_turn_block_reviews_once_and_persists_only_fixed_placeholder() {
    use desk_agent_protocol::content_safety::{
        ContentSafetyDecision, ContentSafetyStage, StreamRetractionReason,
    };
    use desk_utils::error::DeskErrorCode;

    const REJECTED_TEXT: &str = "raw rejected violent instructions";
    const REJECTED_ARG: &str = "raw-secret-action";
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [ModelTurn {
                text: REJECTED_TEXT.into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "sysinfo".into(),
                        arguments_json: format!(r#"{{"payload":"{REJECTED_ARG}"}}"#),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "logs".into(),
                        arguments_json: r#"{"limit":10}"#.into(),
                    },
                ],
                stop_reason: StopReason::ToolUse,
                provider_meta: tool_meta(),
                ..Default::default()
            }]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "must not run".into(),
    };
    let registry = vec![
        read_tool("sysinfo", Capability::SystemInfo),
        read_tool("logs", Capability::LogRecent),
    ];
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [Ok(safety_verdict(
                ContentSafetyDecision::Block,
                ContentSafetyStage::Action,
            ))]
            .into(),
        ),
        ..Default::default()
    };
    let clock = || "t".to_string();
    let mut loop_deps = deps(&sess, &model, &tools, &registry, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut sink = SafetyEventLog(events.clone());
    let outcome = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("u", ChatRole::User, "diagnose the device"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::ContentRejected(ContentSafetyDecision::Block)
    );
    assert!(tools.calls.borrow().is_empty());
    let reviews = safety.model_turn_requests.borrow();
    assert_eq!(reviews.len(), 1, "one complete ModelTurn gets one review");
    assert_eq!(reviews[0].text, REJECTED_TEXT);
    assert_eq!(reviews[0].tool_calls.len(), 2);
    assert_eq!(
        reviews[0].tool_calls[0].canonical_arguments_json,
        format!(r#"{{"payload":"{REJECTED_ARG}"}}"#)
    );
    drop(reviews);

    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.turn_state, TurnState::Idle);
    assert_eq!(stored.conversation.len(), 2);
    assert_eq!(stored.conversation[1].role, ChatRole::Assistant);
    assert!(stored.conversation[1].tool_calls.is_empty());
    let serialized = serde_json::to_string(&stored.conversation).unwrap();
    assert!(!serialized.contains(REJECTED_TEXT));
    assert!(!serialized.contains(REJECTED_ARG));
    assert!(
        stored.conversation[1]
            .text
            .contains("content safety policy")
    );
    assert_eq!(
        *events.borrow(),
        vec![
            format!("partial:{REJECTED_TEXT}"),
            format!(
                "retracted:{:?}:{:?}",
                StreamRetractionReason::PolicyBlocked,
                Some(DeskErrorCode::AI_CONTENT_BLOCKED.code())
            ),
        ]
    );
}

#[tokio::test]
async fn enforced_terminal_safety_unavailable_stays_terminal_and_hides_provider_detail() {
    use desk_agent_protocol::content_safety::StreamRetractionReason;
    use desk_agent_protocol::{AgentError, AgentErrorKind};
    use desk_utils::error::DeskErrorCode;

    const RAW_TEXT: &str = "unreviewed provider output";
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer(RAW_TEXT)].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: String::new(),
    };
    let registry = Vec::new();
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [Err(AgentError {
                kind: AgentErrorKind::TransportError,
                message: "provider secret failure detail".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            })]
            .into(),
        ),
        ..Default::default()
    };
    let clock = || "t".to_string();
    let mut loop_deps = deps(&sess, &model, &tools, &registry, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut sink = SafetyEventLog(events.clone());
    let outcome = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("u", ChatRole::User, "diagnose the device"),
        &mut sink,
    )
    .await
    .unwrap();

    let LoopOutcome::ContentSafetyUnavailable(error) = outcome else {
        panic!("expected the distinct unavailable outcome");
    };
    assert_eq!(error.kind, AgentErrorKind::ContentSafetyUnavailable);
    assert!(!error.retryable);
    assert_eq!(
        error.error_code,
        Some(DeskErrorCode::AI_CONTENT_SAFETY_UNAVAILABLE.code())
    );
    assert!(!error.message.contains("provider secret"));
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.turn_state, TurnState::Failed);
    assert_eq!(stored.conversation.len(), 1);
    assert_eq!(stored.conversation[0].role, ChatRole::User);
    assert!(
        !serde_json::to_string(&stored.conversation)
            .unwrap()
            .contains(RAW_TEXT)
    );
    assert_eq!(
        *events.borrow(),
        vec![
            format!("partial:{RAW_TEXT}"),
            format!(
                "retracted:{:?}:{:?}",
                StreamRetractionReason::SafetyUnavailable,
                Some(DeskErrorCode::AI_CONTENT_SAFETY_UNAVAILABLE.code())
            ),
        ]
    );
}

#[tokio::test]
async fn enforced_allow_persists_and_commits_before_tool_dispatch() {
    use desk_agent_protocol::content_safety::{ContentSafetyDecision, ContentSafetyStage};

    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                ModelTurn {
                    text: "I will inspect the system.".into(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "sysinfo".into(),
                        arguments_json: "{}".into(),
                    }],
                    stop_reason: StopReason::ToolUse,
                    provider_meta: tool_meta(),
                    ..Default::default()
                },
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "ok".into(),
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [
                Ok(safety_verdict(
                    ContentSafetyDecision::Allow,
                    ContentSafetyStage::Action,
                )),
                Ok(safety_verdict(
                    ContentSafetyDecision::Allow,
                    ContentSafetyStage::Output,
                )),
            ]
            .into(),
        ),
        ..Default::default()
    };
    let clock = || "t".to_string();
    let mut loop_deps = deps(&sess, &model, &tools, &registry, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut sink = SafetyEventLog(events.clone());
    let outcome = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("u", ChatRole::User, "diagnose the device"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(safety.model_turn_requests.borrow().len(), 2);
    assert_eq!(*tools.calls.borrow(), vec!["sysinfo"]);
    assert_eq!(
        *events.borrow(),
        vec![
            "partial:I will inspect the system.".to_string(),
            "committed".to_string(),
            "started:sysinfo:c1".to_string(),
            "finished:c1:true".to_string(),
            "partial:done".to_string(),
            "answer:done".to_string(),
        ]
    );
}

struct ImageTools {
    calls: Rc<RefCell<Vec<String>>>,
    image_data_url: String,
    raw_content: String,
}

#[async_trait(?Send)]
impl ToolSeam for ImageTools {
    async fn run_read(
        &self,
        call: &ToolCall,
    ) -> Result<ToolRunOutput, desk_agent_protocol::AgentError> {
        self.calls.borrow_mut().push(call.id.clone());
        Ok(ToolRunOutput {
            content: self.raw_content.clone(),
            image_data_url: Some(self.image_data_url.clone()),
        })
    }
}

#[tokio::test]
async fn rejected_tool_image_is_not_persisted_and_remaining_calls_are_paired() {
    use desk_agent_protocol::content_safety::{ContentSafetyDecision, ContentSafetyStage};

    const RAW_TOOL_CONTENT: &str = "raw tool content next to rejected image";
    let image_data_url = "data:image/jpeg;base64,AQID".to_string();
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [ModelTurn {
                text: "I will inspect screenshots.".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "first".into(),
                        arguments_json: "{}".into(),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "second".into(),
                        arguments_json: "{}".into(),
                    },
                ],
                stop_reason: StopReason::ToolUse,
                provider_meta: tool_meta(),
                ..Default::default()
            }]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let image_tools = ImageTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        image_data_url: image_data_url.clone(),
        raw_content: RAW_TOOL_CONTENT.into(),
    };
    let registry = vec![
        read_tool("first", Capability::SystemInfo),
        read_tool("second", Capability::LogRecent),
    ];
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [Ok(safety_verdict(
                ContentSafetyDecision::Allow,
                ContentSafetyStage::Action,
            ))]
            .into(),
        ),
        image_results: RefCell::new(
            [Ok(safety_verdict(
                ContentSafetyDecision::Block,
                ContentSafetyStage::Image,
            ))]
            .into(),
        ),
        ..Default::default()
    };
    let clock = || "t".to_string();
    let loop_deps = LoopDeps {
        session_seam: &sess,
        model: &model,
        tools: &image_tools,
        content_safety: crate::content_safety::ContentSafetyMode::Enforced {
            seam: &safety,
            context: safety_context(),
        },
        registry: &registry,
        provider_registry: None,
        capability_inventory: None,
        permission_continuation_exact_tools: &[],
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock: &clock,
        heartbeat: None,
    };
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let outcome = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("u", ChatRole::User, "diagnose the device"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::ContentRejected(ContentSafetyDecision::Block)
    );
    assert_eq!(*image_tools.calls.borrow(), vec!["c1"]);
    assert_eq!(safety.image_requests.borrow().len(), 1);
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.turn_state, TurnState::Idle);
    let tool_results: Vec<_> = stored
        .conversation
        .iter()
        .filter(|message| message.role == ChatRole::Tool)
        .collect();
    assert_eq!(tool_results.len(), 2);
    assert_eq!(tool_results[0].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(tool_results[1].tool_call_id.as_deref(), Some("c2"));
    assert!(
        stored
            .conversation
            .iter()
            .all(|message| message.image_data_url.is_none())
    );
    let serialized = serde_json::to_string(&stored.conversation).unwrap();
    assert!(!serialized.contains(&image_data_url));
    assert!(!serialized.contains(RAW_TOOL_CONTENT));
}

#[tokio::test]
async fn rejected_mutating_result_image_is_not_persisted_and_delivery_is_acked() {
    use desk_agent_protocol::content_safety::{ContentSafetyDecision, ContentSafetyStage};

    const RAW_TOOL_CONTENT: &str = "mutating result next to rejected image";
    let image_data_url = "data:image/jpeg;base64,AQID".to_string();
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        data_envelope: None,
        output: ToolRunOutput {
            content: RAW_TOOL_CONTENT.into(),
            image_data_url: Some(image_data_url.clone()),
        },
        event_id: Some("work:8:done".into()),
    }]);
    let registry = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [Ok(safety_verdict(
                ContentSafetyDecision::Allow,
                ContentSafetyStage::Action,
            ))]
            .into(),
        ),
        image_results: RefCell::new(
            [Ok(safety_verdict(
                ContentSafetyDecision::Block,
                ContentSafetyStage::Image,
            ))]
            .into(),
        ),
        ..Default::default()
    };
    let clock = || "t".to_string();
    let mut loop_deps = exec_deps(&sess, &model, &scripted, &registry, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut sink = SafetyEventLog(events.clone());
    let outcome = run_agent_turn(
        &loop_deps,
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "restart it"),
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::ContentRejected(ContentSafetyDecision::Block)
    );
    assert_eq!(*scripted.exec_calls.borrow(), vec!["c1"]);
    assert_eq!(*scripted.acks.borrow(), vec!["work:8:done"]);
    assert_eq!(safety.image_requests.borrow().len(), 1);
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.turn_state, TurnState::Idle);
    let tool_result = stored
        .conversation
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert!(
        tool_result
            .text
            .contains("omitted by content safety policy")
    );
    assert!(
        stored
            .conversation
            .iter()
            .all(|message| message.image_data_url.is_none())
    );
    let serialized = serde_json::to_string(&stored.conversation).unwrap();
    assert!(!serialized.contains(&image_data_url));
    assert!(!serialized.contains(RAW_TOOL_CONTENT));
    assert!(
        events
            .borrow()
            .iter()
            .any(|event| event.starts_with("retracted:PolicyBlocked:"))
    );
}

#[tokio::test]
async fn unavailable_wait_result_image_fails_closed_clears_task_and_acks() {
    use desk_agent_protocol::content_safety::{ContentSafetyDecision, ContentSafetyStage};
    use desk_agent_protocol::{AgentError, AgentErrorKind};

    const RAW_TOOL_CONTENT: &str = "wait result next to unreviewed image";
    let image_data_url = "data:image/jpeg;base64,AQID".to_string();
    let sess = seeded_executing();
    let model = ScriptModel {
        turns: RefCell::new(
            [tool_use_args(
                "c2",
                "wait_for_task",
                r#"{"task_id":"exec_task9"}"#,
            )]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(
        vec![],
        vec![WaitOutcome::Completed {
            output: ToolRunOutput {
                content: RAW_TOOL_CONTENT.into(),
                image_data_url: Some(image_data_url.clone()),
            },
            event_id: Some("work:8:done".into()),
        }],
    );
    let registry = wait_reg();
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [Ok(safety_verdict(
                ContentSafetyDecision::Allow,
                ContentSafetyStage::Action,
            ))]
            .into(),
        ),
        image_results: RefCell::new(
            [Err(AgentError {
                kind: AgentErrorKind::TransportError,
                message: "temporary image classifier outage".into(),
                retryable: true,
                safe_for_model: false,
                error_code: None,
            })]
            .into(),
        ),
        ..Default::default()
    };
    let clock = || "t".to_string();
    let mut loop_deps = exec_deps(&sess, &model, &scripted, &registry, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let outcome = run_agent_turn(
        &loop_deps,
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "wait for it"),
        &mut sink,
    )
    .await
    .unwrap();

    let LoopOutcome::ContentSafetyUnavailable(error) = outcome else {
        panic!("expected content-safety unavailable");
    };
    assert_eq!(error.kind, AgentErrorKind::ContentSafetyUnavailable);
    assert_eq!(*scripted.wait_calls.borrow(), vec!["exec_task9"]);
    assert_eq!(*scripted.acks.borrow(), vec!["work:8:done"]);
    assert_eq!(safety.image_requests.borrow().len(), 1);
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.turn_state, TurnState::Failed);
    assert_eq!(stored.execution_state, ExecutionState::None);
    let tool_result = stored
        .conversation
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert!(tool_result.text.contains("review was unavailable"));
    assert!(
        stored
            .conversation
            .iter()
            .all(|message| message.image_data_url.is_none())
    );
    let serialized = serde_json::to_string(&stored.conversation).unwrap();
    assert!(!serialized.contains(&image_data_url));
    assert!(!serialized.contains(RAW_TOOL_CONTENT));
}

#[tokio::test]
async fn unavailable_retry_resumes_existing_history_without_duplicate_user() {
    use desk_agent_protocol::content_safety::{ContentSafetyDecision, ContentSafetyStage};
    use desk_agent_protocol::{AgentError, AgentErrorKind};

    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("unreviewed"), answer("recovered")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: String::new(),
    };
    let registry = Vec::new();
    let safety = SafetyScript {
        model_turn_results: RefCell::new(
            [
                Err(AgentError {
                    kind: AgentErrorKind::TransportError,
                    message: "temporary classifier outage".into(),
                    retryable: true,
                    safe_for_model: false,
                    error_code: None,
                }),
                Ok(safety_verdict(
                    ContentSafetyDecision::Allow,
                    ContentSafetyStage::Output,
                )),
            ]
            .into(),
        ),
        ..Default::default()
    };
    let clock = || "t".to_string();
    let mut loop_deps = deps(&sess, &model, &tools, &registry, &clock);
    loop_deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
        seam: &safety,
        context: safety_context(),
    };
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let first = run_agent_turn(
        &loop_deps,
        claim(),
        ChatMessage::text("u", ChatRole::User, "diagnose the device"),
        &mut sink,
    )
    .await
    .unwrap();
    assert!(matches!(first, LoopOutcome::ContentSafetyUnavailable(_)));

    let mut retry_claim = claim();
    retry_claim.turn_id = "turn-retry".into();
    retry_claim.request_id = Some("req-retry".into());
    let second = resume_agent_turn(&loop_deps, retry_claim, &mut sink)
        .await
        .unwrap();
    assert_eq!(second, LoopOutcome::Answered("recovered".into()));

    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(
        stored
            .conversation
            .iter()
            .filter(|message| message.role == ChatRole::User)
            .count(),
        1
    );
    assert_eq!(stored.conversation.len(), 2);
    assert_eq!(stored.conversation[1].text, "recovered");
}

#[test]
fn artifact_consumer_lineage_resolves_a_prior_typed_artifact_result() {
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, RetentionBoundary, Sensitivity,
    };

    let mut session = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    let source = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "artifact-envelope-1".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "artifact-metadata-1".into(),
            sha256: "c".repeat(64),
            size_bytes: 10,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "file.provider".into(),
            source_tool_name: "create_file".into(),
            source_object_id: None,
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: "a".repeat(64),
        sensitivity: Sensitivity::Sensitive,
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        },
    };
    let source_text = serde_json::json!({
        "work_id": "work-1",
        "action_request_id": "request-1",
        "execution_generation": "generation-1",
        "result": "verified",
        "facts": [],
        "message": "created",
        "output": {
            "kind": "file_artifact",
            "value": {
                "file": {
                    "token": "artifact-token-1",
                    "snapshot_id": "snapshot-1",
                    "object_kind": "file",
                    "expires_at": "2026-08-29T07:00:00Z"
                },
                "file_name": "report.docx",
                "media_type": "application/test",
                "size_bytes": 7,
                "digest_sha256": "a".repeat(64),
                "content": {
                    "kind": "artifact",
                    "artifact_id": "artifact-token-1",
                    "sha256": "a".repeat(64),
                    "size_bytes": 7,
                    "media_type": "application/test"
                }
            }
        }
    })
    .to_string();
    let mut source_message = ChatMessage::tool_result("result-1", "create-1", source_text);
    source_message.data_envelope = Some(source);
    session.conversation.push(source_message);

    let call = ToolCall {
        id: "gmail-1".into(),
        name: "prepare_gmail_web_draft_handoff".into(),
        arguments_json: serde_json::json!({
            "attachment": {"artifact": {"content": {
                "kind": "artifact",
                "artifact_id": "artifact-token-1",
                "sha256": "a".repeat(64),
                "size_bytes": 7,
                "media_type": "application/test"
            }}}
        })
        .to_string(),
    };
    let mut model_turn = ChatMessage::text("model-1", ChatRole::Assistant, "");
    model_turn.tool_calls.push(call.to_ref());
    model_turn.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "model-envelope-1".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "model-output-1".into(),
            sha256: "d".repeat(64),
            size_bytes: 1,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "model.provider".into(),
            source_tool_name: "model_turn".into(),
            source_object_id: None,
            source_envelope_ids: vec!["artifact-envelope-1".into()],
        },
        digest_sha256: "d".repeat(64),
        sensitivity: Sensitivity::Sensitive,
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    });
    session.conversation.push(model_turn);
    let mut result = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "gmail-envelope-1".into(),
        content: ContentRef::EphemeralObservation {
            observation_id: "gmail-result-1".into(),
            size_bytes: 1,
            expires_at_unix_ms: 2,
        },
        provenance: DataProvenance {
            source_provider_id: "gmail.provider".into(),
            source_tool_name: call.name.clone(),
            source_object_id: None,
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: "b".repeat(64),
        sensitivity: Sensitivity::Sensitive,
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(2),
            delete_with_run: true,
        },
    });
    bind_tool_input_envelopes(&session, &call, &mut result).unwrap();
    assert_eq!(
        result.unwrap().provenance.source_envelope_ids,
        vec!["artifact-envelope-1", "model-envelope-1"]
    );
}

#[test]
fn artifact_producer_lineage_resolves_preview_and_current_requirement() {
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, RetentionBoundary, Sensitivity,
    };

    fn envelope(id: &str, provider: &str, tool: &str, digest: char) -> DataEnvelope {
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("{id}-blob"),
                sha256: digest.to_string().repeat(64),
                size_bytes: 7,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: provider.into(),
                source_tool_name: tool.into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest.to_string().repeat(64),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        }
    }

    let mut session = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    let mut user = ChatMessage::text("user-1", ChatRole::User, "create the report");
    user.data_envelope = Some(envelope("user-envelope-1", "user", "message", 'a'));
    session.conversation.push(user);
    let mut preview = ChatMessage::tool_result(
        "preview-result-1",
        "preview-call-1",
        serde_json::json!({
            "ReadContext": {
                "SpreadsheetMergePreview": {"preview_id": "preview-1"}
            }
        })
        .to_string(),
    );
    preview.data_envelope = Some(envelope(
        "preview-envelope-1",
        "spreadsheet.merge",
        "preview_spreadsheet_merge",
        'b',
    ));
    session.conversation.push(preview);
    session.conversation.push(ChatMessage::assistant_tool_calls(
        "search-turn-1",
        "",
        vec![ToolCallRef {
            id: "search-call-1".into(),
            name: "search_public_web".into(),
            arguments_json: r#"{"query":"public benchmark"}"#.into(),
        }],
    ));
    let mut search = ChatMessage::tool_result(
        "search-result-1",
        "search-call-1",
        serde_json::json!({
            "schema_version": 1,
            "results": [{
                "title": "Public benchmark",
                "url": "https://example.com/benchmark",
                "snippet": "untrusted"
            }]
        })
        .to_string(),
    );
    search.data_envelope = Some(envelope(
        "search-envelope-1",
        "web.search",
        "search_public_web",
        'd',
    ));
    session.conversation.push(search);

    let call = ToolCall {
        id: "docx-1".into(),
        name: "create_word_report_from_merge_preview".into(),
        arguments_json: serde_json::json!({
            "preview_id": "preview-1",
            "file_name": "report.docx",
            "title": "Report",
            "web_search_call_id": "search-call-1",
            "web_sources": [{
                "title": "Public benchmark",
                "url": "https://example.com/benchmark"
            }]
        })
        .to_string(),
    };
    assert!(
        resolve_word_report_web_source_envelope(&session, &call)
            .unwrap()
            .is_some()
    );
    let mut result = Some(envelope(
        "docx-envelope-1",
        "word.document",
        "create_word_report_from_merge_preview",
        'c',
    ));
    bind_tool_input_envelopes(&session, &call, &mut result).unwrap();

    assert_eq!(
        result.unwrap().provenance.source_envelope_ids,
        vec!["preview-envelope-1", "search-envelope-1", "user-envelope-1"]
    );

    let fabricated = ToolCall {
        arguments_json: serde_json::json!({
            "preview_id": "preview-1",
            "file_name": "report.docx",
            "title": "Report",
            "web_search_call_id": "search-call-1",
            "web_sources": [{
                "title": "Invented source",
                "url": "https://example.com/invented"
            }]
        })
        .to_string(),
        ..call
    };
    assert!(resolve_word_report_web_source_envelope(&session, &fabricated).is_err());
}

#[test]
fn requested_artifact_projection_restores_only_verbatim_named_typed_artifact() {
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DestinationIdentity, RetentionBoundary, Sensitivity,
    };

    fn envelope(id: &str, tool: &str, sensitivity: Sensitivity) -> DataEnvelope {
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("{id}-blob"),
                sha256: "a".repeat(64),
                size_bytes: 7,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "test".into(),
                source_tool_name: tool.into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: "a".repeat(64),
            sensitivity,
            allowed_destinations: vec![DestinationIdentity::Model {
                connection_id: "gateway:1".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            }],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        }
    }

    let digest = "b".repeat(64);
    let artifact_json = serde_json::json!({
        "work_id": "work-1",
        "action_request_id": "request-1",
        "execution_generation": "generation-1",
        "result": "verified",
        "facts": [],
        "message": "created",
        "output": {
            "kind": "file_artifact",
            "value": {
                "file": {
                    "token": "artifact-token-1",
                    "snapshot_id": "volume:identity",
                    "object_kind": "file",
                    "expires_at": "2030-01-01T00:00:00Z"
                },
                "file_name": "stage5_report.docx",
                "media_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "size_bytes": 7,
                "digest_sha256": digest,
                "content": {
                    "kind": "artifact",
                    "artifact_id": "artifact-token-1",
                    "sha256": "b".repeat(64),
                    "size_bytes": 7,
                    "media_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                }
            }
        }
    });
    let mut artifact = ChatMessage::tool_result("artifact", "call", artifact_json.to_string());
    artifact.data_envelope = Some(envelope(
        "artifact-envelope",
        "create_word_report_from_merge_preview",
        Sensitivity::Sensitive,
    ));
    let mut user = ChatMessage::text(
        "user",
        ChatRole::User,
        "Attach stage5_report.docx to the manual-only draft",
    );
    user.data_envelope = Some(envelope(
        "user-envelope",
        "send-message",
        Sensitivity::UserContent,
    ));

    let projection =
        requested_artifact_registry_projection(&[artifact.clone(), user.clone()], "projection")
            .unwrap()
            .unwrap();
    assert!(projection.text.contains("artifact-token-1"));
    assert!(projection.text.contains("volume:identity"));
    assert!(!projection.text.contains("C:\\"));
    let projected_envelope = projection.data_envelope.unwrap();
    assert_eq!(projected_envelope.sensitivity, Sensitivity::Sensitive);
    assert!(
        projected_envelope
            .provenance
            .source_envelope_ids
            .contains(&"user-envelope".to_string())
    );
    assert!(
        projected_envelope
            .provenance
            .source_envelope_ids
            .contains(&"artifact-envelope".to_string())
    );

    user.text = "Prepare an unrelated manual-only draft".into();
    assert!(
        requested_artifact_registry_projection(&[artifact, user], "projection-2")
            .unwrap()
            .is_none()
    );
}

#[test]
fn permission_resume_projection_restores_only_bounded_reusable_references() {
    use desk_agent_protocol::browser_control::{
        BROWSER_CONTROL_SCHEMA_VERSION, BrowserActionOutcome, BrowserActionResult,
        BrowserAdapterRef, BrowserElementRef, BrowserElementRole, BrowserEngineKind, BrowserOrigin,
        BrowserOriginKind, BrowserPageRef, BrowserSemanticSnapshot,
    };
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, DestinationIdentity, RetentionBoundary,
        Sensitivity,
    };

    fn envelope(id: &str, tool: &str, sensitivity: Sensitivity) -> DataEnvelope {
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("{id}-blob"),
                sha256: "a".repeat(64),
                size_bytes: 10,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "test".into(),
                source_tool_name: tool.into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: "a".repeat(64),
            sensitivity,
            allowed_destinations: vec![DestinationIdentity::Model {
                connection_id: "gateway:1".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            }],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        }
    }

    let mut preview = ChatMessage::tool_result(
        "preview-result",
        "preview-call",
        serde_json::json!({
            "ReadContext": {"SpreadsheetMergePreview": {
                "preview_id": "spreadsheet-merge-preview-1",
                "rows": [["must", "not", "be", "replayed"]]
            }}
        })
        .to_string(),
    );
    preview.data_envelope = Some(envelope(
        "preview-envelope",
        "preview_spreadsheet_merge",
        Sensitivity::Sensitive,
    ));
    let search_call = ChatMessage::assistant_tool_calls(
        "search-turn",
        "",
        vec![ToolCallRef {
            id: "search-call-1".into(),
            name: "search_public_web".into(),
            arguments_json: r#"{"query":"public benchmark"}"#.into(),
        }],
    );
    let mut search = ChatMessage::tool_result(
        "search-result",
        "search-call-1",
        serde_json::json!({
            "schema_version": 1,
            "web_search_call_id": "search-call-1",
            "results": [{
                "title": "Public benchmark",
                "url": "https://example.com/benchmark",
                "snippet": "must not be replayed"
            }]
        })
        .to_string(),
    );
    search.data_envelope = Some(envelope(
        "search-envelope",
        "search_public_web",
        Sensitivity::Sensitive,
    ));
    let page = BrowserPageRef {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        adapter: BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeExtension,
            device_id: "device-1".into(),
            os_session_id: "session-1".into(),
            browser_major_version: 144,
            browser_version: "144.0.0.0".into(),
            adapter_id: "chrome-extension".into(),
            adapter_version: "1.0.0".into(),
            profile_incarnation: "profile-incarnation-1".into(),
            connection_revision: 7,
        },
        page_id: "page-slack-1".into(),
        page_incarnation: "page-incarnation-1".into(),
        origin: BrowserOrigin {
            kind: BrowserOriginKind::Https,
            host_ascii: "app.slack.com".into(),
            port: 443,
        },
        document_revision: 4,
        url_sha256: "b".repeat(64),
        observed_at_unix_ms: 42,
    };
    let browser_result = BrowserActionResult {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        call_id: "browser-call-1".into(),
        outcome: BrowserActionOutcome::PageOpened,
        page: page.clone(),
        snapshot: None,
        form_readback: Vec::new(),
        completed_at_unix_ms: 43,
    };
    let browser_completion = serde_json::json!({
        "work_id": "15",
        "action_request_id": "browser-call-1",
        "execution_generation": "generation-browser-call-1",
        "result": "verified",
        "facts": [{
            "index": 0,
            "changed": true,
            "verified": true,
            "summary": "browser action completed with bounded semantic read-back"
        }],
        "message": "browser adapter returned a typed, page-bound result",
        "output": {"kind": "browser", "value": browser_result}
    })
    .to_string();
    let mut browser =
        ChatMessage::tool_result("browser-result", "browser-call-1", browser_completion);
    let mut browser_result_envelope = envelope(
        "browser-envelope",
        "browser_open_page",
        Sensitivity::Sensitive,
    );
    browser_result_envelope.retention.expires_at_unix_ms = Some(9_999);
    browser.data_envelope = Some(browser_result_envelope);
    let gmail_page = BrowserPageRef {
        page_id: "page-gmail-1".into(),
        page_incarnation: "gmail-incarnation-1".into(),
        origin: BrowserOrigin {
            kind: BrowserOriginKind::Https,
            host_ascii: "mail.google.com".into(),
            port: 443,
        },
        document_revision: 5,
        url_sha256: "c".repeat(64),
        observed_at_unix_ms: 44,
        ..page
    };
    let gmail_elements = [
        ("gmail-to", BrowserElementRole::Combobox, "To recipients"),
        ("gmail-subject", BrowserElementRole::Textbox, "Subject"),
        ("gmail-body", BrowserElementRole::Textbox, "Message Body"),
    ]
    .into_iter()
    .map(|(element_id, role, accessible_name)| BrowserElementRef {
        page_id: gmail_page.page_id.clone(),
        page_incarnation: gmail_page.page_incarnation.clone(),
        document_revision: gmail_page.document_revision,
        element_id: element_id.into(),
        role,
        accessible_name: accessible_name.into(),
        value: None,
        element_revision: 1,
    })
    .collect::<Vec<_>>();
    let gmail_result = BrowserActionResult {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        call_id: "browser-call-gmail".into(),
        outcome: BrowserActionOutcome::SnapshotCaptured,
        page: gmail_page.clone(),
        snapshot: Some(BrowserSemanticSnapshot {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            page: gmail_page,
            elements: gmail_elements,
            truncated: false,
            captured_at_unix_ms: 45,
        }),
        form_readback: Vec::new(),
        completed_at_unix_ms: 46,
    };
    let mut gmail = ChatMessage::tool_result(
        "gmail-browser-result",
        "browser-call-gmail",
        serde_json::to_string(&gmail_result).unwrap(),
    );
    let mut gmail_envelope = envelope(
        "gmail-browser-envelope",
        "browser_take_snapshot",
        Sensitivity::Sensitive,
    );
    gmail_envelope.retention.expires_at_unix_ms = Some(9_999);
    gmail.data_envelope = Some(gmail_envelope);
    let mut resume = ChatMessage::text(
        "resume",
        ChatRole::User,
        "permission decision for the existing report requirement",
    );
    resume.data_envelope = Some(envelope(
        "resume-envelope",
        "permission-decision-resume",
        Sensitivity::UserContent,
    ));

    let conversation = vec![preview, search_call, search, browser, gmail, resume];
    let projection = reusable_provider_result_projection(&conversation, "projection", 9_000)
        .unwrap()
        .unwrap();
    assert!(projection.text.contains("spreadsheet-merge-preview-1"));
    assert!(projection.text.contains("search-call-1"));
    assert!(projection.text.contains("Public benchmark"));
    assert!(projection.text.contains("https://example.com/benchmark"));
    assert!(projection.text.contains("page-slack-1"));
    assert!(projection.text.contains("page-incarnation-1"));
    assert!(projection.text.contains("app.slack.com"));
    assert!(projection.text.contains("page-gmail-1"));
    assert!(projection.text.contains("To recipients"));
    assert!(projection.text.contains("Message Body"));
    assert!(
        projection
            .text
            .contains("\"page_reference_prerequisite_present\":true")
    );
    assert!(
        projection
            .text
            .contains("supersedes only an older catalog claim")
    );
    assert!(projection.text.contains("browser_take_snapshot"));
    assert!(!projection.text.contains("must not be replayed"));
    assert!(!projection.text.contains("raw page title"));
    let projected = projection.data_envelope.unwrap();
    assert_eq!(projected.sensitivity, Sensitivity::Sensitive);
    assert!(
        projected
            .provenance
            .source_envelope_ids
            .contains(&"preview-envelope".to_string())
    );
    assert!(
        projected
            .provenance
            .source_envelope_ids
            .contains(&"search-envelope".to_string())
    );
    assert!(
        projected
            .provenance
            .source_envelope_ids
            .contains(&"browser-envelope".to_string())
    );
    assert!(
        projected
            .provenance
            .source_envelope_ids
            .contains(&"gmail-browser-envelope".to_string())
    );
    assert_eq!(projected.retention.expires_at_unix_ms, Some(9_999));

    let after_expiry =
        reusable_provider_result_projection(&conversation, "projection-after-expiry", 9_999)
            .unwrap()
            .unwrap();
    assert!(!after_expiry.text.contains("page-slack-1"));
    assert!(after_expiry.text.contains("spreadsheet-merge-preview-1"));
}

#[test]
fn browser_permission_references_must_match_unexpired_verified_edge_evidence() {
    use crate::dynamic_run::{
        GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequest,
        PermissionRequestState,
    };
    use desk_agent_protocol::browser_control::{
        BROWSER_CONTROL_SCHEMA_VERSION, BrowserActionOutcome, BrowserActionResult,
        BrowserAdapterRef, BrowserElementRef, BrowserElementRole, BrowserEngineKind, BrowserOrigin,
        BrowserOriginKind, BrowserPageRef, BrowserSemanticSnapshot,
    };
    use desk_agent_protocol::capability_provider::CapabilityEffect;
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, RetentionBoundary, Sensitivity,
    };

    let page = BrowserPageRef {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        adapter: BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeExtension,
            device_id: "device-1".into(),
            os_session_id: "session-1".into(),
            browser_major_version: 151,
            browser_version: "151.0.0.0".into(),
            adapter_id: "lcxl-browser-extension".into(),
            adapter_version: "0.1.0".into(),
            profile_incarnation: "profile-1".into(),
            connection_revision: 1,
        },
        page_id: "gmail-page-1".into(),
        page_incarnation: "document-1".into(),
        origin: BrowserOrigin {
            kind: BrowserOriginKind::Https,
            host_ascii: "mail.google.com".into(),
            port: 443,
        },
        document_revision: 2,
        url_sha256: "a".repeat(64),
        observed_at_unix_ms: 900,
    };
    let element = BrowserElementRef {
        page_id: page.page_id.clone(),
        page_incarnation: page.page_incarnation.clone(),
        document_revision: page.document_revision,
        element_id: "compose-button".into(),
        role: BrowserElementRole::Button,
        accessible_name: "Compose".into(),
        value: None,
        element_revision: 2,
    };
    let completion = BrowserActionResult {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        call_id: "browser-call".into(),
        outcome: BrowserActionOutcome::PageOpened,
        page: page.clone(),
        snapshot: Some(BrowserSemanticSnapshot {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            page: page.clone(),
            elements: vec![element.clone()],
            truncated: false,
            captured_at_unix_ms: 950,
        }),
        form_readback: Vec::new(),
        completed_at_unix_ms: 975,
    };
    let mut browser_result = ChatMessage::tool_result(
        "browser-result",
        "browser-call",
        serde_json::to_string(&completion).unwrap(),
    );
    browser_result.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "browser-envelope".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "browser-result-blob".into(),
            sha256: "b".repeat(64),
            size_bytes: 10,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "browser.extension".into(),
            source_tool_name: "browser_open_page".into(),
            source_object_id: None,
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: "b".repeat(64),
        sensitivity: Sensitivity::UserContent,
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(5_000),
            delete_with_run: true,
        },
    });

    let request_for = |canonical_input_json: String| PermissionRequest {
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id: "permission-request".into(),
        input_revision: 1,
        state: PermissionRequestState::Pending,
        items: vec![GrantRequestItem {
            item_id: "activate-compose".into(),
            provider_id: "browser.element.activate".into(),
            tool_name: "browser_activate_element".into(),
            expected_effect: CapabilityEffect::InputFallback,
            resource_scope: Vec::new(),
            operation_scope: Vec::new(),
            export_destinations: Vec::new(),
            canonical_input_digest_sha256: None,
            canonical_input_json: Some(canonical_input_json),
            suggested_ttl_seconds: 300,
            suggested_max_uses: 1,
            reason: "Activate Compose".into(),
        }],
        created_at: "1970-01-01T00:00:01Z".into(),
    };
    let exact = serde_json::json!({"page": page, "element": element});
    assert!(
        validate_browser_permission_references(
            std::slice::from_ref(&browser_result),
            &request_for(serde_json::to_string(&exact).unwrap()),
        )
        .is_ok()
    );

    let providers = crate::device_assistant::device_assistant_provider_registry();
    let snapshot_descriptor = providers
        .capability(crate::device_assistant::BROWSER_SNAPSHOT_CAPABILITY_ID)
        .unwrap();
    let snapshot_provider = providers
        .provider_for_capability(crate::device_assistant::BROWSER_SNAPSHOT_CAPABILITY_ID)
        .unwrap();
    let mut snapshot_request =
        request_for(serde_json::json!({"page": page.clone(), "max_elements": 64}).to_string());
    snapshot_request.items[0].item_id = "take-snapshot".into();
    snapshot_request.items[0].provider_id = snapshot_provider.wire.provider_id.clone();
    snapshot_request.items[0].tool_name = "browser_take_snapshot".into();
    snapshot_request.items[0].expected_effect = snapshot_descriptor.wire.effect;
    let snapshot_inventory = [crate::capability_availability::CapabilityAvailability {
        provider_id: snapshot_provider.wire.provider_id.clone(),
        capability_id: snapshot_descriptor.wire.capability_id.clone(),
        tool_name: snapshot_descriptor.tool_spec.name.clone(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    assert!(
        validate_permission_request_availability(
            std::slice::from_ref(&browser_result),
            &snapshot_request,
            &providers,
            &snapshot_inventory,
            &[],
        )
        .is_ok(),
        "a verified fresh page result must satisfy the same-turn snapshot candidate prerequisite"
    );
    assert!(
        validate_permission_request_availability(
            &[],
            &snapshot_request,
            &providers,
            &snapshot_inventory,
            &[],
        )
        .unwrap_err()
        .message
        .contains("must copy its exact page and element references"),
        "the dynamic candidate must remain closed without verified browser evidence"
    );

    let background_completion = serde_json::json!({
        "work_id": "13",
        "action_request_id": "browser-call",
        "execution_generation": "generation-browser-call",
        "result": "verified",
        "facts": [{
            "index": 0,
            "changed": true,
            "verified": true,
            "summary": "browser action completed with bounded semantic read-back"
        }],
        "message": "browser adapter returned a typed, page-bound result",
        "output": {"kind": "browser", "value": completion}
    })
    .to_string();
    let mut background_result = ChatMessage::untrusted_output(
        "browser-background-result",
        "browser-call",
        "browser-call",
        background_completion,
    );
    background_result.data_envelope = browser_result.data_envelope.clone();
    assert!(
        validate_browser_permission_references(
            std::slice::from_ref(&background_result),
            &request_for(serde_json::to_string(&exact).unwrap()),
        )
        .is_ok(),
        "a verified, source-labelled background completion must ground the exact next action"
    );

    let mut rewritten = exact;
    rewritten["page"]["adapter"]["engine"] = serde_json::json!("chrome_devtools_mcp");
    let error = validate_browser_permission_references(
        &[browser_result],
        &request_for(serde_json::to_string(&rewritten).unwrap()),
    )
    .unwrap_err();
    assert!(
        error
            .message
            .contains("must copy its exact page and element references")
    );
}

#[tokio::test]
async fn stage5_fake_same_run_composes_research_artifacts_and_manual_handoffs_with_lineage() {
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, RetentionBoundary, Sensitivity,
    };

    fn artifact_envelope(id: &str, artifact_id: &str, digest: char) -> DataEnvelope {
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::Artifact {
                artifact_id: artifact_id.into(),
                sha256: digest.to_string().repeat(64),
                size_bytes: 7,
                media_type: "application/test".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "file.provider".into(),
                source_tool_name: "create_artifact".into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest.to_string().repeat(64),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        }
    }
    fn handoff_envelope(id: &str, observation_id: &str, digest: char) -> DataEnvelope {
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::EphemeralObservation {
                observation_id: observation_id.into(),
                size_bytes: 1,
                expires_at_unix_ms: 10,
            },
            provenance: DataProvenance {
                source_provider_id: "communication.provider".into(),
                source_tool_name: "manual_handoff".into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest.to_string().repeat(64),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(10),
                delete_with_run: true,
            },
        }
    }
    fn read_tool(name: &str) -> RegisteredTool {
        RegisteredTool {
            spec: ToolSpec {
                name: name.into(),
                description: "read".into(),
                parameters_schema: serde_json::json!({"type":"object"}),
            },
            required_capability: Capability::SystemInfo,
            effect: ToolEffect::ReadOnly,
        }
    }

    let sess = MemSession::default();
    let gmail_args = serde_json::json!({
        "attachment": {"artifact": {"content": {
            "kind": "artifact",
            "artifact_id": "docx-artifact-1",
            "sha256": "b".repeat(64),
            "size_bytes": 7,
            "media_type": "application/test"
        }}}
    })
    .to_string();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("read-1", "inspect_selected_spreadsheets"),
                tool_use("read-2", "preview_spreadsheet_merge"),
                tool_use("read-3", "search_public_web"),
                tool_use("xlsx-1", "create_workbook_from_merge_preview"),
                tool_use("docx-1", "create_word_report_from_merge_preview"),
                tool_use_args("gmail-1", "prepare_gmail_web_draft_handoff", &gmail_args),
                tool_use("slack-1", "prepare_slack_web_message_handoff"),
                answer("combined task complete"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![
        ExecOutcome::Executed {
            data_envelope: None,
            output: ToolRunOutput {
                content: "xlsx created".into(),
                image_data_url: None,
            },
            event_id: None,
        },
        ExecOutcome::Executed {
            data_envelope: None,
            output: ToolRunOutput {
                content: "docx created".into(),
                image_data_url: None,
            },
            event_id: None,
        },
        ExecOutcome::Executed {
            data_envelope: None,
            output: ToolRunOutput {
                content: "gmail handed off".into(),
                image_data_url: None,
            },
            event_id: None,
        },
        ExecOutcome::Executed {
            data_envelope: None,
            output: ToolRunOutput {
                content: "slack handed off".into(),
                image_data_url: None,
            },
            event_id: None,
        },
    ]);
    scripted.mutation_envelopes.borrow_mut().extend([
        Some(artifact_envelope("xlsx-envelope-1", "xlsx-artifact-1", 'a')),
        Some(artifact_envelope("docx-envelope-1", "docx-artifact-1", 'b')),
        Some(handoff_envelope("gmail-envelope-1", "gmail-result-1", 'c')),
        Some(handoff_envelope("slack-envelope-1", "slack-result-1", 'd')),
    ]);
    let registry = vec![
        read_tool("inspect_selected_spreadsheets"),
        read_tool("preview_spreadsheet_merge"),
        read_tool("search_public_web"),
        mutating_tool(
            "create_workbook_from_merge_preview",
            Capability::ShellExecConfirmed,
        ),
        mutating_tool(
            "create_word_report_from_merge_preview",
            Capability::ShellExecConfirmed,
        ),
        mutating_tool(
            "prepare_gmail_web_draft_handoff",
            Capability::ShellExecConfirmed,
        ),
        mutating_tool(
            "prepare_slack_web_message_handoff",
            Capability::ShellExecConfirmed,
        ),
    ];
    let clock = || "t".to_string();
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &registry, &clock),
        exec_claim(),
        ChatMessage::text(
            "u",
            ChatRole::User,
            "merge files, research, create XLSX/DOCX, prepare Gmail and Slack",
        ),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::Answered("combined task complete".into())
    );
    assert_eq!(
        scripted.exec_calls.borrow().as_slice(),
        ["xlsx-1", "docx-1", "gmail-1", "slack-1"]
    );
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    let gmail = stored
        .conversation
        .iter()
        .filter_map(|message| message.data_envelope.as_ref())
        .find(|envelope| envelope.envelope_id == "gmail-envelope-1")
        .unwrap();
    assert_eq!(
        gmail.provenance.source_envelope_ids,
        vec!["docx-envelope-1"]
    );
    assert_eq!(stored.conversation_id, "conv");
}
