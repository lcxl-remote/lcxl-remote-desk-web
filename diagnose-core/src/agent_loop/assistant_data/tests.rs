use super::*;
use crate::ai_assistant::{
    seam::{CurrentMemoryBlock, MemoryMutationSeam, MemorySeam},
    test_support::personal_session,
};
use crate::seam::{ClaimError, ClaimTurnParams};
use async_trait::async_trait;
use desk_agent_protocol::ai_assistant::{memory::*, source::AssistantSourceResolution};
use std::cell::{Cell, RefCell};

struct Store {
    stale: bool,
    current_access: Option<AssistantDataToolAccess>,
    commits: Cell<u32>,
    saves: Cell<u32>,
    stored: RefCell<PersistedAgentSession>,
}
#[async_trait(?Send)]
impl SessionSeam for Store {
    async fn assistant_data_request_manifest(
        &self,
        session: &PersistedAgentSession,
        request: &ModelRequest,
        now: u64,
    ) -> Result<Option<crate::ai_assistant::seam::AssistantModelDataManifest>, AgentError> {
        let access = self.assistant_data_access(session, now).await?.unwrap();
        Ok(Some(
            crate::ai_assistant::seam::AssistantModelDataManifest::for_direct_request(
                session,
                request,
                &access,
                &[],
            )
            .map_err(invalid)?,
        ))
    }
    fn memory(&self) -> Option<&dyn MemorySeam> {
        Some(self)
    }
    fn memory_mutation(&self) -> Option<&dyn MemoryMutationSeam> {
        Some(self)
    }
    async fn assistant_data_access(
        &self,
        _: &PersistedAgentSession,
        _: u64,
    ) -> Result<Option<AssistantDataToolAccess>, AgentError> {
        let data = self.stored.borrow();
        let data = data.assistant_data.as_ref().unwrap();
        Ok(Some(self.current_access.clone().unwrap_or(
            AssistantDataToolAccess {
                settings: Default::default(),
                policy_revision: 1,
                memory_state_generation: data.memory_state_generation,
                control_generation: data.control_generation,
            },
        )))
    }
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut session = self.stored.borrow_mut();
        session
            .begin_turn(
                params.turn_id.clone(),
                params.request_id,
                params.connection_id,
                params.policy_revision,
                params.current_pdp_scope,
                params.now,
            )
            .map_err(|_| ClaimError::Busy)?;
        session.adopt_trigger(params.trigger_origin, &params.turn_id);
        session.lease_token += 1;
        Ok(session.clone())
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.saves.set(self.saves.get() + 1);
        session.version += 1;
        *self.stored.borrow_mut() = session.clone();
        Ok(())
    }
}
#[async_trait(?Send)]
impl MemorySeam for Store {
    async fn current_memories(
        &self,
        _: &PersistedAgentSession,
        _: &desk_agent_protocol::data_lineage::DestinationIdentity,
        _: &str,
        _: usize,
        _: u64,
    ) -> Result<CurrentMemoryBlock, AgentError> {
        unreachable!()
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
        request: &MemoryHistoryRequest,
    ) -> Result<AssistantDataRead<MemoryHistoryResult>, AgentError> {
        Ok(AssistantDataRead {
            result: MemoryHistoryResult {
                memory_id: request.memory_id.clone(),
                current_revision: 2,
                versions: [
                    (2, "yyy", MemoryStatus::Active),
                    (1, "sss", MemoryStatus::Superseded),
                ]
                .into_iter()
                .map(|(revision, name, status)| MemoryHistoryEntry {
                    revision,
                    content: format!("Call me {name}"),
                    context: Some("Owner preference".into()),
                    status,
                    recorded_at_unix_ms: revision,
                    source_ids: vec![crate::native_ids::stable(
                        crate::native_ids::Kind::AssistantSource,
                        &revision,
                    )],
                })
                .collect(),
                has_more: false,
                next_cursor: None,
                partial: false,
                reasons: Vec::new(),
            },
            projection: None,
        })
    }
    async fn read_memory_source(
        &self,
        _: AssistantDataCall<'_>,
        _: &str,
    ) -> Result<AssistantDataRead<AssistantSourceResolution>, AgentError> {
        Err(invalid(
            "Requested memory evidence is unavailable; use read_conversation for history sources.",
        ))
    }
}
#[async_trait(?Send)]
impl MemoryMutationSeam for Store {
    async fn manage_memory(
        &self,
        _: AssistantDataCall<'_>,
        _: &MemoryWriteRequest,
    ) -> Result<AssistantDataRead<MemoryWriteResult>, AgentError> {
        self.commits.set(self.commits.get() + 1);
        Ok(AssistantDataRead {
            result: MemoryWriteResult {
                outcome: MemoryWriteOutcome::Skipped,
                memory: None,
                reason: Some("source_changed".into()),
                control_receipt: None,
            },
            projection: None,
        })
    }
    async fn forget_group(
        &self,
        session: &mut PersistedAgentSession,
        call: &crate::ai_assistant::seam::MemoryForgetCall<'_>,
    ) -> Result<AssistantDataControlReceipt, AgentError> {
        self.commits.set(self.commits.get() + 1);
        let data = session.assistant_data.as_ref().unwrap();
        let receipt = AssistantDataControlReceipt {
            operation_id: call.operation_id.into(),
            outcome: if self.stale {
                DataControlOutcome::StaleTarget
            } else {
                DataControlOutcome::Applied
            },
            reason: DataControlReason::MemoryForget,
            affected_count: if self.stale { 0 } else { 2 },
            control_generation: data.control_generation + u64::from(!self.stale),
            memory_state_generation: data.memory_state_generation + u64::from(!self.stale),
            cleanup_pending: !self.stale,
        };
        if !self.stale {
            let message = session
                .conversation
                .iter()
                .find(|message| message.message_id == call.producer_message_id)
                .unwrap();
            let hash =
                crate::ai_assistant::data_control::tool_group_sha256(std::slice::from_ref(message))
                    .unwrap();
            crate::ai_assistant::data_control::commit_memory_forget_group(
                session,
                call.producer_message_id,
                &hash,
                &receipt,
                "1970-01-01T00:00:01Z",
            )
            .unwrap();
            *self.stored.borrow_mut() = session.clone();
        }
        Ok(receipt)
    }
}

struct Model;
#[async_trait(?Send)]
impl ModelSeam for Model {
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(crate::model_egress::ModelEgressPolicy {
            destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
            selected_source_tools: Default::default(),
            export_authorization_id: "export".into(),
            now_unix_ms: 1000,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        }))
    }
    async fn call(
        &self,
        _: ModelRequest,
        _: &mut dyn TurnSink,
    ) -> Result<crate::chat::ModelTurn, AgentError> {
        panic!("control must not call a model")
    }
}
struct Tools;
#[async_trait(?Send)]
impl ToolSeam for Tools {
    async fn run_read(&self, _: &ToolCall) -> Result<crate::seam::ToolRunOutput, AgentError> {
        panic!("control must not dispatch another tool")
    }
}
#[derive(Default)]
struct Sink {
    finished: Vec<bool>,
    answers: Vec<String>,
}
impl TurnSink for Sink {
    fn on_text_delta(&mut self, _: &str) {}
    fn on_answer_committed(&mut self, text: &str) {
        self.answers.push(text.into());
    }
    fn on_tool_finished(&mut self, _: &str, ok: bool, _: &str, _: Option<&str>) {
        self.finished.push(ok);
    }
}

#[tokio::test]
async fn complete_forget_dispatch_returns_results_to_the_active_loop() {
    exercise(false, true, 0).await;
}

#[tokio::test]
async fn a_stale_batch_returns_all_results_without_control_closure_or_generation_changes() {
    exercise(true, true, 0).await;
}

#[tokio::test]
async fn an_unexposed_or_over_limit_forget_batch_never_partially_reaches_the_port() {
    exercise(false, false, 0).await;
    exercise(false, true, crate::MAX_SAME_TOOL_PER_TURN - 1).await;
    exercise(false, true, u32::MAX).await;
}

async fn exercise(stale: bool, exposed: bool, prior: u32) {
    let mut session = personal_session("owner", "root", "device");
    session.turn_state = crate::session::TurnState::Running;
    session.current_turn_id = Some("turn".into());
    session.latest_input_seq = 1;
    session.input_revision = 1;
    let calls = ["a", "b"].map(|id| ToolCall { id: id.into(), name: data_tools::MANAGE_MEMORIES.into(),
        arguments_json: serde_json::json!({"operation":"forget", "memory_id":crate::native_ids::stable(crate::native_ids::Kind::Memory, &id), "expected_revision":1}).to_string() });
    session.conversation.push(
        ChatMessage::assistant_tool_calls(
            "producer",
            "old model text",
            calls.iter().map(ToolCall::to_ref).collect(),
        )
        .with_turn_id("turn"),
    );
    let store = Store {
        stale,
        current_access: None,
        commits: Cell::new(0),
        saves: Cell::new(0),
        stored: RefCell::new(session.clone()),
    };
    let clock = || "1970-01-01T00:00:01Z".into();
    let deps = deps(&store, &clock);
    let mut sink = Sink::default();
    let targets = data_tools::prepare_memory_batch(&calls, &Default::default())
        .unwrap()
        .unwrap();
    let result = crate::future::phase(|| {
        dispatch_forget(
            &deps,
            &mut session,
            &calls,
            &targets,
            "producer",
            exposed,
            prior,
            &mut sink,
        )
    })
    .await;
    if !exposed || prior >= crate::MAX_SAME_TOOL_PER_TURN - 1 {
        assert_eq!(store.commits.get(), 0);
        assert_eq!(store.saves.get(), 1);
        assert_eq!(sink.finished, vec![false; 2]);
        assert_eq!(session.conversation.len(), 3);
        if exposed {
            assert!(matches!(
                result.unwrap(),
                Some(LoopOutcome::CircuitBreak(
                    CircuitBreakReason::SameToolRepeat
                ))
            ));
        } else {
            assert!(result.is_err());
        }
        return;
    }
    let result = result.unwrap();
    assert_eq!(store.commits.get(), 1);
    assert_eq!(store.saves.get(), u32::from(stale));
    assert_eq!(sink.finished, vec![!stale; 2]);
    assert_eq!(session.conversation.len(), 3);
    let data = session.assistant_data.as_ref().unwrap();
    assert_eq!(data.control_generation, 1 + u64::from(!stale));
    assert!(result.is_none());
    assert_eq!(session.turn_state, crate::session::TurnState::Running);
    assert_eq!(session.handled_input_seq, 0);
    assert!(sink.answers.is_empty());
    assert_eq!(
        session.model_conversation().len(),
        session.conversation.len()
    );
}

fn deps<'a>(store: &'a Store, clock: &'a dyn Fn() -> String) -> LoopDeps<'a> {
    LoopDeps {
        response_locale: None,
        interactive_user_home: None,
        interactive_user_home_incarnation: None,
        session_seam: store,
        model: &Model,
        tools: &Tools,
        content_safety: crate::content_safety::ContentSafetyMode::Disabled,
        registry: &[],
        provider_registry: None,
        capability_inventory: None,
        capability_permission_candidates: &[],
        capability_catalog_metrics: None,
        permission_continuation_exact_tools: &[],
        response_format: crate::prompt::ResponseFormatSpec::None,
        system_prompt: ChatMessage::text("system", ChatRole::System, "instruction"),
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock,
        heartbeat: None,
    }
}

#[tokio::test]
async fn a_skipped_write_uses_original_scope_without_rewriting_generations_or_reading_new_facts() {
    let mut session = personal_session("owner", "root", "device");
    session.turn_state = crate::session::TurnState::Running;
    session.current_turn_id = Some("turn".into());
    session.input_revision = 1;
    let call = ToolCall { id: "write-call".into(), name: data_tools::MANAGE_MEMORIES.into(),
        arguments_json: serde_json::json!({"operation":"remember", "content":"a stable fact", "source_refs":[{
            "kind":"input", "id":crate::native_ids::stable(crate::native_ids::Kind::AssistantSource, &"owner-input"),
            "role":"evidence", "revision":1}]}).to_string() };
    let policy = Model.model_egress_policy().unwrap().unwrap();
    let input = crate::model_message_labels::model_bound_user_message(
        "owner-input".into(),
        "new assertion".into(),
        policy.destination.clone(),
    )
    .unwrap()
    .data_envelope
    .unwrap();
    let parent = policy
        .derive_model_output_envelope(
            &crate::chat::ModelTurn {
                tool_calls: vec![call.clone()],
                stop_reason: crate::chat::StopReason::ToolUse,
                ..Default::default()
            },
            &[input],
        )
        .unwrap();
    let mut producer =
        ChatMessage::assistant_tool_calls("producer", "", vec![call.to_ref()]).with_turn_id("turn");
    producer.data_envelope = Some(parent.clone());
    session.conversation.push(producer);
    let before = session.clone();
    let data = session.assistant_data.as_ref().unwrap();
    let store = Store {
        stale: false,
        current_access: Some(AssistantDataToolAccess {
            settings: desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettings {
                automatic_memory_enabled: false,
                ..Default::default()
            },
            policy_revision: 1,
            memory_state_generation: data.memory_state_generation + 1,
            control_generation: data.control_generation + 1,
        }),
        commits: Cell::new(0),
        saves: Cell::new(0),
        stored: RefCell::new(session.clone()),
    };
    let clock = || "1970-01-01T00:00:01Z".into();
    let deps = deps(&store, &clock);
    let result = crate::future::boxed(|| {
        dispatch(
            &deps,
            &mut session,
            &call,
            "producer",
            "result".into(),
            Some(&parent),
        )
    })
    .await
    .unwrap();
    assert!(result.ok && !result.replayed);
    assert_eq!(
        result.message.data_envelope,
        crate::model_message_labels::internal_tool_result_envelope(
            Some(&parent),
            &call.id,
            &result.message.text,
            &call.name,
        )
        .unwrap()
    );
    assert!(data_tools::is_memory_write_acknowledgement(
        &result.message.text
    ));
    assert_eq!(session, before);
    assert_eq!(store.commits.get(), 1);
    assert_eq!(store.saves.get(), 0);
}

struct FollowupModel {
    history: bool,
    calls: Cell<u32>,
    fail_reply: bool,
    unavailable_source: bool,
}
#[async_trait(?Send)]
impl ModelSeam for FollowupModel {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        Ok(request.caller_output_hard_cap.unwrap_or(1024).min(1024))
    }
    async fn context_policy(
        &self,
        _: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        crate::model_context::PinnedContextPolicy::window(
            crate::replay::SourceContextKey::derive(
                crate::model_profile::WireProtocol::OpenAiChatCompletions,
                "gateway",
                "model",
                "test",
            ),
            1,
            crate::MIN_MODEL_CONTEXT_BYTES * 4,
        )
        .map_err(|error| invalid(error.to_string()))
    }
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Model.model_egress_policy()
    }
    async fn call(
        &self,
        request: ModelRequest,
        _: &mut dyn TurnSink,
    ) -> Result<crate::chat::ModelTurn, AgentError> {
        use crate::chat::{ModelTurn, StopReason};
        assert_eq!(
            std::thread::current().name(),
            Some("memory-data-followup-stack")
        );
        let ordinal = self.calls.get();
        self.calls.set(ordinal + 1);
        assert!(
            ordinal < 2,
            "forget should continue once to the requested reply"
        );
        let policy = self.model_egress_policy()?.unwrap();
        let authorized = policy
            .authorize_request(request)
            .map_err(|error| invalid(error.to_string()))?;
        let mut turn = if ordinal == 0 && self.history {
            assert!(
                authorized
                    .request
                    .tools
                    .iter()
                    .any(|tool| tool.name == data_tools::READ_MEMORY_HISTORY)
            );
            ModelTurn {
                stop_reason: StopReason::ToolUse,
                tool_calls: vec![ToolCall { id: "read-history".into(), name: data_tools::READ_MEMORY_HISTORY.into(),
                    arguments_json: serde_json::json!({"memory_id": shown_memory_alias(&authorized.request, &crate::native_ids::stable(crate::native_ids::Kind::Memory, &"preferred-name"))}).to_string() }],
                ..Default::default()
            }
        } else if ordinal == 0 && self.unavailable_source {
            ModelTurn {
                stop_reason: StopReason::ToolUse,
                tool_calls: vec![ToolCall {
                    id: "source-read".into(),
                    name: data_tools::READ_MEMORY_SOURCE.into(),
                    arguments_json: serde_json::json!({"source_id": crate::native_ids::stable(crate::native_ids::Kind::AssistantSource, &"history-source")}).to_string(),
                }],
                ..Default::default()
            }
        } else if ordinal == 0 {
            assert!(
                authorized
                    .request
                    .tools
                    .iter()
                    .any(|tool| tool.name == data_tools::MANAGE_MEMORIES)
            );
            ModelTurn { stop_reason: StopReason::ToolUse,
                tool_calls: ["a", "b"].map(|id| ToolCall { id: id.into(), name: data_tools::MANAGE_MEMORIES.into(),
                    arguments_json: serde_json::json!({"operation":"forget","memory_id":crate::native_ids::stable(crate::native_ids::Kind::Memory, &id),"expected_revision":1}).to_string() }).into(),
                ..Default::default() }
        } else {
            let results: Vec<_> = authorized
                .request
                .messages
                .iter()
                .filter(|message| message.role == ChatRole::Tool)
                .collect();
            assert_eq!(
                results.len(),
                if self.unavailable_source || self.history {
                    1
                } else {
                    2
                }
            );
            if self.history {
                let history: MemoryHistoryResult = serde_json::from_str(&results[0].text).unwrap();
                assert!(history.versions[0].content.contains("yyy"));
                assert!(history.versions[1].content.contains("sss"));
                assert_eq!(history.versions[1].status, MemoryStatus::Superseded);
            }
            if self.unavailable_source {
                assert_eq!(results[0].tool_ok, Some(false));
                assert!(results[0].text.contains("read_conversation"));
            }
            for result in results
                .into_iter()
                .filter(|_| !self.unavailable_source && !self.history)
            {
                let receipt: AssistantDataControlReceipt =
                    serde_json::from_str(&result.text).unwrap();
                assert_eq!(receipt.outcome, DataControlOutcome::Applied);
                assert_eq!(receipt.affected_count, 2);
            }
            if self.fail_reply {
                return Err(invalid("synthetic reply failure after committed forget"));
            }
            ModelTurn {
                text: if self.history {
                    "Previously sss; currently yyy."
                } else {
                    "The two memories were deleted."
                }
                .into(),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            }
        };
        turn.provider_meta.stop_reason = turn.stop_reason;
        turn.provider_meta.replay = Some(crate::replay::ReplayDisposition::NotRequired {
            source_context_key: crate::replay::SourceContextKey::derive(
                crate::model_profile::WireProtocol::OpenAiChatCompletions,
                "gateway",
                "model",
                "test",
            ),
        });
        turn.provider_meta.data_envelope = Some(
            policy
                .derive_model_output_envelope(&turn, &authorized.input_envelopes)
                .map_err(|error| invalid(error.to_string()))?,
        );
        Ok(turn)
    }
}

async fn exercise_data_followup(history_mode: bool) {
    for (history, unavailable_source, fail_reply) in [
        (false, false, false),
        (false, false, true),
        (false, true, false),
        (false, true, true),
        (true, false, false),
        (true, false, true),
    ]
    .into_iter()
    .filter(|case| case.0 == history_mode)
    {
        let mut session = personal_session("owner", "root", "device");
        session.latest_input_seq = 1;
        session.input_revision = 1;
        // An earlier result showed this memory; the model copies its alias.
        session
            .ui_references
            .register_records(
                "memory_id",
                [crate::native_ids::stable(
                    crate::native_ids::Kind::Memory,
                    &"preferred-name",
                )]
                .iter()
                .map(String::as_str),
            )
            .unwrap();
        let scope = session.scope_snapshot.clone();
        let store = Store {
            stale: false,
            current_access: None,
            commits: Cell::new(0),
            saves: Cell::new(0),
            stored: RefCell::new(session),
        };
        let model = FollowupModel {
            history,
            calls: Cell::new(0),
            fail_reply,
            unavailable_source,
        };
        let clock = || "1970-01-01T00:00:01Z".into();
        let mut deps = deps(&store, &clock);
        deps.model = &model;
        let params = ClaimTurnParams {
            conversation_id: "root".into(),
            actor_id: "owner".into(),
            device_id: "device".into(),
            policy_revision: 1,
            current_pdp_scope: scope,
            turn_id: "turn".into(),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::User,
            now: clock(),
        };
        let mut sink = Sink::default();
        let input = crate::model_message_labels::model_bound_user_message(
            "owner-input".into(),
            if history {
                "What did I previously ask you to call me?"
            } else {
                "Forget both memories and tell me the result."
            }
            .into(),
            model.model_egress_policy().unwrap().unwrap().destination,
        )
        .unwrap();
        let result = run_agent_turn(&deps, params, input, &mut sink).await;
        assert_eq!(model.calls.get(), 2, "{result:?}");
        assert_eq!(
            store.commits.get(),
            u32::from(!unavailable_source && !history)
        );
        let persisted = store.stored.borrow();
        assert_eq!(
            persisted
                .conversation
                .iter()
                .filter(|message| message.role == ChatRole::Tool
                    && message.tool_ok == Some(!unavailable_source))
                .count(),
            if unavailable_source || history { 1 } else { 2 }
        );
        assert!(persisted.unclosed_tool_call_ids().is_empty());
        if fail_reply {
            assert!(result.is_err());
            assert!(sink.answers.is_empty());
        } else {
            assert_eq!(
                result.unwrap(),
                LoopOutcome::Answered(
                    if history {
                        "Previously sss; currently yyy."
                    } else {
                        "The two memories were deleted."
                    }
                    .into()
                )
            );
            assert_eq!(
                sink.answers,
                [if history {
                    "Previously sss; currently yyy."
                } else {
                    "The two memories were deleted."
                }]
            );
            if history {
                assert_eq!(
                    persisted
                        .assistant_data
                        .as_ref()
                        .unwrap()
                        .retrieval_budget
                        .read_calls,
                    1
                );
            }
            assert_eq!(persisted.handled_input_seq, 1);
        }
    }
}

fn check_data_followup_stacks(test_name: &str, history: bool) {
    const STACK: &str = "LRD_TEST_FORGET_FOLLOWUP_STACK_BYTES";
    if let Some(bytes) = std::env::var_os(STACK) {
        let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
        assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
        std::thread::Builder::new()
            .name("memory-data-followup-stack".into())
            .stack_size(bytes)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(exercise_data_followup(history));
            })
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--nocapture", "--test-threads=1"])
            .env(STACK, bytes.to_string())
            .env_remove("RUST_MIN_STACK")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "data tool followup failed on {bytes} bytes"
        );
    }
}

#[test]
fn successful_forget_returns_to_the_model_and_reply_failure_keeps_the_committed_results() {
    check_data_followup_stacks(
        "successful_forget_returns_to_the_model_and_reply_failure_keeps_the_committed_results",
        false,
    );
}

#[test]
fn memory_history_read_returns_to_the_model_and_reply_failure_keeps_the_read_result() {
    check_data_followup_stacks(
        "memory_history_read_returns_to_the_model_and_reply_failure_keeps_the_read_result",
        true,
    );
}

/// The alias the session issued for a memory the model saw earlier.
fn shown_memory_alias(request: &crate::seam::ModelRequest, native: &str) -> String {
    let alias = request
        .ui_references
        .as_ref()
        .expect("request alias table")
        .record_aliases("memory_id", std::iter::once(native))
        .pop_first()
        .expect("registered memory alias");
    assert!(alias.starts_with("mem") && alias != native, "{alias}");
    alias
}
