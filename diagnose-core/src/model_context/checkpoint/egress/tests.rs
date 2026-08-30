use super::*;
use crate::{
    chat::{StopReason, ToolCallRef},
    model_message_labels::model_bound_user_message,
    model_profile::WireProtocol,
    replay::SourceContextKey,
};
use desk_agent_protocol::data_lineage::{DestinationIdentity, Sensitivity};

fn policy() -> ModelEgressPolicy {
    ModelEgressPolicy {
        destination: DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
        selected_source_tools: BTreeSet::from(["read_processes".into()]),
        export_authorization_id: "selected-send".into(),
        now_unix_ms: 1000,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        omit_finite_retention_historical_turns: false,
    }
}

fn context_policy() -> PinnedContextPolicy {
    PinnedContextPolicy::checkpoint_summary(
        SourceContextKey::derive(
            WireProtocol::OpenAiChatCompletions,
            "gateway",
            "model",
            "test",
        ),
        1,
        crate::MIN_MODEL_CONTEXT_BYTES * 4,
        1,
    )
    .unwrap()
}

fn user(id: &str, size: usize) -> ChatMessage {
    model_bound_user_message(id.into(), "x".repeat(size), policy().destination)
        .unwrap()
        .with_turn_id(format!("turn-{id}"))
}

fn history() -> Vec<ChatMessage> {
    vec![user("old", 9000), user("recent", 8000)]
}

fn plan(conversation: &[ChatMessage], state: &ModelContextState) -> CompressionPlan {
    match plan_model_context(
        conversation,
        state,
        &context_policy(),
        &ContextProtectionSet::default(),
        7,
    )
    .unwrap()
    {
        ContextBuildPlan::NeedsCompression(plan) => *plan,
        other => panic!("expected compression, got {other:?}"),
    }
}

fn provenance() -> CompressorProvenanceV1 {
    CompressorProvenanceV1 {
        source_context_key: context_policy().source_context_key.as_str().into(),
        provider_identity_sha256: "a".repeat(64),
        model_identity_sha256: "b".repeat(64),
        connection_revision: 1,
        model_profile_revision: 1,
        prompt_version: CONTEXT_SUMMARY_PROMPT_VERSION.into(),
        schema_version: CONTEXT_SUMMARY_SCHEMA_VERSION,
        provider_call_key: "c".repeat(64),
        created_at: "2026-08-30T00:00:00Z".into(),
        created_turn_id: "current".into(),
    }
}

fn response(input: &AuthorizedCompressionInput) -> ModelTurn {
    let mut turn = ModelTurn {
        text: r#"{ "goals": [{"text": "Earlier goal", "source_message_ids": ["old"]}] }"#.into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let projected = policy()
        .authorize_request(ModelRequest::text_only(
            input.messages.clone(),
            ResponseFormatSpec::None,
        ))
        .unwrap();
    turn.provider_meta.data_envelope = Some(
        policy()
            .derive_model_output_envelope(&turn, &projected.input_envelopes)
            .unwrap(),
    );
    turn
}

fn checkpoint(conversation: &[ChatMessage]) -> (ModelContextState, ModelContextView) {
    let plan = plan(conversation, &ModelContextState::default());
    let input = authorize_compression_input(&policy(), &plan, conversation).unwrap();
    let turn = response(&input);
    let mut validated = parse_validated_context_summary(&turn.text, &plan, provenance()).unwrap();
    bind_context_summary_lineage(&policy(), &mut validated, &turn, &input).unwrap();
    apply_validated_checkpoint(
        &plan,
        validated,
        conversation,
        &ModelContextState::default(),
        7,
    )
    .unwrap()
}

#[test]
fn compression_and_reloaded_checkpoint_keep_exact_lineage_and_lens_dependencies() {
    let conversation = history();
    let (state, view) = checkpoint(&conversation);
    let encoded = serde_json::to_string(&state).unwrap();
    assert!(!encoded.contains(&"x".repeat(100)));
    let restored: ModelContextState = serde_json::from_str(&encoded).unwrap();
    assert_eq!(restored, state);
    authorize_context_checkpoint(&policy(), &restored, &context_policy().key(), &conversation)
        .unwrap();
    let lineage = restored.entries[0]
        .checkpoint
        .as_ref()
        .unwrap()
        .v1()
        .lineage
        .as_ref()
        .unwrap();
    assert_eq!(
        lineage
            .sources
            .iter()
            .map(|source| source.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["old", "recent"]
    );
    let summary = &view.messages[0];
    assert_eq!(summary.role, ChatRole::ContextSummary);
    assert_eq!(summary.data_envelope.as_ref(), Some(&lineage.envelope));
    assert_eq!(
        lineage.envelope.digest_sha256,
        sha256_hex(summary.text.as_bytes())
    );
    assert_eq!(
        lineage.envelope.allowed_destinations,
        vec![policy().destination]
    );
    policy()
        .authorize_request(ModelRequest::text_only(
            view.messages,
            ResponseFormatSpec::None,
        ))
        .unwrap();
    let mut changed_lens = conversation;
    changed_lens[1] = user("recent", 7999);
    assert!(
        authorize_context_checkpoint(&policy(), &restored, &context_policy().key(), &changed_lens)
            .is_err()
    );
}

#[test]
fn required_inputs_cannot_be_pruned_rebound_or_mutated_before_compression() {
    for mutate in [
        |messages: &mut Vec<ChatMessage>| messages[0].data_envelope = None,
        |messages: &mut Vec<ChatMessage>| {
            messages[0]
                .data_envelope
                .as_mut()
                .unwrap()
                .allowed_destinations
                .clear()
        },
        |messages: &mut Vec<ChatMessage>| {
            messages[0]
                .data_envelope
                .as_mut()
                .unwrap()
                .retention
                .expires_at_unix_ms = Some(1000)
        },
        |messages: &mut Vec<ChatMessage>| {
            messages[0].data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Secret
        },
        |messages: &mut Vec<ChatMessage>| messages[1].data_envelope = None,
    ] {
        let mut conversation = history();
        mutate(&mut conversation);
        let plan = plan(&conversation, &ModelContextState::default());
        assert!(authorize_compression_input(&policy(), &plan, &conversation).is_err());
    }
    let mut conversation = history();
    let frozen = plan(&conversation, &ModelContextState::default());
    conversation[0].text.push('!');
    assert!(authorize_compression_input(&policy(), &frozen, &conversation).is_err());
    let conversation = history();
    let mut forged = plan(&conversation, &ModelContextState::default());
    forged.input.continuation_lens[0].text = "not the frozen lens".into();
    forged.input_canonical_json = canonical_json(&forged.input).unwrap();
    forged.input_projection_sha256 = sha256_hex(forged.input_canonical_json.as_bytes());
    assert!(authorize_compression_input(&policy(), &forged, &conversation).is_err());
}

#[test]
fn checkpoint_does_not_hide_deselected_tool_data() {
    let mut conversation = history();
    let mut assistant = ChatMessage::assistant_tool_calls(
        "call-message",
        "",
        vec![ToolCallRef {
            id: "read-1".into(),
            name: "read_processes".into(),
            arguments_json: "{}".into(),
        }],
    );
    // Label the exact assistant/tool bytes through the same model gate helpers.
    let mut tool = ChatMessage::tool_result("read-message", "read-1", "process observation");
    let mut envelope =
        model_bound_user_message("read-label".into(), tool.text.clone(), policy().destination)
            .unwrap()
            .data_envelope
            .unwrap();
    envelope.provenance.source_tool_name = "read_processes".into();
    envelope.allowed_destinations.clear();
    tool.data_envelope = Some(envelope);
    tool.turn_id = Some("turn-old".into());
    let mut output = ModelTurn {
        tool_calls: vec![crate::chat::ToolCall {
            id: "read-1".into(),
            name: "read_processes".into(),
            arguments_json: "{}".into(),
        }],
        ..Default::default()
    };
    output.provider_meta.data_envelope = Some(
        policy()
            .derive_model_output_envelope(
                &output,
                &[conversation[0].data_envelope.clone().unwrap()],
            )
            .unwrap(),
    );
    assistant.data_envelope = output.provider_meta.data_envelope;
    assistant.turn_id = Some("turn-old".into());
    conversation.insert(1, assistant);
    conversation.insert(2, tool);
    let (state, _) = checkpoint(&conversation);
    let mut deselected = policy();
    deselected.selected_source_tools.clear();
    assert!(
        authorize_context_checkpoint(&deselected, &state, &context_policy().key(), &conversation)
            .is_err()
    );
}

#[test]
fn canonical_summary_preserves_observation_expiry_even_without_retention_field() {
    let mut conversation = history();
    let envelope = conversation[0].data_envelope.as_mut().unwrap();
    envelope.sensitivity = Sensitivity::Sensitive;
    envelope.content = ContentRef::EphemeralObservation {
        observation_id: "old-observation".into(),
        size_bytes: 9000,
        expires_at_unix_ms: 5000,
    };
    let plan = plan(&conversation, &ModelContextState::default());
    let input = authorize_compression_input(&policy(), &plan, &conversation).unwrap();
    let turn = response(&input);
    let mut validated = parse_validated_context_summary(&turn.text, &plan, provenance()).unwrap();
    bind_context_summary_lineage(&policy(), &mut validated, &turn, &input).unwrap();
    let lineage = validated.lineage.as_ref().unwrap();
    assert_eq!(lineage.envelope.sensitivity, Sensitivity::Sensitive);
    assert_eq!(lineage.envelope.retention.expires_at_unix_ms, Some(5000));
    let mut expired = policy();
    expired.now_unix_ms = 5000;
    assert!(bind_context_summary_lineage(&expired, &mut validated, &turn, &input).is_err());
    let (state, _) = checkpoint(&conversation);
    assert!(
        authorize_context_checkpoint(&expired, &state, &context_policy().key(), &conversation)
            .is_err()
    );
}

#[test]
fn legacy_checkpoint_remains_readable_but_never_gains_strict_export_authority() {
    let conversation = history();
    let frozen = plan(&conversation, &ModelContextState::default());
    let input = authorize_compression_input(&policy(), &frozen, &conversation).unwrap();
    let turn = response(&input);
    let validated = parse_validated_context_summary(&turn.text, &frozen, provenance()).unwrap();
    let (state, _) = apply_validated_checkpoint(
        &frozen,
        validated,
        &conversation,
        &ModelContextState::default(),
        7,
    )
    .unwrap();
    assert!(matches!(
        plan_model_context(
            &conversation,
            &state,
            &context_policy(),
            &ContextProtectionSet::default(),
            8
        )
        .unwrap(),
        ContextBuildPlan::Ready(_)
    ));
    assert!(
        authorize_context_checkpoint(&policy(), &state, &context_policy().key(), &conversation)
            .is_err()
    );
    let encoded = serde_json::to_string(&state).unwrap();
    assert!(!encoded.contains("lineage"));
}

#[test]
fn summary_output_and_persisted_dependency_tampering_are_rejected() {
    let conversation = history();
    let frozen = plan(&conversation, &ModelContextState::default());
    let input = authorize_compression_input(&policy(), &frozen, &conversation).unwrap();
    let mut turn = response(&input);
    let mut validated = parse_validated_context_summary(&turn.text, &frozen, provenance()).unwrap();
    turn.text.push(' ');
    assert!(bind_context_summary_lineage(&policy(), &mut validated, &turn, &input).is_err());
    let turn = response(&input);
    bind_context_summary_lineage(&policy(), &mut validated, &turn, &input).unwrap();
    validated.lineage.as_mut().unwrap().sources.pop();
    assert!(
        apply_validated_checkpoint(
            &frozen,
            validated,
            &conversation,
            &ModelContextState::default(),
            7
        )
        .is_err()
    );
    let (mut state, _) = checkpoint(&conversation);
    let Some(ContextCheckpoint::V1(checkpoint)) = state.entries[0].checkpoint.as_mut() else {
        unreachable!()
    };
    checkpoint.summary.goals[0].text = "modified after compression".into();
    assert!(
        authorize_context_checkpoint(&policy(), &state, &context_policy().key(), &conversation)
            .is_err()
    );
}

#[test]
fn another_compression_generation_keeps_prior_retention_and_all_dependencies() {
    let mut conversation = history();
    let (state, _) = checkpoint(&conversation);
    let previous_expiry = state.entries[0]
        .checkpoint
        .as_ref()
        .unwrap()
        .v1()
        .lineage
        .as_ref()
        .unwrap()
        .envelope
        .retention
        .expires_at_unix_ms;
    conversation.push(user("next", 9000));
    let next_plan = plan(&conversation, &state);
    assert_eq!(next_plan.generation, 2);
    assert!(next_plan.input.prior_checkpoint.is_some());
    let input = authorize_compression_input(&policy(), &next_plan, &conversation).unwrap();
    let turn = response(&input);
    let mut validated =
        parse_validated_context_summary(&turn.text, &next_plan, provenance()).unwrap();
    bind_context_summary_lineage(&policy(), &mut validated, &turn, &input).unwrap();
    let (next_state, _) =
        apply_validated_checkpoint(&next_plan, validated, &conversation, &state, 7).unwrap();
    let lineage = next_state.entries[0]
        .checkpoint
        .as_ref()
        .unwrap()
        .v1()
        .lineage
        .as_ref()
        .unwrap();
    assert_eq!(
        lineage.envelope.retention.expires_at_unix_ms,
        previous_expiry
    );
    assert_eq!(
        lineage
            .sources
            .iter()
            .map(|source| source.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["next", "old", "recent"]
    );
    authorize_context_checkpoint(
        &policy(),
        &next_state,
        &context_policy().key(),
        &conversation,
    )
    .unwrap();
}

#[test]
fn restored_checkpoint_rejects_removed_dependencies_and_changed_model() {
    let conversation = history();
    let (state, _) = checkpoint(&conversation);
    let mut changed_model = policy();
    changed_model.destination = DestinationIdentity::Model {
        connection_id: "another-gateway".into(),
        connection_revision: 1,
        model_id: "another-model".into(),
        profile_revision: 1,
    };
    assert!(
        authorize_context_checkpoint(
            &changed_model,
            &state,
            &context_policy().key(),
            &conversation
        )
        .is_err()
    );
    let mut corrupted = state;
    let Some(ContextCheckpoint::V1(checkpoint)) = corrupted.entries[0].checkpoint.as_mut() else {
        unreachable!()
    };
    checkpoint.lineage.as_mut().unwrap().sources.pop();
    assert!(
        authorize_context_checkpoint(
            &policy(),
            &corrupted,
            &context_policy().key(),
            &conversation
        )
        .is_err()
    );
}
