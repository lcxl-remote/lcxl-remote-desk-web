use super::*;
use crate::model_preparation::ModelRequestPreparation;

struct RebuildingModel<'a> {
    requests: RefCell<Vec<ModelRequest>>,
    main_steps: std::cell::Cell<usize>,
    preparation_calls: std::cell::Cell<usize>,
    maximum: usize,
    reject_summary_candidate: bool,
    policy: Option<crate::model_egress::ModelEgressPolicy>,
    action: Option<ToolCall>,
    during_summary: Option<&'a dyn Fn()>,
}

struct AuditedGrantTools {
    inner: super::grant_exposure::GrantTools,
    label: desk_agent_protocol::data_lineage::DataEnvelope,
}
impl std::ops::Deref for AuditedGrantTools {
    type Target = super::grant_exposure::GrantTools;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
#[async_trait(?Send)]
impl ToolSeam for AuditedGrantTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.inner.run_read(call).await
    }

    async fn current_grant_disclosure(
        &self,
    ) -> Result<Option<crate::grant_disclosure::GrantDisclosureSnapshot>, AgentError> {
        self.inner.current_grant_disclosure().await
    }
    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        context: &crate::seam::ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        self.inner.confirm_and_exec(call, context).await
    }
    fn mutating_data_envelope(
        &self,
        call: &ToolCall,
        output: &ToolRunOutput,
    ) -> Result<Option<desk_agent_protocol::data_lineage::DataEnvelope>, AgentError> {
        crate::model_message_labels::internal_tool_result_envelope(
            Some(&self.label),
            &call.id,
            &output.content,
            &call.name,
        )
    }
}

impl RebuildingModel<'_> {
    fn body(&self, request: &ModelRequest) -> serde_json::Value {
        serde_json::json!({"protocol_framing": if self.policy.is_some() && request.use_case == crate::model_profile::ModelUseCase::Agent { "p".repeat(5000) } else { String::new() }, "messages": request.messages.iter().map(|message| serde_json::json!({
            "role": message.role.as_str(), "content": message.text, "tool_calls": message.tool_calls,
            "tool_call_id": message.tool_call_id,
        })).collect::<Vec<_>>(), "tools": request.tools, "max_tokens": request.caller_output_hard_cap.unwrap_or(65536)})
    }
}

#[async_trait(?Send)]
impl ModelSeam for RebuildingModel<'_> {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        Ok(request.caller_output_hard_cap.unwrap_or(65536).min(65536))
    }
    fn prepare_request(
        &self,
        request: &ModelRequest,
    ) -> Result<Option<ModelRequestPreparation>, AgentError> {
        let count = self.preparation_calls.get();
        self.preparation_calls.set(count + 1);
        assert!(
            count < 80,
            "preparation repeated without progress: {:?} {:?}",
            request.use_case,
            request
                .messages
                .iter()
                .map(|message| (&message.message_id, message.text.len()))
                .collect::<Vec<_>>()
        );
        let authorized = self
            .policy
            .as_ref()
            .map(|policy| policy.authorize_request(request.clone()))
            .transpose()
            .map_err(|error| error.agent_error())?;
        let request = authorized
            .as_ref()
            .map_or(request, |authorized| &authorized.request);
        let mut body = self.body(request);
        if self.reject_summary_candidate
            && request.use_case == crate::model_profile::ModelUseCase::Agent
            && request
                .messages
                .iter()
                .any(|message| message.role == ChatRole::ContextSummary)
        {
            body["unavoidable_protocol_overhead"] = "x".repeat(self.maximum).into();
        }
        ModelRequestPreparation::measure(&body, request, self.maximum).map(Some)
    }
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(self.policy.clone())
    }
    async fn context_policy(
        &self,
        _: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        crate::model_context::PinnedContextPolicy::checkpoint_summary(
            SourceContextKey::derive(
                WireProtocol::OpenAiChatCompletions,
                "context-boundary",
                "model",
                "1",
            ),
            1,
            self.maximum,
            1,
        )
        .map_err(model_context_error)
    }
    fn context_compression_provenance(
        &self,
        turn_id: &str,
        created_at: &str,
    ) -> Result<crate::model_context::CompressorProvenanceV1, AgentError> {
        let policy = crate::model_context::PinnedContextPolicy::checkpoint_summary(
            SourceContextKey::derive(
                WireProtocol::OpenAiChatCompletions,
                "context-boundary",
                "model",
                "1",
            ),
            1,
            self.maximum,
            1,
        )
        .unwrap();
        Ok(crate::model_context::CompressorProvenanceV1::for_call(
            &policy,
            "a".repeat(64),
            "b".repeat(64),
            1,
            format!("{:064x}", self.requests.borrow().len()),
            created_at,
            turn_id,
        ))
    }
    async fn call(
        &self,
        request: ModelRequest,
        _: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        let authorized = self
            .policy
            .as_ref()
            .map(|policy| policy.authorize_request(request.clone()))
            .transpose()
            .map_err(|error| error.agent_error())?;
        let request = authorized
            .as_ref()
            .map_or(request.clone(), |authorized| authorized.request.clone());
        let compression =
            request.use_case == crate::model_profile::ModelUseCase::ContextCompression;
        let mut reply = if compression {
            if let Some(hook) = self.during_summary {
                hook();
            }
            let input: serde_json::Value =
                serde_json::from_str(&request.messages.last().unwrap().text).unwrap();
            let source = input["summarize_only_range"]["allowed_source_message_ids"][0]
                .as_str()
                .unwrap();
            assert!(request.compression_identity.is_some());
            answer(&serde_json::json!({"goals":[{"text":"Continue the existing requested task","source_message_ids":[source]}]}).to_string())
        } else {
            ModelRequestPreparation::verify_send(&self.body(&request), &request, self.maximum)?;
            let step = self.main_steps.get();
            self.main_steps.set(step + 1);
            if let Some(action) = &self.action {
                if step == 0 {
                    tool_use_args(&action.id, &action.name, &action.arguments_json)
                } else {
                    answer("done")
                }
            } else if step < 2 {
                tool_use(&format!("bulk-{step}"), "bulk_read")
            } else {
                answer("done")
            }
        };
        reply.provider_meta.replay = Some(crate::replay::ReplayDisposition::NotRequired {
            source_context_key: SourceContextKey::derive(
                WireProtocol::OpenAiChatCompletions,
                "context-boundary",
                "model",
                "1",
            ),
        });
        if let Some(authorized) = &authorized {
            reply.provider_meta.data_envelope = Some(
                self.policy
                    .as_ref()
                    .unwrap()
                    .derive_model_output_envelope(&reply, &authorized.input_envelopes)
                    .map_err(|error| error.agent_error())?,
            );
        }
        self.requests.borrow_mut().push(request);
        Ok(reply)
    }
}

fn seeded_history() -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    session.conversation.push(ChatMessage::text(
        "original-owner",
        ChatRole::User,
        "Complete the existing task without repeating completed operations",
    ));
    session.conversation.push(ChatMessage::text(
        "consumed-old-answer",
        ChatRole::Assistant,
        "old fact ".repeat(950),
    ));
    session
}

#[tokio::test]
async fn one_long_turn_can_compress_two_new_prefixes_without_replaying_tools() {
    let sess = MemSession::default();
    *sess.inner.borrow_mut() = Some(seeded_history());
    let model = RebuildingModel {
        requests: RefCell::new(vec![]),
        main_steps: std::cell::Cell::new(0),
        preparation_calls: std::cell::Cell::new(0),
        maximum: 16384,
        reject_summary_candidate: false,
        policy: None,
        action: None,
        during_summary: None,
    };
    let calls = Rc::new(RefCell::new(vec![]));
    let tools = RecordingTools {
        calls: calls.clone(),
        reply: "x".repeat(10000),
    };
    let registry = vec![read_tool("bulk_read", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut dependencies = deps(&sess, &model, &tools, &registry, &clock);
    dependencies.system_prompt = ChatMessage::text(
        "system",
        ChatRole::System,
        "Follow the existing task and report actual results",
    );
    let outcome = run_agent_turn(
        &dependencies,
        claim(),
        ChatMessage::text("continue", ChatRole::User, "continue"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(calls.borrow().as_slice(), ["bulk_read", "bulk_read"]);
    let requests = model.requests.borrow();
    let summaries = requests
        .iter()
        .filter(|request| {
            request.use_case == crate::model_profile::ModelUseCase::ContextCompression
        })
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 2);
    assert_ne!(
        summaries[0].compression_identity,
        summaries[1].compression_identity
    );
    for request in requests
        .iter()
        .filter(|request| request.use_case == crate::model_profile::ModelUseCase::Agent)
    {
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| message.message_id == "original-owner")
                .count(),
            1
        );
        request.preparation.as_ref().unwrap().require_fit().unwrap();
    }
}

#[tokio::test]
async fn an_unsendable_compression_candidate_never_advances_the_durable_floor() {
    let sess = MemSession::default();
    let initial = seeded_history();
    let original_state = initial.model_context_state.clone();
    *sess.inner.borrow_mut() = Some(initial);
    let model = RebuildingModel {
        requests: RefCell::new(vec![]),
        main_steps: std::cell::Cell::new(0),
        preparation_calls: std::cell::Cell::new(0),
        maximum: 16384,
        reject_summary_candidate: true,
        policy: None,
        action: None,
        during_summary: None,
    };
    let calls = Rc::new(RefCell::new(vec![]));
    let tools = RecordingTools {
        calls: calls.clone(),
        reply: "x".repeat(10000),
    };
    let registry = vec![read_tool("bulk_read", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut dependencies = deps(&sess, &model, &tools, &registry, &clock);
    dependencies.system_prompt = ChatMessage::text("system", ChatRole::System, "Follow the task");
    assert!(
        run_agent_turn(
            &dependencies,
            claim(),
            ChatMessage::text("continue", ChatRole::User, "continue"),
            &mut NullTurnSink
        )
        .await
        .is_err()
    );
    let stored = sess.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(
        stored
            .model_context_state
            .entries
            .iter()
            .filter(|entry| entry.checkpoint.is_some())
            .count(),
        original_state
            .entries
            .iter()
            .filter(|entry| entry.checkpoint.is_some())
            .count()
    );
    assert_eq!(calls.borrow().len(), 1);
    assert!(
        stored
            .conversation
            .iter()
            .any(|message| message.message_id == "consumed-old-answer")
    );
}

#[tokio::test]
async fn approval_recovery_can_compress_without_reasking_or_losing_exact_arguments() {
    run_permission_compaction_case(0).await;
}

#[tokio::test]
async fn revocation_during_compression_does_not_publish_the_stale_candidate_or_execute() {
    run_permission_compaction_case(1).await;
}

#[tokio::test]
async fn expiry_during_compression_keeps_the_original_floor_and_does_not_execute() {
    run_permission_compaction_case(2).await;
}

#[tokio::test]
async fn new_owner_input_during_compression_cannot_be_overwritten_by_the_old_candidate() {
    run_permission_compaction_case(3).await;
}

async fn run_permission_compaction_case(mutation: u8) {
    use crate::dynamic_run::{
        GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequest,
        PermissionRequestState,
    };
    use desk_agent_protocol::capability_grant::{CapabilityGrantUsePolicy, CapabilityRiskTier};
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    };
    use sha2::{Digest, Sha256};
    let (mut initial, providers, mut snapshot) = crate::grant_disclosure::tests::fixture();
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    let descriptor = providers.capability_for_tool("launch_application").unwrap();
    let registry = vec![descriptor.registered_tool()];
    let mut scope = initial.scope_snapshot.clone();
    scope.mode = ExecutionMode::ConfirmEachAction;
    crate::tool_exposure::extend_candidate_scope(
        &providers,
        &registry,
        &[],
        &[],
        &mut scope.granted,
    );
    initial.scope_snapshot = scope.clone();
    let exact = r#"{"args":[],"cwd":null,"run_as_admin":false,"target":{"kind":"executable","value":"C:\\app.exe"}}"#;
    let digest = format!("{:x}", Sha256::digest(exact.as_bytes()));
    let grant = &mut snapshot.grants[0];
    grant.provider_id = providers
        .provider_for_capability(&descriptor.wire.capability_id)
        .unwrap()
        .wire
        .provider_id
        .clone();
    grant.capability_id = descriptor.wire.capability_id.clone();
    grant.tool_name = "launch_application".into();
    grant.effect = descriptor.wire.effect;
    grant.risk_tier = CapabilityRiskTier::R3;
    grant.use_policy = CapabilityGrantUsePolicy::OneShotExact;
    grant.canonical_input_digest_sha256 = Some(digest.clone());
    grant.resource_scope = vec![format!("application_input:sha256:{}", "a".repeat(64))];
    grant.operation_scope = vec!["launch_application".into()];
    let original_expiry = grant.expires_at_unix_ms;
    initial.permission_requests.push(PermissionRequest {
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id: "approved-launch".into(),
        input_revision: 1,
        state: PermissionRequestState::Approved,
        items: vec![GrantRequestItem {
            command_confirmation: None,
            launch_confirmation: None,
            item_id: "launch".into(),
            provider_id: grant.provider_id.clone(),
            tool_name: grant.tool_name.clone(),
            expected_effect: grant.effect,
            resource_scope: grant.resource_scope.clone(),
            operation_scope: grant.operation_scope.clone(),
            export_destinations: vec![],
            canonical_input_json: Some(exact.into()),
            canonical_input_digest_sha256: Some(digest),
            suggested_ttl_seconds: 300,
            suggested_max_uses: 1,
            reason: "Open the requested app".into(),
        }],
        created_at: "1970-01-01T00:00:00.100Z".into(),
    });
    snapshot.ready_capabilities = vec![descriptor.required_capability];
    let destination = DestinationIdentity::Model {
        connection_id: "test".into(),
        connection_revision: 1,
        model_id: "test".into(),
        profile_revision: 1,
    };
    initial.conversation = vec![
        ChatMessage::text(
            "original-owner",
            ChatRole::User,
            "Open the approved app and continue the original task",
        ),
        ChatMessage::text(
            "consumed-history",
            ChatRole::Assistant,
            "old fact ".repeat(1120),
        ),
    ];
    for message in &mut initial.conversation {
        let bytes = crate::model_egress::message_content_bytes(message).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        message.data_envelope = Some(DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("source-{}", message.message_id),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("blob-{}", message.message_id),
                sha256: digest.clone(),
                size_bytes: bytes.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "test-runtime".into(),
                source_tool_name: "source".into(),
                source_object_id: Some(message.message_id.clone()),
                source_envelope_ids: vec![],
            },
            digest_sha256: digest,
            sensitivity: Sensitivity::Public,
            allowed_destinations: vec![destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        });
    }
    let policy = crate::model_egress::ModelEgressPolicy {
        destination,
        selected_source_tools: Default::default(),
        export_authorization_id: "test-export".into(),
        now_unix_ms: 500,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: true,
    };
    let bridge = crate::permission_resume::authorized_permission_resume_message(
        "permission-resume".into(),
        &policy,
        &initial.conversation[0],
    )
    .unwrap();
    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let tools = AuditedGrantTools {
        label: session.inner.borrow().as_ref().unwrap().conversation[0]
            .data_envelope
            .clone()
            .unwrap(),
        inner: super::grant_exposure::GrantTools {
            snapshot: RefCell::new(snapshot),
            reads: std::cell::Cell::new(0),
        },
    };
    let expired = std::cell::Cell::new(false);
    let change_during_summary = || match mutation {
        1 => {
            let mut state = tools.snapshot.borrow_mut();
            state.grants[0].revoked_at_unix_ms = Some(500);
            state.grants[0].revoked_reason = Some("owner".into());
        }
        2 => expired.set(true),
        3 => *session.latest_revision.borrow_mut() = Some(2),
        _ => {}
    };
    let model = RebuildingModel {
        requests: RefCell::new(vec![]),
        main_steps: std::cell::Cell::new(0),
        preparation_calls: std::cell::Cell::new(0),
        maximum: 16384,
        reject_summary_candidate: false,
        policy: Some(policy),
        action: Some(ToolCall {
            id: "launch".into(),
            name: "launch_application".into(),
            arguments_json: exact.into(),
        }),
        during_summary: if mutation != 0 {
            Some(&change_during_summary)
        } else {
            None
        },
    };
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: tools.snapshot.borrow().grants[0].provider_id.clone(),
        capability_id: descriptor.wire.capability_id.clone(),
        tool_name: "launch_application".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let clock = || {
        if expired.get() {
            "2099-01-01T00:00:00Z"
        } else {
            "1970-01-01T00:00:00.500Z"
        }
        .to_string()
    };
    let exact_names = vec!["launch_application".into()];
    let mut dependencies = deps(&session, &model, &tools, &registry, &clock);
    dependencies.provider_registry = Some(&providers);
    dependencies.capability_inventory = Some(&inventory);
    dependencies.permission_continuation_exact_tools = &exact_names;
    dependencies.system_prompt = ChatMessage::text(
        "system",
        ChatRole::System,
        "Continue the authorized operation in dependency order",
    );
    let mut claim = exec_claim();
    claim.current_pdp_scope = scope;
    claim.trigger_origin = crate::session::TriggerOrigin::PermissionDecision;
    let outcome =
        resume_agent_turn_after_permission(&dependencies, claim, bridge, &mut NullTurnSink).await;
    if mutation != 0 {
        if mutation == 3 {
            assert!(matches!(outcome, Ok(LoopOutcome::Superseded { .. })));
        } else {
            assert!(outcome.is_err());
        }
        assert_eq!(tools.reads.get(), 0);
        assert!(
            session
                .inner
                .borrow()
                .as_ref()
                .unwrap()
                .model_context_state
                .entries
                .iter()
                .all(|entry| entry.checkpoint.is_none())
        );
        assert_eq!(
            model
                .requests
                .borrow()
                .iter()
                .filter(|request| request.use_case
                    == crate::model_profile::ModelUseCase::ContextCompression)
                .count(),
            1
        );
        assert_eq!(tools.snapshot.borrow().grants[0].remaining_uses, 1);
        return;
    }
    assert_eq!(outcome.unwrap(), LoopOutcome::Answered("done".into()));
    assert_eq!(tools.reads.get(), 1);
    assert_eq!(
        tools.snapshot.borrow().grants[0].expires_at_unix_ms,
        original_expiry
    );
    assert_eq!(tools.snapshot.borrow().grants[0].remaining_uses, 0);
    let requests = model.requests.borrow();
    assert!(
        requests
            .iter()
            .any(|request| request.use_case
                == crate::model_profile::ModelUseCase::ContextCompression)
    );
    let first = requests
        .iter()
        .find(|request| request.use_case == crate::model_profile::ModelUseCase::Agent)
        .unwrap();
    assert!(
        first
            .messages
            .iter()
            .any(|message| message.text.contains("approved_exact_input"))
    );
    assert!(
        first
            .messages
            .iter()
            .any(|message| message.text.contains("PERMISSION CONTINUATION CHECKPOINT"))
    );
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.permission_requests.len(), 1);
    assert_eq!(stored.permission_requests[0].request_id, "approved-launch");
    let restored: serde_json::Value = serde_json::from_str(
        stored.permission_requests[0].items[0]
            .canonical_input_json
            .as_ref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        restored,
        serde_json::from_str::<serde_json::Value>(exact).unwrap()
    );
}

#[tokio::test]
async fn saving_a_new_permission_request_pauses_without_starting_another_model_or_summary() {
    let session = MemSession::default();
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
    *session.inner.borrow_mut() = Some(initial);
    let providers = crate::ai_assistant::ai_assistant_provider_registry();
    let descriptor = providers
        .capability_for_tool("inspect_desktop_session")
        .unwrap();
    let candidate = vec![descriptor.registered_tool()];
    let mut registry = candidate.clone();
    registry.extend(crate::permission_tools::permission_planning_tool_registry());
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: providers
            .provider_for_capability(&descriptor.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone(),
        capability_id: descriptor.wire.capability_id.clone(),
        tool_name: "inspect_desktop_session".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let model = RebuildingModel { requests: RefCell::new(vec![]), main_steps: std::cell::Cell::new(0), preparation_calls: std::cell::Cell::new(0), maximum: 16384, reject_summary_candidate: false,
        policy: None, action: Some(ToolCall { id: "request".into(), name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"items":[{"tool_name":"inspect_desktop_session","reason":"Observe the device for the current task","suggested_ttl_seconds":300,"suggested_max_uses":1}]}).to_string() }), during_summary: None };
    let calls = Rc::new(RefCell::new(vec![]));
    let tools = RecordingTools {
        calls: calls.clone(),
        reply: "must not run".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut dependencies = deps(&session, &model, &tools, &registry, &clock);
    dependencies.provider_registry = Some(&providers);
    dependencies.capability_inventory = Some(&inventory);
    dependencies.capability_permission_candidates = &candidate;
    dependencies.system_prompt = ChatMessage::text(
        "system",
        ChatRole::System,
        "Request required permission before reading the device",
    );
    let outcome = run_agent_turn(
        &dependencies,
        claim(),
        ChatMessage::text("owner", ChatRole::User, "Inspect this device"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::PermissionRequested { .. }));
    assert!(calls.borrow().is_empty());
    assert_eq!(model.requests.borrow().len(), 1);
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.permission_requests.len(), 1);
    assert_eq!(
        stored.permission_requests[0].state,
        crate::dynamic_run::PermissionRequestState::Pending
    );
    assert!(
        stored
            .model_context_state
            .entries
            .iter()
            .all(|entry| entry.checkpoint.is_none())
    );
}
