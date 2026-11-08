use super::*;
use crate::ai_assistant::data_tools;
use crate::{
    ai_assistant::test_support::personal_session,
    chat::{ChatMessage, ModelTurn, StopReason, ToolCall},
    model_egress::{ModelEgressPolicy, NativeModelOutputEvidence, project_model_input_lineage},
    model_message_labels::model_bound_user_message,
    sink_authorizer::{MAX_SINK_BYTES, SinkProjectionAudit},
};
use desk_agent_protocol::data_lineage::DestinationIdentity;

struct Fixture {
    session: PersistedAgentSession,
    call: ToolCall,
    arguments: crate::ai_assistant::data_tools::ParsedDataCall,
    parent: DataEnvelope,
    logical: String,
    destination: DestinationIdentity,
    sources: NativeModelOutputSources,
}
impl Fixture {
    fn new() -> Self {
        Self::with_call(ToolCall {
            id: "call".into(),
            name: data_tools::SEARCH_MEMORIES.into(),
            arguments_json: "{\"queries\":[\"language\"]}".into(),
        })
    }

    fn memory_write() -> Self {
        Self::with_call(ToolCall {
            id: "write".into(),
            name: data_tools::MANAGE_MEMORIES.into(),
            arguments_json: serde_json::json!({
                "operation": "remember", "content": "用户偏好中文回答",
                "source_refs": [{"kind": "input", "role": "evidence",
                    "id": native_ids::stable(Kind::AssistantSource, &"owner-input"), "revision": 1}]
            })
            .to_string(),
        })
    }

    fn with_call(call: ToolCall) -> Self {
        Self::with_calls(vec![call])
    }

    fn with_calls(calls: Vec<ToolCall>) -> Self {
        let call = calls[0].clone();
        let mut session = personal_session("owner", "root", "device");
        session.turn_state = TurnState::Running;
        session.input_revision = 1;
        session.lease_token = 1;
        session.current_turn_id = Some("turn".into());
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let input = model_bound_user_message(
            "owner-input".into(),
            "what do you remember".into(),
            destination.clone(),
        )
        .unwrap()
        .data_envelope
        .unwrap();
        let policy = ModelEgressPolicy {
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "export".into(),
            now_unix_ms: 900,
            byte_cap: MAX_SINK_BYTES,
            permission_resume: false,
        };
        let turn = ModelTurn {
            tool_calls: calls.clone(),
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        };
        let parent = policy
            .derive_model_output_envelope(&turn, std::slice::from_ref(&input))
            .unwrap();
        let audit = SinkProjectionAudit {
            destination: destination.clone(),
            envelope_ids: vec![input.envelope_id.clone()],
            total_bytes: 23,
        };
        let input_snapshot = crate::model_egress::NativeModelInputSnapshot::capture(
            &audit,
            std::slice::from_ref(&input),
        )
        .unwrap();
        let inputs = project_model_input_lineage(&audit, &[input]).unwrap();
        let evidence = NativeModelOutputEvidence::from_receipt(
            "physical",
            "root",
            "export",
            900,
            950,
            &audit,
            &inputs,
            &input_snapshot,
            &parent,
        )
        .unwrap();
        let sources = NativeModelOutputSources {
            evidence,
            inputs,
            input_snapshot,
        };
        let mut producer = ChatMessage::assistant_tool_calls(
            "producer",
            "",
            calls.iter().map(ToolCall::to_ref).collect(),
        );
        producer.data_envelope = Some(parent.clone());
        session.conversation.push(producer);
        let data = session.assistant_data.as_mut().unwrap();
        let logical = if call.name == data_tools::MANAGE_MEMORIES {
            native_ids::stable(
                Kind::AssistantReadAuthorization,
                &(&session.conversation_id, "producer", &call.id),
            )
        } else {
            let logical = native_ids::stable(
                Kind::AssistantReadAuthorization,
                &(
                    &session.actor_id,
                    &session.conversation_id,
                    &data.retrieval_budget.episode_id,
                    "producer",
                    &call.id,
                ),
            );
            data.retrieval_budget
                .reserve(
                    &logical,
                    &request_sha256(&call.name, &call.arguments_json).unwrap(),
                    RetrievalCallKind::Relevance,
                    16384,
                    1000,
                )
                .unwrap();
            logical
        };
        let arguments = crate::ai_assistant::data_tools::parse(&call).unwrap();
        Self {
            session,
            call,
            arguments,
            parent,
            logical,
            destination,
            sources,
        }
    }
    fn context(&self) -> AssistantDataCall<'_> {
        AssistantDataCall {
            session: &self.session,
            tool_call: &self.call,
            arguments: &self.arguments,
            producer_message: self
                .session
                .conversation
                .iter()
                .find(|message| message.message_id == "producer")
                .unwrap(),
            logical_call_id: &self.logical,
            retrieval_reservation: self
                .session
                .assistant_data
                .as_ref()
                .unwrap()
                .retrieval_budget
                .reservations
                .first(),
            producer_output_envelope: &self.parent,
            maximum_response_bytes: 16384,
            destination: &self.destination,
            now_unix_ms: 999,
        }
    }
}

#[test]
fn data_call_resolution_preserves_original_argument_bytes_and_rejects_changes() {
    let source = native_ids::stable(Kind::AssistantSource, &"observed-source");
    let cases = [
        (
            data_tools::SEARCH_MEMORIES,
            r#"{ "queries": ["language"], "limit": 5, "scope": "current_device" }"#.to_owned(),
        ),
        (
            data_tools::SEARCH_CONVERSATIONS,
            r#"{ "queries": ["report"], "limit": 5 }"#.to_owned(),
        ),
        (
            data_tools::READ_MEMORY_SOURCE,
            format!(r#"{{ "source_id": "{source}", "max_bytes": 4096 }}"#),
        ),
        (
            data_tools::READ_CONVERSATION,
            format!(r#"{{ "source_id": "{source}", "limit": 5 }}"#),
        ),
        (
            data_tools::MANAGE_MEMORIES,
            format!(
                r#"{{ "operation": "remember", "content": "Concise Chinese replies", "source_refs": [{{ "kind": "input", "id": "{source}", "revision": 1, "role": "evidence" }}] }}"#
            ),
        ),
    ];
    for (name, arguments_json) in cases {
        let fixture = Fixture::with_call(ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments_json,
        });
        data_tools::parse(&fixture.call).unwrap();
        let resolved = crate::ui_model_ids::resolve_session_call_with_context(
            &fixture.call,
            &fixture.session,
            999,
            None,
        )
        .unwrap();
        assert_eq!(
            resolved, fixture.call,
            "{name} must preserve the native model output"
        );
        let context = AssistantDataCall {
            tool_call: &resolved,
            ..fixture.context()
        };
        producer_on(&context, &fixture.session, 999).unwrap();

        let mut changed = resolved.clone();
        changed.arguments_json = changed
            .arguments_json
            .replace("5", "6")
            .replace("4096", "8192")
            .replace("Concise", "Detailed");
        assert_ne!(changed.arguments_json, resolved.arguments_json, "{name}");
        let context = AssistantDataCall {
            tool_call: &changed,
            ..fixture.context()
        };
        assert_eq!(
            producer_on(&context, &fixture.session, 999).unwrap_err(),
            "original assistant-data tool arguments changed"
        );
    }
}

#[test]
fn original_call_uses_the_durable_admission_time_and_does_not_require_the_earlier_dispatch_clock() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let original = OriginalAssistantDataCall::retrieval(&context, 1100).unwrap();
    assert_eq!(original.purpose(), SourceReadPurpose::ModelMemory);
    assert_eq!(
        original.unused_read_deadline().unwrap(),
        1000 + desk_agent_protocol::ai_assistant::limits::DEFAULT_UNUSED_PROJECTION_TTL_MS
    );
    assert!(OriginalAssistantDataCall::retrieval(&context, 999).is_err());
    assert!(
        OriginalAssistantDataCall::retrieval(&context, original.unused_read_deadline().unwrap())
            .is_err()
    );
    assert!(
        OriginalAssistantDataCall::retrieval(
            &context,
            original.unused_read_deadline().unwrap() + 1
        )
        .is_err()
    );
    OriginalAssistantDataCall::retrieval(&context, 1100)
        .unwrap()
        .persisted_producer(&context, &fixture.session, 1100)
        .unwrap();
    OriginalAssistantDataCall::retrieval(&context, 1100)
        .unwrap()
        .accepted_producer(&context, &fixture.sources, 1100)
        .unwrap();
}

#[test]
fn matching_assistant_state_cannot_hide_a_changed_persisted_producer_or_task() {
    let fixture = Fixture::new();
    let context = fixture.context();
    for variant in 0..8 {
        let mut changed = fixture.session.clone();
        match variant {
            0 => changed.conversation.clear(),
            1 => changed.conversation.push(changed.conversation[0].clone()),
            2 => changed.conversation[0].tool_calls[0].arguments_json = "{}".into(),
            3 => {
                changed.conversation[0]
                    .data_envelope
                    .as_mut()
                    .unwrap()
                    .envelope_id = "user-message-other".into()
            }
            4 => changed.trigger_origin = crate::session::TriggerOrigin::PermissionDecision,
            5 => changed.focus_epoch.input_revision += 1,
            6 => changed.current_turn_id = Some("other-turn".into()),
            _ => changed.turn_state = TurnState::Idle,
        }
        assert!(
            OriginalAssistantDataCall::retrieval(&context, 1100)
                .unwrap()
                .persisted_producer(&context, &changed, 1100)
                .is_err(),
            "variant {variant}"
        );
    }
}

#[test]
fn native_output_must_be_accepted_in_the_same_context_before_the_durable_read_reservation() {
    let fixture = Fixture::new();
    let context = fixture.context();
    for variant in 0..6 {
        let mut changed = fixture.sources.clone();
        match variant {
            0 => changed.evidence.context_conversation_id = "another-root".into(),
            1 => changed.evidence.completed_at_unix_ms = 1001,
            2 => changed.evidence.output_label_sha256 = "a".repeat(64),
            3 => changed.inputs.clear(),
            4 => {
                changed.evidence.destination = DestinationIdentity::Model {
                    connection_id: "gateway".into(),
                    connection_revision: 1,
                    model_id: "another-model".into(),
                    profile_revision: 1,
                }
            }
            _ => changed.evidence.output_envelope_id = "another-output".into(),
        }
        assert!(
            OriginalAssistantDataCall::retrieval(&context, 1100)
                .unwrap()
                .accepted_producer(&context, &changed, 1100)
                .is_err(),
            "variant {variant}"
        );
    }
}

#[test]
fn settled_or_unpersisted_reservations_and_mutated_arguments_cannot_start_another_read() {
    for variant in 0..4 {
        let mut fixture = Fixture::new();
        let persisted = fixture.session.clone();
        let original = OriginalAssistantDataCall::retrieval(&fixture.context(), 1100).unwrap();
        match variant {
            0 => {
                fixture
                    .session
                    .assistant_data
                    .as_mut()
                    .unwrap()
                    .retrieval_budget
                    .reservations[0]
                    .actual_bytes = Some(10)
            }
            1 => fixture.call.arguments_json = "{\"queries\":[\"changed\"]}".into(),
            2 => fixture.logical = "fabricated-logical-id".into(),
            _ => fixture.session.assistant_data.as_mut().unwrap().temporary = true,
        }
        assert!(
            original
                .persisted_producer(&fixture.context(), &persisted, 1100)
                .is_err(),
            "variant {variant}"
        );
    }
    let fixture = Fixture::new();
    let mut context = fixture.context();
    let mut reservation = context.retrieval_reservation.unwrap().clone();
    reservation.reserved_at_unix_ms = 1001;
    context.retrieval_reservation = Some(&reservation);
    assert!(
        OriginalAssistantDataCall::retrieval(&context, 1100)
            .unwrap()
            .persisted_producer(&context, &fixture.session, 1100)
            .is_err()
    );
}

#[test]
fn original_consent_deadlines_current_settings_and_reconciled_epochs_remain_independent_gates() {
    use crate::ai_assistant::data_access::AssistantDataToolAccess;
    use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettings;
    let mut fixture = Fixture::new();
    fixture
        .session
        .assistant_data
        .as_mut()
        .unwrap()
        .memory_access_scope
        .as_mut()
        .unwrap()
        .expires_at_unix_ms = Some(1200);
    let context = fixture.context();
    let original = OriginalAssistantDataCall::retrieval(&context, 1100).unwrap();
    let mut access = AssistantDataToolAccess {
        settings: AssistantDataSettings::default(),
        policy_revision: 1,
        memory_state_generation: 1,
        control_generation: 1,
    };
    assert_eq!(
        original.scope_deadline(&context, &access, 1100).unwrap(),
        Some(1200)
    );
    assert!(original.scope_deadline(&context, &access, 1200).is_err());
    access.settings.automatic_memory_enabled = false;
    assert!(original.scope_deadline(&context, &access, 1100).is_err());
    access.settings.automatic_memory_enabled = true;
    access.control_generation = 2;
    assert!(original.scope_deadline(&context, &access, 1100).is_err());
    access.control_generation = 1;
    access.memory_state_generation = 2;
    assert!(original.scope_deadline(&context, &access, 1100).is_err());
}

#[test]
fn memory_writes_keep_their_input_time_boundary_without_borrowing_a_read_reservation() {
    let mut fixture = Fixture::memory_write();
    let context = fixture.context();
    let original = OriginalAssistantDataCall::memory_write(&context, 1100).unwrap();
    assert!(original.unused_read_deadline().is_none());
    assert!(OriginalAssistantDataCall::retrieval(&context, 1100).is_err());
    original
        .accepted_producer(&context, &fixture.sources, 1100)
        .unwrap();
    assert!(
        original
            .accepted_producer(&context, &fixture.sources, 949)
            .is_err()
    );
    fixture
        .session
        .assistant_data
        .as_mut()
        .unwrap()
        .current_input_recorded_at_unix_ms = 901;
    assert!(
        original
            .accepted_producer(&fixture.context(), &fixture.sources, 1100)
            .is_err()
    );
}

#[test]
fn unified_admission_keeps_both_call_kinds_bound_to_their_original_request_and_session() {
    for mut fixture in [Fixture::new(), Fixture::memory_write()] {
        let context = fixture.context();
        let original = if context.retrieval_reservation.is_some() {
            OriginalAssistantDataCall::retrieval(&context, 1100)
        } else {
            OriginalAssistantDataCall::memory_write(&context, 1100)
        }
        .unwrap();
        original
            .persisted_producer(&context, &fixture.session, 1100)
            .unwrap();
        let mut changed = fixture.session.clone();
        changed.lease_token += 1;
        assert!(
            original
                .persisted_producer(&context, &changed, 1100)
                .is_err()
        );
        let mut sources = fixture.sources.clone();
        sources.evidence.context_conversation_id = "another-root".into();
        assert!(
            original
                .accepted_producer(&context, &sources, 1100)
                .is_err()
        );
        fixture.call.arguments_json = "{}".into();
        assert!(
            original
                .persisted_producer(&fixture.context(), &fixture.session, 1100)
                .is_err()
        );
    }
}

fn forget_call(label: &str, revision: u64) -> ToolCall {
    ToolCall { id: label.into(), name: data_tools::MANAGE_MEMORIES.into(), arguments_json: serde_json::json!({
        "operation":"forget", "memory_id":native_ids::stable(Kind::Memory, &label), "expected_revision":revision,
    }).to_string() }
}

#[test]
fn memory_control_binds_the_complete_original_batch_input_sequence_and_call_message() {
    let fixture = Fixture::with_calls(vec![forget_call("first", 1), forget_call("second", 2)]);
    let context = fixture.context();
    let targets = data_tools::forget_batch(
        &fixture.session.conversation[0]
            .tool_calls
            .iter()
            .map(|call| ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
            .collect::<Vec<_>>(),
    )
    .unwrap()
    .unwrap();
    let call = crate::ai_assistant::seam::MemoryForgetCall {
        producer_message_id: "producer",
        targets: &targets,
        operation_id: "control",
        destination: &fixture.destination,
        now_unix_ms: 1100,
    };
    let original = OriginalAssistantDataCall::memory_control(&context, &call).unwrap();
    assert!(original.requires_native_archive());
    assert!(!original.is_memory_write());
    assert!(original.unused_read_deadline().is_none());
    assert!(OriginalAssistantDataCall::retrieval(&context, 1100).is_err());
    let OriginalAssistantDataCall::MemoryControl(group) = &original else {
        unreachable!()
    };
    assert_eq!(group.targets().len(), 2);
    assert_eq!(group.targets()[1].expected_revision, 2);
    assert_eq!(
        group.owner_input_seq(),
        fixture
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .current_actor_input_seq
    );
    let message = fixture
        .session
        .conversation
        .iter()
        .find(|message| message.message_id == "producer")
        .unwrap();
    assert_eq!(
        group.call_message_sha256(),
        super::super::data_control::tool_group_sha256(std::slice::from_ref(message)).unwrap()
    );
    original
        .persisted_producer(&context, &fixture.session, 1100)
        .unwrap();
    original
        .accepted_producer(&context, &fixture.sources, 1100)
        .unwrap();
    let mut changed = fixture.session.clone();
    changed
        .assistant_data
        .as_mut()
        .unwrap()
        .current_actor_input_seq += 1;
    assert!(
        original
            .persisted_producer(&fixture.context(), &changed, 1100)
            .is_err()
    );
}

#[test]
fn model_boundary_rejects_mixed_and_duplicate_forget_batches() {
    for fixture in [Fixture::new(), Fixture::memory_write()] {
        assert!(data_tools::forget_batch(&[fixture.call]).unwrap().is_none());
    }
    let first = forget_call("first", 1);
    let read = ToolCall {
        id: "read".into(),
        name: data_tools::SEARCH_MEMORIES.into(),
        arguments_json: "{\"queries\":[\"language\"]}".into(),
    };
    let mut duplicate = first.clone();
    duplicate.id = "duplicate".into();
    for calls in [vec![first.clone(), read], vec![first, duplicate]] {
        assert!(data_tools::forget_batch(&calls).is_err());
    }
}
