use super::*;
use crate::{model_egress::ModelEgressPolicy, model_message_labels::model_bound_user_message};
use desk_agent_protocol::data_lineage::DestinationIdentity;

struct StrictCompressionModel {
    inner: CompressionScriptModel,
    policy: RefCell<ModelEgressPolicy>,
    expire_after_compression: bool,
}

#[async_trait(?Send)]
impl ModelSeam for StrictCompressionModel {
    fn model_egress_policy(&self) -> Result<Option<ModelEgressPolicy>, AgentError> {
        Ok(Some(self.policy.borrow().clone()))
    }

    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(requirements).await
    }

    fn context_compression_provenance(
        &self,
        turn_id: &str,
        created_at: &str,
    ) -> Result<crate::model_context::CompressorProvenanceV1, AgentError> {
        self.inner
            .context_compression_provenance(turn_id, created_at)
    }

    async fn audit_context_compression(
        &self,
        outcome: crate::seam::ContextCompressionAuditOutcome,
    ) {
        self.inner.audit_context_compression(outcome).await;
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        let is_compression =
            request.use_case == crate::model_profile::ModelUseCase::ContextCompression;
        let authorized = self.policy.borrow().authorize_request(request).unwrap();
        let mut turn = self.inner.call(authorized.request, sink).await?;
        turn.provider_meta.data_envelope = Some(
            self.policy
                .borrow()
                .derive_model_output_envelope(&turn, &authorized.input_envelopes)
                .unwrap(),
        );
        if is_compression && self.expire_after_compression {
            self.policy.borrow_mut().now_unix_ms += 400_000;
        }
        Ok(turn)
    }
}

#[tokio::test]
async fn strict_compression_loop_commits_only_authorized_unexpired_summaries() {
    for (unlabeled_history, expire_after_compression, expected_calls) in
        [(false, false, 2), (true, false, 0), (false, true, 1)]
    {
        let policy = ModelEgressPolicy {
            destination: DestinationIdentity::Model {
                connection_id: "test".into(),
                connection_revision: 1,
                model_id: "test".into(),
                profile_revision: 1,
            },
            selected_source_tools: Default::default(),
            export_authorization_id: "send".into(),
            now_unix_ms: 1000,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            omit_finite_retention_historical_turns: false,
        };
        let user = |id: &str, size: usize| {
            model_bound_user_message(id.into(), "x".repeat(size), policy.destination.clone())
                .unwrap()
        };
        let sess = MemSession::default();
        let mut existing = PersistedAgentSession::new(
            "conv",
            "actor",
            "device",
            1,
            scope(),
            "2026-06-19T00:00:00Z",
        );
        existing.conversation = vec![user("old-a", 5000), user("old-b", 6000)];
        if unlabeled_history {
            existing.conversation[0].data_envelope = None;
        }
        *sess.inner.borrow_mut() = Some(existing);
        let current = user("current", 7000);
        let requests = Rc::new(RefCell::new(Vec::new()));
        let audits = Rc::new(RefCell::new(Vec::new()));
        let mut compression_turn =
            answer(r#"{"goals":[{"text":"Earlier goal","source_message_ids":["old-a"]}]}"#);
        compression_turn.usage = crate::chat::TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        };
        let model = StrictCompressionModel {
            inner: CompressionScriptModel {
                turns: RefCell::new([Ok(compression_turn), Ok(answer("done"))].into()),
                requests: requests.clone(),
                audits: audits.clone(),
                source: SourceContextKey::derive(
                    WireProtocol::OpenAiChatCompletions,
                    "test",
                    "test",
                    "test",
                ),
                max_context_bytes: crate::MIN_MODEL_CONTEXT_BYTES * 4,
            },
            policy: RefCell::new(policy),
            expire_after_compression,
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
        let clock = || "2026-06-20T00:00:01Z".into();
        let loop_deps = deps(&sess, &model, &tools, &reg, &clock);
        let mut sink = Collector(Rc::new(RefCell::new(String::new())));
        let outcome = run_agent_turn(&loop_deps, claim(), current, &mut sink).await;
        assert_eq!(requests.borrow().len(), expected_calls);
        let stored = sess.inner.borrow();
        let stored = stored.as_ref().unwrap();
        if expected_calls == 2 {
            assert_eq!(outcome.unwrap(), LoopOutcome::Answered("done".into()));
            assert!(stored.model_context_state.entries.iter().any(|entry| {
                entry
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.v1().lineage.is_some())
            }));
            assert!(
                requests.borrow()[1]
                    .messages
                    .iter()
                    .any(|message| message.role == ChatRole::ContextSummary
                        && message.data_envelope.is_some())
            );
            assert_eq!(sink.0.borrow().as_str(), "done");
        } else {
            assert!(outcome.is_err());
            assert!(stored.model_context_state.entries.is_empty());
            assert!(sink.0.borrow().is_empty());
            assert_eq!(stored.current_turn_steps, 0);
            if expire_after_compression {
                assert_eq!(stored.current_turn_tokens.output_tokens, Some(5));
            }
        }
    }
}
