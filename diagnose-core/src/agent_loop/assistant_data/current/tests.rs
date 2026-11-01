use super::*;
use crate::ai_assistant::data_access::scope_sha256;
use crate::{
    ai_assistant::{
        data_control::CurrentMemoryUse,
        read_projection::{CurrentMemoryPreparation, ReadProjectionProof},
        seam::{AssistantDataRead, AssistantReadRoot, MemorySeam},
        test_support::personal_session,
    },
    chat::ModelTurn,
    seam::{ClaimError, ClaimTurnParams, ModelSeam, SessionSeam, ToolRunOutput, ToolSeam},
};
use async_trait::async_trait;
use desk_agent_protocol::ai_assistant::{
    memory::{MemoryRecord, MemoryScope, MemorySearchRequest, MemorySearchResult, MemoryStatus},
    source::{AssistantSourceResolution, HistoryDependency},
    user_instructions::AssistantDataSettings,
};
use desk_agent_protocol::{
    ai_assistant::source::SourceReadPurpose, data_lineage::DestinationIdentity,
};
use std::cell::{Cell, RefCell};

#[derive(Clone, Copy)]
enum Mode {
    Valid,
    ChangedGeneration,
}
struct Port {
    mode: Mode,
    calls: RefCell<Vec<(String, usize, usize)>>,
    large_batch: bool,
}
impl Port {
    fn block(
        &self,
        session: &PersistedAgentSession,
        destination: &DestinationIdentity,
        preparation: &str,
        maximum: usize,
        now: u64,
    ) -> CurrentMemoryBlock {
        let data = session.assistant_data.as_ref().unwrap();
        let memory = native_ids::stable(Kind::Memory, &"loop-current-fact");
        let source = native_ids::stable(Kind::AssistantSource, &"loop-current-evidence");
        let mut record = MemoryRecord {
            memory_id: memory.clone(),
            revision: 1,
            scope: MemoryScope::Personal,
            source_device: None,

            content: "中文".into(),
            context: Some("本人明确偏好".into()),
            status: MemoryStatus::Active,
            source_ids: vec![source.clone()],
            observed_at_unix_ms: 800,
            updated_at_unix_ms: 900,
            valid_from_unix_ms: 900,
            valid_until_unix_ms: None,
        };
        let usage = AssistantDataReadUse {
            projection_id: native_ids::stable(Kind::AssistantReadProjection, &"loop-candidate"),
            source_envelope_id: "candidate".into(),
            purpose: SourceReadPurpose::ModelMemory,
            retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
            scope_sha256: scope_sha256(session).unwrap(),
            policy_revision: 1,
            control_generation: data.control_generation,
            memory_state_generation: data.memory_state_generation,
            roots: vec![AssistantReadRoot {
                source_id: source.clone(),
                source_revision: 1,
            }],
            decision_roots: Vec::new(),
            history_dependencies: vec![HistoryDependency {
                source_id: source,
                source_revision: 1,
                content_sha256: "a".repeat(64),
                origin_id: native_ids::stable(Kind::AssistantOrigin, &"loop-current-origin"),
                origin_deletion_generation: 1,
                policy_revision: 1,
                expires_at_unix_ms: None,
            }],
            memories: vec![CurrentMemoryUse {
                memory_id: memory,
                revision: 1,
                purpose: MemoryUsePurpose::CurrentFact,
            }],
            expires_at_unix_ms: None,
        };
        if self.large_batch {
            record.content = "汉\\\"".repeat(200);
        }
        let mut candidates = vec![(record.clone(), usage.clone())];
        if self.large_batch {
            for index in 1..8 {
                let mut record = record.clone();
                let mut usage = usage.clone();
                record.memory_id = native_ids::stable(Kind::Memory, &("loop-current-fact", index));
                record.source_ids = vec![native_ids::stable(
                    Kind::AssistantSource,
                    &("loop-current-evidence", index),
                )];
                usage.projection_id =
                    native_ids::stable(Kind::AssistantReadProjection, &("loop-candidate", index));
                usage.memories[0].memory_id = record.memory_id.clone();
                usage.roots[0].source_id = record.source_ids[0].clone();
                usage.history_dependencies[0].source_id = record.source_ids[0].clone();
                usage.history_dependencies[0].origin_id =
                    native_ids::stable(Kind::AssistantOrigin, &("loop-current-origin", index));
                candidates.push((record, usage));
            }
        }
        let mut records = Vec::new();
        let mut selected = Vec::new();
        for (record, usage) in candidates {
            records.push(record);
            if CurrentMemoryPayload::new(data.memory_state_generation, records.clone())
                .encode(maximum, now)
                .unwrap()
                .is_some()
            {
                selected.push(usage);
            } else {
                records.pop();
                break;
            }
        }
        if records.is_empty() {
            return CurrentMemoryBlock {
                memory_state_generation: data.memory_state_generation,
                records,
                source_envelopes: Vec::new(),
                uses: Vec::new(),
            };
        }
        let access = access(session);
        let body = CurrentMemoryPayload::new(data.memory_state_generation, records.clone())
            .encode(maximum, now)
            .unwrap()
            .unwrap();
        let proof = ReadProjectionProof::for_current_memories(
            CurrentMemoryPreparation {
                session,
                access: &access,
                destination,
                preparation_id: preparation,
                maximum_bytes: maximum,
                authority_deadline: 90_000,
                now,
            },
            &body,
            &selected,
        )
        .unwrap();
        let mut block = CurrentMemoryBlock {
            memory_state_generation: data.memory_state_generation,
            records,
            source_envelopes: vec![proof.source_envelope],
            uses: vec![proof.usage],
        };
        match self.mode {
            Mode::Valid => {}
            Mode::ChangedGeneration => block.memory_state_generation += 1,
        }
        block
    }
}

#[async_trait(?Send)]
impl MemorySeam for Port {
    async fn current_memories(
        &self,
        session: &PersistedAgentSession,
        destination: &DestinationIdentity,
        preparation: &str,
        maximum: usize,
        now: u64,
    ) -> Result<CurrentMemoryBlock, AgentError> {
        let block = self.block(session, destination, preparation, maximum, now);
        self.calls
            .borrow_mut()
            .push((preparation.into(), maximum, block.records.len()));
        Ok(block)
    }
    async fn search_memories(
        &self,
        _: AssistantDataCall<'_>,
        _: &MemorySearchRequest,
    ) -> Result<AssistantDataRead<MemorySearchResult>, AgentError> {
        unreachable!()
    }
    async fn read_memory_history(
        &self,
        _: AssistantDataCall<'_>,
        _: &desk_agent_protocol::ai_assistant::memory::MemoryHistoryRequest,
    ) -> Result<
        AssistantDataRead<desk_agent_protocol::ai_assistant::memory::MemoryHistoryResult>,
        AgentError,
    > {
        unreachable!()
    }
    async fn read_memory_source(
        &self,
        _: AssistantDataCall<'_>,
        _: &str,
    ) -> Result<AssistantDataRead<AssistantSourceResolution>, AgentError> {
        unreachable!()
    }
}

struct Store {
    port: Port,
    stored: RefCell<PersistedAgentSession>,
    saves: Cell<u32>,
    fail: bool,
    generation_delta: Cell<u64>,
}
fn access(session: &PersistedAgentSession) -> AssistantDataToolAccess {
    let data = session.assistant_data.as_ref().unwrap();
    AssistantDataToolAccess {
        settings: AssistantDataSettings::default(),
        policy_revision: 1,
        memory_state_generation: data.memory_state_generation,
        control_generation: data.control_generation,
    }
}
#[async_trait(?Send)]
impl SessionSeam for Store {
    fn memory(&self) -> Option<&dyn MemorySeam> {
        Some(&self.port)
    }
    async fn assistant_data_access(
        &self,
        session: &PersistedAgentSession,
        _: u64,
    ) -> Result<Option<AssistantDataToolAccess>, AgentError> {
        let mut access = access(session);
        access.memory_state_generation += self.generation_delta.get();
        Ok(Some(access))
    }
    async fn claim_turn(&self, _: ClaimTurnParams) -> Result<PersistedAgentSession, ClaimError> {
        unreachable!()
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.saves.set(self.saves.get() + 1);
        if self.fail {
            return Err(changed("fixture source save failure"));
        }
        let previous = self.stored.borrow();
        assert_eq!(previous.version, session.version);
        assert_eq!(previous.lease_token, session.lease_token);
        drop(previous);
        session.version += 1;
        *self.stored.borrow_mut() = session.clone();
        Ok(())
    }
}
struct Model {
    completion: bool,
    calls: Cell<u32>,
    maximum: Cell<usize>,
    memory_padding: Cell<usize>,
    measurable: Cell<bool>,
}
fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    }
}

#[async_trait(?Send)]
impl ModelSeam for Model {
    fn command_completion_event_id(&self) -> Option<&str> {
        self.completion.then_some("completion")
    }
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(crate::model_egress::ModelEgressPolicy {
            destination: destination(),
            selected_source_tools: Default::default(),
            export_authorization_id: "fixture-export".into(),
            now_unix_ms: 1000,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        }))
    }
    fn prepare_request(
        &self,
        request: &ModelRequest,
    ) -> Result<Option<crate::model_preparation::ModelRequestPreparation>, AgentError> {
        if !self.measurable.get() {
            return Ok(None);
        }
        let messages = request.messages.iter().map(|message| serde_json::json!({ "role": message.role, "content": message.text.as_str() })).collect::<Vec<_>>();
        let mut body = serde_json::json!({ "messages": messages });
        if request.messages.iter().any(current_memory::is_projection)
            && self.memory_padding.get() > 0
        {
            body["model_format_padding"] = "x".repeat(self.memory_padding.get()).into();
        }
        crate::model_preparation::ModelRequestPreparation::measure(
            &body,
            request,
            self.maximum.get(),
        )
        .map(Some)
    }
    async fn call(&self, _: ModelRequest, _: &mut dyn TurnSink) -> Result<ModelTurn, AgentError> {
        self.calls.set(self.calls.get() + 1);
        unreachable!()
    }
}
struct Tools;
#[async_trait(?Send)]
impl ToolSeam for Tools {
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        unreachable!()
    }
}

fn fixture(mode: Mode, fail: bool) -> (PersistedAgentSession, Store, Model, ModelRequest) {
    let mut session = personal_session("owner", "root", "device");
    session.input_revision = 1;
    session.version = 1;
    session.lease_token = 1;
    session.current_turn_id = Some("turn".into());
    let store = Store {
        port: Port {
            mode,
            calls: RefCell::new(Vec::new()),
            large_batch: false,
        },
        stored: RefCell::new(session.clone()),
        saves: Cell::new(0),
        fail,
        generation_delta: Cell::new(0),
    };
    let model = Model {
        completion: false,
        calls: Cell::new(0),
        maximum: Cell::new(65_536),
        memory_padding: Cell::new(0),
        measurable: Cell::new(true),
    };
    let mut request = ModelRequest::text_only(
        vec![ChatMessage::text(
            "system",
            ChatRole::System,
            "server instruction",
        )],
        crate::prompt::ResponseFormatSpec::None,
    );
    request.context_conversation_id = Some(session.conversation_id.clone());
    (session, store, model, request)
}
fn deps<'a>(
    store: &'a Store,
    model: &'a Model,
    tools: &'a Tools,
    clock: &'a dyn Fn() -> String,
) -> LoopDeps<'a> {
    LoopDeps {
        response_locale: None,
        interactive_user_home: None,
        interactive_user_home_incarnation: None,
        session_seam: store,
        model,
        tools,
        content_safety: crate::content_safety::ContentSafetyMode::Disabled,
        registry: &[],
        provider_registry: None,
        capability_inventory: None,
        capability_permission_candidates: &[],
        capability_catalog_metrics: None,
        permission_continuation_exact_tools: &[],
        response_format: crate::prompt::ResponseFormatSpec::None,
        system_prompt: ChatMessage::text("system", ChatRole::System, "server instruction"),
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock,
        heartbeat: None,
    }
}

#[tokio::test]
async fn current_body_enters_only_the_request_after_its_exact_source_references_are_saved_and_retries_do_not_grow_them()
 {
    let (mut session, store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    let deps = deps(&store, &model, &tools, &clock);
    prepare(&deps, &mut session, &mut request, "turn")
        .await
        .unwrap();
    assert_eq!(store.saves.get(), 1);
    assert_eq!(request.messages.len(), 2);
    assert!(current_memory::is_projection(&request.messages[1]));
    assert!(
        crate::ai_assistant::seam::AssistantModelDataManifest::validate_absence(&request).is_err()
    );
    assert!(
        session
            .conversation
            .iter()
            .all(|message| !current_memory::is_projection(message))
    );
    {
        let saved = store.stored.borrow();
        let data = saved.assistant_data.as_ref().unwrap();
        assert_eq!(data.current_memories.len(), 1);
        assert_eq!(data.read_uses.len(), 1);
    }
    let original = request.messages[1].clone();
    prepare(&deps, &mut session, &mut request, "turn")
        .await
        .unwrap();
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.messages[1].text, original.text);
    assert_eq!(request.messages[1].data_envelope, original.data_envelope);
    assert_eq!(store.saves.get(), 1);
    assert_eq!(session.assistant_data.as_ref().unwrap().read_uses.len(), 1);
    assert_eq!(store.port.calls.borrow()[0], store.port.calls.borrow()[1]);
    assert_eq!(model.calls.get(), 0);
}

#[tokio::test]
#[allow(clippy::field_reassign_with_default)]
async fn context_assembly_keeps_current_facts_after_personalization_and_before_the_actual_question()
{
    let (mut session, store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    let mut instructions =
        desk_agent_protocol::ai_assistant::user_instructions::UserInstructions::default();
    instructions.content = "回答保持简洁".into();
    let frozen = crate::ai_assistant::settings::freeze_instructions(&instructions, 1, 100).unwrap();
    session.assistant_data.as_mut().unwrap().instructions = frozen.clone();
    *store.stored.borrow_mut() = session.clone();
    request.messages.push(
        crate::ai_assistant::settings::instructions_projection(
            &frozen,
            &session.conversation_id,
            destination(),
        )
        .unwrap()
        .unwrap(),
    );
    let policy = crate::model_context::PinnedContextPolicy::window(
        crate::replay::SourceContextKey::derive(
            crate::model_profile::WireProtocol::OpenAiChatCompletions,
            "model",
            "profile",
            "source",
        ),
        1,
        crate::MIN_MODEL_CONTEXT_BYTES * 4,
    )
    .unwrap();
    let view = crate::model_context::ModelContextView {
        notice_kind: crate::model_context::ContextNoticeKind::Refreshed,
        messages: vec![ChatMessage::text(
            "question",
            ChatRole::User,
            "现在应该怎么处理？",
        )],
        policy_key: policy.key(),
        floor_group_head_message_id: None,
        floor_advanced: false,
    };
    let mut assembled =
        crate::agent_loop::assemble_context_request(&request, &view, &session).unwrap();
    prepare(
        &deps(&store, &model, &tools, &clock),
        &mut session,
        &mut assembled,
        "turn",
    )
    .await
    .unwrap();
    assert_eq!(assembled.messages.len(), 4);
    assert_eq!(assembled.messages[0].role, ChatRole::System);
    assert!(crate::ai_assistant::settings::is_personalization_projection(&assembled.messages[1]));
    assert!(current_memory::is_projection(&assembled.messages[2]));
    assert_eq!(assembled.messages[3].message_id, "question");
    assert!(session.conversation.is_empty());
}

#[tokio::test]
async fn a_changed_database_generation_requires_reconciliation_before_current_memory_is_installed()
{
    let mode = Mode::ChangedGeneration;
    let (mut session, store, model, mut request) = fixture(mode, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    assert!(
        prepare(
            &deps(&store, &model, &tools, &clock),
            &mut session,
            &mut request,
            "turn"
        )
        .await
        .is_err()
    );
    assert_eq!(request.messages.len(), 1);
    assert_eq!(store.saves.get(), 0);
    assert!(
        store
            .stored
            .borrow()
            .assistant_data
            .as_ref()
            .unwrap()
            .current_memories
            .is_empty()
    );
    assert_eq!(model.calls.get(), 0);
}

#[tokio::test]
async fn a_later_memory_generation_refreshes_the_optional_block_without_withdrawing_the_conversation()
 {
    let (mut session, store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    prepare(
        &deps(&store, &model, &tools, &clock),
        &mut session,
        &mut request,
        "turn",
    )
    .await
    .unwrap();
    store.generation_delta.set(1);
    prepare(
        &deps(&store, &model, &tools, &clock),
        &mut session,
        &mut request,
        "turn",
    )
    .await
    .unwrap();
    assert_eq!(store.port.calls.borrow().len(), 2);
    assert_eq!(store.saves.get(), 3);
    assert_eq!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .memory_state_generation,
        2
    );
    assert_eq!(
        request
            .messages
            .iter()
            .filter(|m| current_memory::is_projection(m))
            .count(),
        1
    );
    assert_eq!(model.calls.get(), 0);
}

#[tokio::test]
async fn a_failed_source_cas_stops_before_current_text_or_a_model_call_and_ineligible_requests_do_not_load_it()
 {
    let (mut session, store, model, mut request) = fixture(Mode::Valid, true);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    assert!(
        prepare(
            &deps(&store, &model, &tools, &clock),
            &mut session,
            &mut request,
            "turn"
        )
        .await
        .is_err()
    );
    assert_eq!(request.messages.len(), 1);
    assert!(
        store
            .stored
            .borrow()
            .assistant_data
            .as_ref()
            .unwrap()
            .read_uses
            .is_empty()
    );
    assert_eq!(model.calls.get(), 0);
    for variant in 0..5 {
        let (mut session, store, mut model, mut request) = fixture(Mode::Valid, false);
        match variant {
            0 => session.surface = AgentSessionSurface::TerminalAiAssistant,
            1 => {
                let data = session.assistant_data.as_mut().unwrap();
                data.temporary = true;
                data.history_access_scope = None;
                data.memory_access_scope = None;
            }
            2 => request.use_case = crate::model_profile::ModelUseCase::ContextCompression,
            3 => model.completion = true,
            _ => session.assistant_data.as_mut().unwrap().memory_access_scope = None,
        }
        prepare(
            &deps(&store, &model, &tools, &clock),
            &mut session,
            &mut request,
            "turn",
        )
        .await
        .unwrap();
        assert!(store.port.calls.borrow().is_empty());
        assert_eq!(store.saves.get(), 0);
        assert_eq!(request.messages.len(), 1);
    }
}

fn protected_question(request: &mut ModelRequest) {
    request.messages.push(ChatMessage::assistant_tool_calls(
        "producer",
        "",
        vec![crate::chat::ToolCallRef {
            id: "important-result".into(),
            name: "inspect".into(),
            arguments_json: "{}".into(),
        }],
    ));
    request.messages.push(ChatMessage::tool_result(
        "result",
        "important-result",
        "protected actual tool evidence",
    ));
    request.messages.push(ChatMessage::text(
        "original-question",
        ChatRole::User,
        "请依据实际结果继续处理",
    ));
}

#[tokio::test]
async fn a_temporary_request_does_not_require_a_memory_manifest_just_because_the_host_has_memory_ports()
 {
    let (mut session, store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    let data = session.assistant_data.as_mut().unwrap();
    data.temporary = true;
    data.history_access_scope = None;
    data.memory_access_scope = None;
    super::super::prepare_request(
        &deps(&store, &model, &tools, &clock),
        &session,
        &mut request,
    )
    .await
    .unwrap();
    assert!(request.assistant_data.is_none());
    assert!(store.port.calls.borrow().is_empty());
    assert_eq!(store.saves.get(), 0);
}

#[tokio::test]
async fn an_eligible_request_with_memory_ports_still_requires_its_host_manifest() {
    let (session, store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    assert!(
        super::super::prepare_request(
            &deps(&store, &model, &tools, &clock),
            &session,
            &mut request
        )
        .await
        .is_err()
    );
    assert!(request.assistant_data.is_none());
    assert!(store.port.calls.borrow().is_empty());
    assert_eq!(model.calls.get(), 0);
}

#[tokio::test]
async fn no_spare_or_unmeasurable_context_omits_optional_memory_without_touching_the_question_or_protected_results()
 {
    for measurable in [true, false] {
        let (mut session, store, model, mut request) = fixture(Mode::Valid, false);
        let tools = Tools;
        let clock = || "1970-01-01T00:00:01Z".into();
        protected_question(&mut request);
        let baseline = model.prepare_request(&request).unwrap().unwrap();
        model.maximum.set(baseline.context_bytes);
        model.measurable.set(measurable);
        let original = serde_json::to_string(&request.messages).unwrap();
        let added = prepare(
            &deps(&store, &model, &tools, &clock),
            &mut session,
            &mut request,
            "turn",
        )
        .await
        .unwrap();
        assert_eq!(added, 0);
        assert_eq!(serde_json::to_string(&request.messages).unwrap(), original);
        assert!(store.port.calls.borrow().is_empty());
        assert_eq!(store.saves.get(), 0);
        assert_eq!(model.calls.get(), 0);
    }
}

#[tokio::test]
async fn an_oversized_required_request_remains_an_original_context_error_and_does_not_start_recall()
{
    let (mut session, store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    protected_question(&mut request);
    let baseline = model.prepare_request(&request).unwrap().unwrap();
    model.maximum.set(baseline.context_bytes - 1);
    let original = serde_json::to_string(&request.messages).unwrap();
    let error = prepare(
        &deps(&store, &model, &tools, &clock),
        &mut session,
        &mut request,
        "turn",
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.error_code,
        Some(desk_utils::error::DeskErrorCode::AI_CONTEXT_ITEM_TOO_LARGE.code())
    );
    assert_eq!(serde_json::to_string(&request.messages).unwrap(), original);
    assert!(store.port.calls.borrow().is_empty());
    assert_eq!(store.saves.get(), 0);
}

#[tokio::test]
async fn an_actual_wire_overrun_gets_one_smaller_complete_fact_preparation_and_persists_only_what_will_be_sent()
 {
    let (mut session, mut store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    store.port.large_batch = true;
    protected_question(&mut request);
    let original = request.messages.clone();
    let baseline = model.prepare_request(&request).unwrap().unwrap();
    model.maximum.set(baseline.context_bytes + 16_384);
    // Set the alternate wire format just beyond the first complete block's
    // exact measured limit, rather than guessing how many UTF-8 facts fit.
    let first_maximum = 16_384.min(model.maximum.get() / 2);
    let preview = store
        .port
        .block(&session, &destination(), "preview", first_maximum, 1000);
    assert!(preview.records.len() > 1);
    let preview = project(&session, &access(&session), preview, first_maximum, 1000).unwrap();
    let first_payload_bytes =
        current_memory::framed_bytes(&preview.message.as_ref().unwrap().text).unwrap();
    assert!(first_payload_bytes < first_maximum);
    let mut first_request = request.clone();
    first_request.messages.insert(1, preview.message.unwrap());
    let first_cost = model
        .prepare_request(&first_request)
        .unwrap()
        .unwrap()
        .context_bytes;
    model
        .memory_padding
        .set(model.maximum.get().saturating_sub(first_cost) + 128);
    let excess = model
        .prepare_request(&first_request)
        .unwrap()
        .unwrap()
        .context_bytes
        - model.maximum.get();
    let added = prepare(
        &deps(&store, &model, &tools, &clock),
        &mut session,
        &mut request,
        "turn",
    )
    .await
    .unwrap();
    let calls = store.port.calls.borrow();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, first_maximum);
    assert_eq!(calls[1].1, first_payload_bytes - excess);
    assert!(calls[1].2 > 0 && calls[1].2 < calls[0].2);
    assert_ne!(calls[0].0, calls[1].0);
    let measured = model.prepare_request(&request).unwrap().unwrap();
    assert!(measured.context_bytes <= measured.maximum_context_bytes);
    assert_eq!(added, measured.context_bytes - baseline.context_bytes);
    let retained = request
        .messages
        .iter()
        .filter(|message| !current_memory::is_projection(message))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_string(&retained).unwrap(),
        serde_json::to_string(&original).unwrap()
    );
    let data = session.assistant_data.as_ref().unwrap();
    assert_eq!(data.read_uses.len(), 1);
    assert_eq!(data.current_memories.len(), calls[1].2);
    assert_eq!(data.history_dependencies.len(), calls[1].2);
    assert_eq!(store.saves.get(), 1);
    assert_eq!(model.calls.get(), 0);
}

#[tokio::test]
async fn impossible_wire_overhead_omits_optional_facts_without_another_preparation_or_source_reference_save()
 {
    let (mut session, mut store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    store.port.large_batch = true;
    protected_question(&mut request);
    let baseline = model.prepare_request(&request).unwrap().unwrap();
    model.maximum.set(baseline.context_bytes + 8192);
    model.memory_padding.set(16_384);
    let original = serde_json::to_string(&request.messages).unwrap();
    assert_eq!(
        prepare(
            &deps(&store, &model, &tools, &clock),
            &mut session,
            &mut request,
            "turn"
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(serde_json::to_string(&request.messages).unwrap(), original);
    assert_eq!(store.port.calls.borrow().len(), 1);
    assert!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .read_uses
            .is_empty()
    );
    assert_eq!(store.saves.get(), 0);
    assert_eq!(model.calls.get(), 0);
}

#[tokio::test]
async fn later_budget_omission_clears_only_the_current_selection_and_keeps_older_output_dependencies()
 {
    let (mut session, store, model, mut request) = fixture(Mode::Valid, false);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    prepare(
        &deps(&store, &model, &tools, &clock),
        &mut session,
        &mut request,
        "turn",
    )
    .await
    .unwrap();
    let previous = session.assistant_data.as_ref().unwrap().clone();
    model.measurable.set(false);
    assert_eq!(
        prepare(
            &deps(&store, &model, &tools, &clock),
            &mut session,
            &mut request,
            "turn"
        )
        .await
        .unwrap(),
        0
    );
    let data = session.assistant_data.as_ref().unwrap();
    assert!(data.current_memories.is_empty());
    assert_eq!(data.read_uses, previous.read_uses);
    assert_eq!(data.history_dependencies, previous.history_dependencies);
    assert!(
        request
            .messages
            .iter()
            .all(|message| !current_memory::is_projection(message))
    );
    assert_eq!(store.saves.get(), 2);
    assert_eq!(store.port.calls.borrow().len(), 1);
    assert_eq!(model.calls.get(), 0);
}

#[tokio::test]
async fn current_memory_injection_uses_platform_headroom_above_eight_kib_without_model_calls() {
    let (mut session, mut store, model, mut request) = fixture(Mode::Valid, false);
    store.port.large_batch = true;
    model.maximum.set(262_144);
    let tools = Tools;
    let clock = || "1970-01-01T00:00:01Z".into();
    let added = prepare(
        &deps(&store, &model, &tools, &clock),
        &mut session,
        &mut request,
        "large-context",
    )
    .await
    .unwrap();
    assert!(added > 8192);
    assert_eq!(store.port.calls.borrow()[0].1, 131_072);
    assert_eq!(store.port.calls.borrow()[0].2, 8);
    assert!(
        model
            .prepare_request(&request)
            .unwrap()
            .unwrap()
            .context_bytes
            <= model.maximum.get()
    );
    assert_eq!(model.calls.get(), 0);
}

#[test]
fn large_current_memory_context_assembly_fits_one_and_two_mib_stacks() {
    for stack_bytes in [1024 * 1024, 2 * 1024 * 1024] {
        let (mut session, mut store, model, mut request) = fixture(Mode::Valid, false);
        store.port.large_batch = true;
        model.maximum.set(262_144);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        std::thread::Builder::new()
            .name("current-memory-budget-stack".into())
            .stack_size(stack_bytes)
            .spawn(move || {
                assert_eq!(
                    std::thread::current().name(),
                    Some("current-memory-budget-stack")
                );
                let tools = Tools;
                let clock = || "1970-01-01T00:00:01Z".into();
                let dependencies = deps(&store, &model, &tools, &clock);
                let added = runtime
                    .block_on(crate::future::boxed(|| {
                        prepare(
                            &dependencies,
                            &mut session,
                            &mut request,
                            "large-stack-context",
                        )
                    }))
                    .unwrap();
                assert!(added > 8192);
                assert_eq!(store.port.calls.borrow()[0].1, 131_072);
                assert_eq!(store.port.calls.borrow()[0].2, 8);
                assert_eq!(model.calls.get(), 0);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
