use super::*;
use crate::{
    ai_assistant::{
        current_memory::CurrentMemoryPayload,
        data_access::AssistantDataToolAccess,
        output_archive::ArchivedAssistantOutput,
        read_projection::CurrentMemoryPreparation,
        retained_copy::OwnerHistoryReadValidation,
        source_registry::{
            NativeInputPolicyGraph, NativeInputPolicyRegistration, SOURCE_CONTRACT_REVISION,
        },
    },
    chat::{ChatMessage, ChatRole, ModelTurn},
    model_egress::{
        ModelEgressPolicy, NativeModelOutputEvidence, NativeModelOutputSources,
        project_model_input_lineage,
    },
    prompt::ResponseFormatSpec,
    seam::ModelRequest,
};
use desk_agent_protocol::ai_assistant::memory::{
    MemoryRecord, MemoryScope, MemoryStatus, MemoryUsePurpose,
};

fn original() -> (
    PersistedAgentSession,
    ReadProjectionProof,
    AssistantDataExportAuthorization,
    ArchivedAssistantOutput,
) {
    original_with_turn(&ModelTurn {
        text: "An accepted answer using the original preference".into(),
        ..Default::default()
    })
}

fn original_with_turn(
    turn: &ModelTurn,
) -> (
    PersistedAgentSession,
    ReadProjectionProof,
    AssistantDataExportAuthorization,
    ArchivedAssistantOutput,
) {
    let (session, mut manifest, mut selected, call) = fixture();
    let access = AssistantDataToolAccess {
        settings: Default::default(),
        policy_revision: 1,
        control_generation: 1,
        memory_state_generation: 1,
    };
    let source_id = native_ids::fixture_source_id(&"owner-copy-evidence");
    let memory_id = native_ids::stable(Kind::Memory, &"owner-copy-memory");
    selected.usage.history_dependencies = vec![
        desk_agent_protocol::ai_assistant::source::HistoryDependency {
            source_id: source_id.clone(),
            source_revision: 1,
            origin_id: native_ids::fixture_origin_id(&"owner-copy-origin"),
            origin_deletion_generation: 1,
            policy_revision: 1,
            expires_at_unix_ms: Some(5000),
        },
    ];
    selected.usage.roots = vec![crate::ai_assistant::seam::AssistantReadRoot {
        source_id: source_id.clone(),
        source_revision: 1,
    }];
    selected.usage.memories = vec![crate::ai_assistant::data_control::CurrentMemoryUse {
        memory_id: memory_id.clone(),
        revision: 1,
        purpose: MemoryUsePurpose::CurrentFact,
    }];
    selected.usage.expires_at_unix_ms = Some(5000);
    let payload = CurrentMemoryPayload::new(
        1,
        vec![MemoryRecord {
            memory_id,
            revision: 1,
            scope: MemoryScope::Personal,
            source_device: None,

            content: "Prefer Chinese".into(),
            context: Some("A stable owner preference".into()),
            status: MemoryStatus::Active,
            source_ids: vec![source_id],
            observed_at_unix_ms: 100,
            updated_at_unix_ms: 100,
            valid_from_unix_ms: 100,
            valid_until_unix_ms: None,
        }],
    );
    let content = payload.encode(8192, 1000).unwrap().unwrap();
    let proof = ReadProjectionProof::for_current_memories(
        CurrentMemoryPreparation {
            session: &session,
            access: &access,
            destination: &call.destination,
            preparation_id: "owner-copy-preparation",
            maximum_bytes: 8192,
            authority_deadline: 10_000,
            now: 1000,
        },
        &content,
        &[selected.usage],
    )
    .unwrap();
    let mut message = ChatMessage::text("current-memory-input", ChatRole::User, content);
    message.data_envelope = Some(proof.source_envelope.clone());
    let egress = ModelEgressPolicy {
        destination: call.destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: call.model_export_authorization_id.clone(),
        now_unix_ms: 1001,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let mut raw = ModelRequest::text_only(vec![message], ResponseFormatSpec::None);
    raw.context_conversation_id = Some("root".into());
    let request = egress.authorize_request(raw).unwrap();
    let input_snapshot = request.input_snapshot().unwrap();
    let inputs = project_model_input_lineage(&request.audit, &request.input_envelopes).unwrap();
    manifest.read_projection_ids = vec![proof.usage.projection_id.clone()];
    manifest.current_memories = proof.usage.memories.clone();
    manifest.selected_envelopes = vec![SelectedAssistantEnvelope {
        envelope_id: proof.source_envelope.envelope_id.clone(),
    }];
    let grant = AssistantDataExportAuthorization::for_projection(
        &session,
        &manifest,
        &proof,
        call.clone(),
        None,
        1001,
    )
    .unwrap();
    let source_set = AssistantNativeSourceSet::from_grants(std::slice::from_ref(&grant)).unwrap();
    let output = egress
        .derive_model_output_envelope(turn, &request.input_envelopes)
        .unwrap();
    let evidence = NativeModelOutputEvidence::from_receipt(
        &call.physical_call_id,
        "root",
        &call.model_export_authorization_id,
        1001,
        1100,
        &request.audit,
        &inputs,
        &input_snapshot,
        &output,
    )
    .unwrap();
    let input_policies =
        NativeInputPolicyRegistration::capture(&input_snapshot, SOURCE_CONTRACT_REVISION, 1)
            .unwrap();
    let input_policy_graph = NativeInputPolicyGraph::empty("owner", "root").unwrap();
    let archive = ArchivedAssistantOutput {
        source_set,
        input_revision: 1,
        actor_input_seq: 1,
        input_recorded_at_unix_ms: 100,
        statement_timezone: "UTC".into(),
        sources: NativeModelOutputSources {
            evidence,
            inputs,
            input_snapshot,
        },
        input_policies,
        retained_policy: None,
        retained_roots: Vec::new(),
        input_policy_graph,
        output,
    };
    archive
        .validate_grants(std::slice::from_ref(&grant))
        .unwrap();
    (session, proof, grant, archive)
}

#[test]
fn owner_copy_retains_original_identity_without_new_consent_settings_or_model_authority() {
    let (mut session, proof, grant, archive) = original();
    let before = (proof.clone(), grant.clone(), archive.clone());
    let data = session.assistant_data.as_mut().unwrap();
    data.memory_access_scope = None;
    data.history_access_scope = None;
    data.memory_state_generation += 1;
    data.control_generation += 1;
    let view =
        OwnerHistoryReadValidation::for_consumed_projection(&archive, &grant, &proof, 1001, 1001)
            .unwrap();
    assert_eq!(view.actor_id(), "owner");
    assert_eq!(view.conversation_id(), "root");
    assert_eq!(view.source_device_ref(), "device");
    assert_eq!(view.input_envelope(), &proof.source_envelope);
    assert_eq!(view.usage(), &proof.usage);
    assert_eq!(view.usage().expires_at_unix_ms, Some(5000));
    // This is still the original CurrentFact declaration, never a rewritten
    // current-input projection that can be passed to model-purpose validators.
    assert_eq!(
        view.usage().memories[0].purpose,
        MemoryUsePurpose::CurrentFact
    );
    assert_eq!((proof, grant, archive), before);
}

#[test]
fn missing_membership_changed_projection_or_invalid_consumption_never_proves_an_owner_copy() {
    let (_, proof, grant, archive) = original();
    for case in 0..6 {
        let mut old = archive.clone();
        let mut read = proof.clone();
        let mut original_grant = grant.clone();
        let mut first = 1001;
        let mut last = 1001;
        match case {
            0 => old.source_set.authorizations.clear(),
            1 => read.usage.history_dependencies[0].source_revision += 1,
            2 => original_grant.actor_id = "another-owner".into(),
            3 => first = proof.created_at_unix_ms - 1,
            4 => first = grant.authorized_at_unix_ms + 1,
            _ => last = grant.authorized_at_unix_ms - 1,
        }
        assert!(
            OwnerHistoryReadValidation::for_consumed_projection(
                &old,
                &original_grant,
                &read,
                first,
                last
            )
            .is_err(),
            "case {case}"
        );
    }
}

#[test]
fn a_grant_sealed_for_the_same_sink_does_not_substitute_for_the_exact_original_input_label() {
    let (_, proof, grant, archive) = original();
    // Seal another valid proof with the same content digest and destination.
    // Its full label is absent from the actual immutable input snapshot.
    let mut other = proof.clone();
    let mut another_destination = other.source_envelope.allowed_destinations[0].clone();
    if let DestinationIdentity::Model {
        profile_revision, ..
    } = &mut another_destination
    {
        *profile_revision += 1;
    }
    other
        .source_envelope
        .allowed_destinations
        .push(another_destination);
    archive
        .validate_grants(std::slice::from_ref(&grant))
        .unwrap();
    assert!(
        OwnerHistoryReadValidation::for_consumed_projection(&archive, &grant, &other, 1001, 1001)
            .is_err()
    );
}

#[test]
fn a_tool_read_binds_the_actual_producer_result_label_and_original_consumed_call_without_its_body_cache()
 {
    use crate::{
        ai_assistant::{history::RetrievalCallKind, seam::AssistantDataCall},
        chat::ToolCall,
    };
    let tool = ToolCall {
        id: "query-call".into(),
        name: "search_memories".into(),
        arguments_json: r#"{"queries":["language"],"limit":5}"#.into(),
    };
    let turn = ModelTurn {
        text: "Look up the original preference".into(),
        tool_calls: vec![tool.clone()],
        ..Default::default()
    };
    let (mut session, _, _, parent) = original_with_turn(&turn);
    let mut producer = ChatMessage::text("query-producer", ChatRole::Assistant, turn.text.clone());
    producer.tool_calls = turn
        .tool_calls
        .iter()
        .map(|call| crate::chat::ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        })
        .collect();
    producer.data_envelope = Some(parent.output.clone());
    session.conversation.push(producer);
    let logical_id = native_ids::stable(
        Kind::AssistantReadAuthorization,
        &(
            &session.actor_id,
            &session.conversation_id,
            &session
                .assistant_data
                .as_ref()
                .unwrap()
                .retrieval_budget
                .episode_id,
            "query-producer",
            &tool.id,
        ),
    );
    let request_hash =
        crate::ai_assistant::read_projection::request_sha256(&tool.name, &tool.arguments_json)
            .unwrap();
    session
        .assistant_data
        .as_mut()
        .unwrap()
        .retrieval_budget
        .reserve(
            &logical_id,
            &request_hash,
            RetrievalCallKind::Relevance,
            4096,
            1200,
        )
        .unwrap();
    let reservation = session
        .assistant_data
        .as_ref()
        .unwrap()
        .retrieval_budget
        .reservations[0]
        .clone();
    let access = AssistantDataToolAccess {
        settings: Default::default(),
        policy_revision: 1,
        control_generation: 1,
        memory_state_generation: 1,
    };
    let content = serde_json::to_string(
        &desk_agent_protocol::ai_assistant::memory::MemorySearchResult {
            memories: Vec::new(),
            effective_queries: vec![vec!["language".into()]],
            has_more: false,
            next_cursor: None,
            memory_state_generation: 1,
            coverage: desk_agent_protocol::ai_assistant::history::AssistantHistoryCoverage {
                retained_from_unix_ms: None,
                retained_until_unix_ms: None,
                indexed_through_unix_ms: None,
                pending: 0,
                failed: 0,
                has_gaps: false,
                partial: false,
                reasons: Vec::new(),
            },
        },
    )
    .unwrap();
    let arguments = crate::ai_assistant::data_tools::parse(&tool).unwrap();
    let context = AssistantDataCall {
        session: &session,
        tool_call: &tool,
        arguments: &arguments,
        producer_message: session
            .conversation
            .iter()
            .find(|message| message.message_id == "query-producer")
            .unwrap(),
        logical_call_id: &logical_id,
        retrieval_reservation: Some(&reservation),
        producer_output_envelope: &parent.output,
        maximum_response_bytes: 4096,
        destination: &parent.source_set.destination,
        now_unix_ms: 1200,
    };
    let proof = ReadProjectionProof::for_retrieval(
        &context,
        &access,
        &content,
        SourceReadPurpose::ModelMemory,
        &[],
        1200,
    )
    .unwrap();
    let label = crate::model_message_labels::assistant_data_result_envelope(
        Some(&parent.output),
        std::slice::from_ref(&proof.source_envelope),
        &tool.id,
        &content,
        &tool.name,
        &proof.usage.retrieval_episode_id,
    )
    .unwrap()
    .unwrap();
    let mut result = ChatMessage::tool_result("query-result", &tool.id, content);
    result.data_envelope = Some(label.clone());
    session.conversation.push(result);
    session.assistant_data.as_mut().unwrap().read_uses = vec![proof.usage.reference()];
    let egress = ModelEgressPolicy {
        destination: parent.source_set.destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: "query-export".into(),
        now_unix_ms: 1201,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let mut raw = ModelRequest::text_only(session.conversation.clone(), ResponseFormatSpec::None);
    raw.context_conversation_id = Some("root".into());
    let request = egress.authorize_request(raw).unwrap();
    let manifest = AssistantModelDataManifest::for_direct_request(
        &session,
        &request.request,
        &access,
        &[&proof],
    )
    .unwrap();
    let inputs = project_model_input_lineage(&request.audit, &request.input_envelopes).unwrap();
    let snapshot = request.input_snapshot().unwrap();
    let call = NativeAssistantModelCall {
        physical_call_id: "query-physical".into(),
        model_export_authorization_id: "query-export".into(),
        destination: egress.destination.clone(),
        authority_expires_at_unix_ms: Some(5000),
    };
    let grant = AssistantDataExportAuthorization::for_projection(
        &session,
        &manifest,
        &proof,
        call.clone(),
        None,
        1201,
    )
    .unwrap();
    let set = AssistantNativeSourceSet::for_request(
        &session,
        &manifest,
        &call,
        std::slice::from_ref(&grant),
        1201,
    )
    .unwrap();
    let output = egress
        .derive_model_output_envelope(
            &ModelTurn {
                text: "The lookup returned no additional memories".into(),
                ..Default::default()
            },
            &request.input_envelopes,
        )
        .unwrap();
    let evidence = NativeModelOutputEvidence::from_receipt(
        "query-physical",
        "root",
        "query-export",
        1201,
        1250,
        &request.audit,
        &inputs,
        &snapshot,
        &output,
    )
    .unwrap();
    let input_policies =
        NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION, 1).unwrap();
    let graph = NativeInputPolicyGraph::from_archives(
        &snapshot,
        &input_policies,
        "owner",
        "root",
        &[&parent],
    )
    .unwrap();
    let archive = ArchivedAssistantOutput {
        source_set: set,
        input_revision: 1,
        actor_input_seq: 1,
        input_recorded_at_unix_ms: 100,
        statement_timezone: "UTC".into(),
        sources: NativeModelOutputSources {
            evidence,
            inputs,
            input_snapshot: snapshot,
        },
        input_policies,
        retained_policy: None,
        retained_roots: Vec::new(),
        input_policy_graph: graph,
        output,
    };
    let view =
        OwnerHistoryReadValidation::for_consumed_projection(&archive, &grant, &proof, 1201, 1201)
            .unwrap();
    assert_eq!(view.input_envelope(), &label);
    assert!(view.usage().roots.is_empty());
    // A different call with the same result digest and grant shape must not
    // acquire the old result's exact outer label or producer relationship.
    let mut other = proof;
    other.tool_call_id = Some("other-read-call".into());
    let changed_grant = grant;
    let changed_archive = archive;
    changed_archive
        .validate_grants(std::slice::from_ref(&changed_grant))
        .unwrap();
    assert!(
        OwnerHistoryReadValidation::for_consumed_projection(
            &changed_archive,
            &changed_grant,
            &other,
            1201,
            1201
        )
        .is_err()
    );
}
