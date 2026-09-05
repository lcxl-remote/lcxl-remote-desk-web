use super::*;

struct CompletionModel(ScriptModel);

#[async_trait(?Send)]
impl ModelSeam for CompletionModel {
    fn command_completion_event_id(&self) -> Option<&str> {
        Some("completed-1")
    }

    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        assert!(
            !requirements.image_input,
            "old screenshots are not completion inputs"
        );
        crate::model_context::PinnedContextPolicy::checkpoint_summary(
            SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test"),
            1,
            TEST_MODEL_CONTEXT_BYTES,
            1,
        )
        .map_err(model_context_error)
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        assert_eq!(
            request.use_case,
            crate::model_profile::ModelUseCase::Agent,
            "must never compress expired history to interpret a new result"
        );
        assert!(request.tools.is_empty());
        sink.on_text_delta("unvalidated provider bytes");
        self.0.call(request, sink).await
    }
}

#[tokio::test]
async fn exact_completion_bypasses_old_checkpoint_history_and_only_drains_seen_result() {
    check_completion_usage(false, 0).await;
    check_completion_usage(true, 0).await;
}

#[tokio::test]
async fn completion_rejects_text_invocations_and_retries_once_without_streaming_or_dispatch() {
    check_completion_usage(true, 1).await;
    check_completion_usage(true, 2).await;
}

struct NoProvisionalText;
impl TurnSink for NoProvisionalText {
    fn on_text_delta(&mut self, _: &str) {
        panic!("unvalidated completion must not stream");
    }
}

async fn check_completion_usage(with_usage: bool, invalid_count: usize) {
    let sess = MemSession::default();
    let mut session = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "now");
    session.surface = crate::session::AgentSessionSurface::DeviceAssistant;
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session.chain_id = "original-chain".into();
    session.response_locale = Some("zh-CN".into());
    let user = crate::model_message_labels::model_bound_user_message(
        "user".into(),
        "Find large directories".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "test".into(),
            connection_revision: 1,
            model_id: "test".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    session.conversation.push(ChatMessage::text(
        "old",
        ChatRole::Assistant,
        "expired".repeat(TEST_MODEL_CONTEXT_BYTES),
    ));
    session.conversation.push(user.clone());
    session.conversation[0].image_data_url = Some("data:image/png;base64,old-image".into());
    for index in [1, 2] {
        let event = format!("completed-{index}");
        let mut result = ChatMessage::untrusted_output(
            &event,
            &format!("call-{index}"),
            &format!("task-{index}"),
            "du completed",
        );
        result.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            user.data_envelope.as_ref(),
            &event,
            &result.text,
            "execute_confirmed_command",
        )
        .unwrap();
        session.conversation.push(result);
        session
            .pending_auto_triggers
            .push(crate::session::PendingWorkTrigger {
                work_id: index,
                kind: crate::session::WorkKind::AgentExec,
                execution_id: format!("generation-{index}"),
                tool_call_id: format!("call-{index}"),
                event_id: event,
                chain_id: session.chain_id.clone(),
                resolution_org_id: None,
                since: "now".into(),
            });
    }
    let original_context = session.model_context_state.clone();
    if with_usage {
        let policy = crate::model_context::PinnedContextPolicy::window(
            SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test"),
            1,
            TEST_MODEL_CONTEXT_BYTES,
        )
        .unwrap();
        session.context_usage_basis = Some(crate::context_usage::ContextUsageBasis::observe(
            &session.conversation[..2],
            &session.conversation[..2],
            &policy,
        ));
    }
    let original_usage = session.context_usage_basis.clone();
    let used_before = original_usage
        .as_ref()
        .map(|basis| basis.usage(&session.conversation).unwrap().used_bytes);
    *sess.inner.borrow_mut() = Some(session);
    let mut turns = std::collections::VecDeque::new();
    for _ in 0..invalid_count {
        turns.push_back(answer(
            "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"exec\">",
        ));
    }
    turns.push_back(answer("目录统计已完成"));
    let model = CompletionModel(ScriptModel {
        turns: RefCell::new(turns),
        requests: Rc::new(RefCell::new(vec![])),
    });
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-09-05T00:00:00Z".to_string();
    let mut completion_claim = claim();
    completion_claim.trigger_origin = crate::session::TriggerOrigin::WorkCompletion {
        kind: crate::session::WorkKind::AgentExec,
    };
    let outcome = resume_agent_turn(
        &deps(&sess, &model, &tools, &[], &clock),
        completion_claim,
        &mut NoProvisionalText,
    )
    .await;
    if invalid_count == 2 {
        assert!(
            outcome
                .unwrap_err()
                .message
                .contains("No additional command was executed")
        );
        assert_eq!(model.0.requests.borrow().len(), 2);
        assert!(tools.calls.borrow().is_empty());
        assert!(
            !sess
                .inner
                .borrow()
                .as_ref()
                .unwrap()
                .conversation
                .iter()
                .any(|m| m.text.contains("DSML"))
        );
        return;
    }
    assert_eq!(
        outcome.unwrap(),
        LoopOutcome::Answered("目录统计已完成".into())
    );
    let requests = model.0.requests.borrow();
    assert_eq!(requests.len(), invalid_count + 1);
    assert!(
        requests[0].messages[0]
            .text
            .contains("No tools are available")
    );
    if invalid_count == 1 {
        assert!(
            requests[1].messages[0]
                .text
                .contains("previous response was discarded")
        );
    }
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .filter(|message| message.role != ChatRole::System)
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "completed-1"]
    );
    assert!(requests[0].messages[0].text.contains("locale zh-CN"));
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.model_context_state, original_context);
    assert_eq!(stored.context_usage_basis, original_usage);
    assert!(stored.context_notices.is_empty());
    if let Some(used_before) = used_before {
        assert!(
            stored
                .context_usage_basis
                .as_ref()
                .unwrap()
                .usage(&stored.conversation)
                .unwrap()
                .used_bytes
                > used_before
        );
    }
    assert_eq!(stored.pending_auto_triggers.len(), 1);
    assert_eq!(stored.pending_auto_triggers[0].event_id, "completed-2");
    assert!(tools.calls.borrow().is_empty());
}
