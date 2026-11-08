use super::*;

mod visible;
use crate::{
    ai_assistant::output_archive::ArchivedAssistantOutput,
    chat::{ModelTurn, StopReason},
    model_egress::{
        NativeModelInputSnapshot, NativeModelOutputEvidence, NativeModelOutputSources,
        project_model_input_lineage,
    },
};

fn native_archive(
    id: &str,
    messages: Vec<ChatMessage>,
    parents: &[&ArchivedAssistantOutput],
) -> ArchivedAssistantOutput {
    native_archive_with_turn(
        id,
        messages,
        parents,
        &ModelTurn {
            text: format!("answer-{id}"),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        },
        "UTC",
    )
}

fn native_archive_with_turn(
    id: &str,
    messages: Vec<ChatMessage>,
    parents: &[&ArchivedAssistantOutput],
    turn: &ModelTurn,
    timezone: &str,
) -> ArchivedAssistantOutput {
    native_archive_in_context(id, messages, parents, turn, timezone, "root", 1)
}

fn native_archive_in_context(
    id: &str,
    messages: Vec<ChatMessage>,
    parents: &[&ArchivedAssistantOutput],
    turn: &ModelTurn,
    timezone: &str,
    conversation: &str,
    actor_input_seq: u64,
) -> ArchivedAssistantOutput {
    use crate::ai_assistant::{
        data_access::AssistantDataToolAccess,
        native_export::{AssistantNativeSourceSet, NativeAssistantModelCall},
        seam::AssistantModelDataManifest,
        test_support::personal_session,
    };
    let policy = ModelEgressPolicy {
        destination: destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: format!("export-{id}"),
        now_unix_ms: 100,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let mut raw = ModelRequest::text_only(messages, ResponseFormatSpec::None);
    raw.context_conversation_id = Some(conversation.into());
    let request = policy.authorize_request(raw).unwrap();
    let output = policy
        .derive_model_output_envelope(turn, &request.input_envelopes)
        .unwrap();
    let input_snapshot = request.input_snapshot().unwrap();
    let inputs = project_model_input_lineage(&request.audit, &request.input_envelopes).unwrap();
    let evidence = NativeModelOutputEvidence::from_receipt(
        id,
        conversation,
        &policy.export_authorization_id,
        100,
        110,
        &request.audit,
        &inputs,
        &input_snapshot,
        &output,
    )
    .unwrap();
    let input_policies =
        NativeInputPolicyRegistration::capture(&input_snapshot, SOURCE_CONTRACT_REVISION, 1)
            .unwrap();
    let mut session = personal_session("owner", conversation, "device");
    session.lease_token = 1;
    session.input_revision = 1;
    session
        .assistant_data
        .as_mut()
        .unwrap()
        .current_input_timezone = timezone.into();
    if actor_input_seq == 0 {
        let data = session.assistant_data.as_mut().unwrap();
        data.current_actor_input_seq = 0;
        data.instructions.accepted_input_seq = 0;
        data.history_access_scope = None;
        data.memory_access_scope = None;
        data.retrieval_task_id = Some(conversation.into());
    }
    let access = AssistantDataToolAccess {
        settings: Default::default(),
        policy_revision: 1,
        memory_state_generation: 1,
        control_generation: 1,
    };
    let manifest =
        AssistantModelDataManifest::for_direct_request(&session, &request.request, &access, &[])
            .unwrap();
    let call = NativeAssistantModelCall {
        physical_call_id: id.into(),
        model_export_authorization_id: policy.export_authorization_id.clone(),
        destination: policy.destination.clone(),
        input_lineage_sha256: crate::model_egress::model_input_lineage_sha256(&inputs).unwrap(),
        authority_expires_at_unix_ms: Some(1000),
    };
    let source_set =
        AssistantNativeSourceSet::for_request(&session, &manifest, &call, &[], 100).unwrap();
    let input_policy_graph = NativeInputPolicyGraph::from_archives(
        &input_snapshot,
        &input_policies,
        "owner",
        conversation,
        parents,
    )
    .unwrap();
    let mut value = ArchivedAssistantOutput {
        source_set,
        input_revision: 1,
        actor_input_seq,
        input_recorded_at_unix_ms: 100,
        statement_timezone: timezone.into(),
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
    value.retained_policy = value
        .input_policy_graph
        .complete_text_policy(&value.sources.input_snapshot, &value.input_policies)
        .unwrap();
    value.validate().unwrap();
    value
}

#[test]
fn native_task_output_preserves_absent_owner_input_without_granting_source_access() {
    let run = "schedule-run-native-output";
    let archive = native_archive_in_context(
        "native-task-call",
        vec![owner_message()],
        &[],
        &ModelTurn {
            text: "Scheduled report".into(),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        },
        "UTC",
        run,
        0,
    );
    archive.validate_grants(&[]).unwrap();
    assert!(archive.source_set.authorizations.is_empty());
    let parent = NativeDerivedInputPolicy::from_archive(&archive).unwrap();
    parent.validate_archive(&archive).unwrap();
    let policy = crate::ai_assistant::source::owner_text_policy(1, false);
    let mut capture = crate::ai_assistant::source::HistoryCapture {
        native: None,
        actor_id: "owner",
        device_ref: "device",
        directory: None,
        conversation_id: run,
        root_conversation_id: run,
        native_id: "task-answer",
        accepted_input_seq: 0,
        kind: desk_agent_protocol::ai_assistant::source::AssistantSourceKind::AssistantMessage,
        role: "assistant",
        content: "Scheduled report",
        envelope: &archive.output,
        source_policy: &policy,
        dependencies: &[],
        recorded_at_unix_ms: 110,
        statement_timezone: "UTC",
        revision: 1,
    };
    let retention =
        desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy::default();
    assert!(
        crate::ai_assistant::source::prepare_capture(&capture, &retention)
            .unwrap()
            .is_some()
    );
    capture.role = "user";
    assert!(crate::ai_assistant::source::prepare_capture(&capture, &retention).is_err());
    capture.role = "assistant";
    capture.conversation_id = "ordinary-conversation";
    assert!(crate::ai_assistant::source::prepare_capture(&capture, &retention).is_err());
    let mut wrong_parent = parent;
    wrong_parent.conversation_id = "ordinary-conversation".into();
    assert!(wrong_parent.validate().is_err());
    let mut changed = archive.clone();
    changed.source_set.context.task_id = None;
    assert!(changed.validate().is_err());
    let mut changed = archive.clone();
    changed.source_set.context.task_id = Some("schedule-run-other".into());
    assert!(changed.validate().is_err());
    let mut changed = archive;
    changed.actor_input_seq = 1;
    assert!(changed.validate().is_err());
}

fn owner_message() -> ChatMessage {
    model_bound_user_message(
        "owner-message".into(),
        "Use my usual environment".into(),
        destination(),
    )
    .unwrap()
}

fn answer(parent: &ArchivedAssistantOutput, id: &str) -> ChatMessage {
    let mut message = ChatMessage::text(
        format!("message-{id}"),
        ChatRole::Assistant,
        format!("answer-{id}"),
    );
    message.data_envelope = Some(parent.output.clone());
    message
}

fn root(
    parent: &ArchivedAssistantOutput,
    id: &str,
) -> (NativeModelInputSnapshot, NativeInputPolicyRegistration) {
    let authorized = ModelEgressPolicy {
        destination: destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "root-export".into(),
        now_unix_ms: 120,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    }
    .authorize_request(ModelRequest::text_only(
        vec![answer(parent, id)],
        ResponseFormatSpec::None,
    ))
    .unwrap();
    let snapshot = authorized.input_snapshot().unwrap();
    let registration =
        NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION, 1).unwrap();
    (snapshot, registration)
}

#[test]
fn a_native_chain_keeps_only_direct_parent_references_and_preserves_the_original_deadline() {
    let mut owner = owner_message();
    owner
        .data_envelope
        .as_mut()
        .unwrap()
        .retention
        .expires_at_unix_ms = Some(5000);
    let first = native_archive("first", vec![owner], &[]);
    let second = native_archive("second", vec![answer(&first, "first")], &[&first]);
    let third = native_archive("third", vec![answer(&second, "second")], &[&second]);
    assert_eq!(third.input_policy_graph.parents.len(), 1);
    let parent = &third.input_policy_graph.parents[0];
    assert_eq!(parent.physical_call_id, "second");
    parent.validate_archive(&second).unwrap();
    assert!(parent.validate_archive(&first).is_err());
    third
        .input_policy_graph
        .validate_native_closure(&third.sources.input_snapshot)
        .unwrap();
    let resolved = third
        .input_policy_graph
        .complete_text_policy(&third.sources.input_snapshot, &third.input_policies)
        .unwrap()
        .unwrap();
    assert!(resolved.history_retention_allowed && resolved.independent_fact_allowed);
    assert_eq!(resolved.content_hard_expires_at_unix_ms, Some(5000));
    let encoded = serde_json::to_string(&third.input_policy_graph).unwrap();
    for forbidden in [
        "Use my usual environment",
        "answer-first",
        "envelopes",
        "source_set",
        "input_policies",
    ] {
        assert!(!encoded.contains(forbidden), "{forbidden}");
    }
    let decoded: NativeInputPolicyGraph = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, third.input_policy_graph);
}

#[test]
pub(super) fn a_native_control_still_requires_the_actual_parent_archive() {
    let first = native_archive("first-control-parent", vec![owner_message()], &[]);
    let mut control = ChatMessage::system_event("control", "ready");
    control.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        Some(&first.output),
        "review",
        &control.text,
        "context_review",
    )
    .unwrap();
    let second = native_archive(
        "second-control-parent",
        vec![control, answer(&first, "first-control-parent")],
        &[&first],
    );
    let policy = second
        .input_policy_graph
        .complete_text_policy(&second.sources.input_snapshot, &second.input_policies)
        .unwrap()
        .unwrap();
    assert!(policy.history_retention_allowed && !policy.independent_fact_allowed);
    let missing =
        NativeInputPolicyGraph::empty(&second.sources.input_snapshot, "owner", "root").unwrap();
    assert!(
        missing
            .complete_text_policy(&second.sources.input_snapshot, &second.input_policies)
            .unwrap()
            .is_none()
    );
    assert!(
        missing
            .validate_native_closure(&second.sources.input_snapshot)
            .is_err()
    );
}

#[test]
fn missing_parent_or_unknown_original_contract_does_not_receive_current_policy_permission() {
    let first = native_archive("first", vec![owner_message()], &[]);
    let (snapshot, registration) = root(&first, "first");
    let missing = NativeInputPolicyGraph::empty(&snapshot, "owner", "root").unwrap();
    assert!(
        missing
            .complete_text_policy(&snapshot, &registration)
            .unwrap()
            .is_none()
    );
    assert!(missing.validate_native_closure(&snapshot).is_err());
    let known =
        NativeInputPolicyGraph::from_archives(&snapshot, &registration, "owner", "root", &[&first])
            .unwrap();
    let unavailable =
        NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION + 1, 1).unwrap();
    assert!(
        known
            .complete_text_policy(&snapshot, &unavailable)
            .unwrap()
            .is_none()
    );
}

#[test]
fn normalized_references_reject_wrong_context_labels_duplicates_unrelated_parents_and_original_archive_changes()
 {
    let first = native_archive("first", vec![owner_message()], &[]);
    let other = native_archive("other", vec![owner_message()], &[]);
    let (snapshot, registration) = root(&first, "first");
    let original =
        NativeInputPolicyGraph::from_archives(&snapshot, &registration, "owner", "root", &[&first])
            .unwrap();
    for case in 0..8 {
        let mut changed = original.clone();
        match case {
            0 => changed.actor_id = "other-owner".into(),
            1 => changed.parents[0].output_label_sha256 = "b".repeat(64),
            2 => changed.parents.push(changed.parents[0].clone()),
            3 => changed
                .parents
                .push(NativeDerivedInputPolicy::from_archive(&other).unwrap()),
            4 => changed.parents[0].conversation_id = "another-root".into(),
            5 => changed.parents[0].output_digest_sha256 = "invalid".into(),
            6 => changed.parents[0].source_contract_revision += 1,
            _ => changed.parents[0].policy_revision += 1,
        }
        assert!(
            changed.validate(&snapshot, &registration).is_err(),
            "case {case}"
        );
    }
    let mut relabeled = snapshot.clone();
    relabeled.envelopes[0].sensitivity = Sensitivity::Secret;
    let registration =
        NativeInputPolicyRegistration::capture(&relabeled, SOURCE_CONTRACT_REVISION, 1).unwrap();
    let mut changed = original;
    changed.input_snapshot_sha256 = relabeled.fingerprint().unwrap();
    assert!(changed.validate(&relabeled, &registration).is_err());
}

#[test]
fn a_policy_summary_is_not_authority_and_cannot_replace_the_actual_private_parent_record() {
    let mut private = owner_message();
    private
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_provider_id = "unregistered-provider".into();
    let first = native_archive("private", vec![private], &[]);
    let second = native_archive("second", vec![answer(&first, "private")], &[&first]);
    let resolved = second
        .input_policy_graph
        .complete_text_policy(&second.sources.input_snapshot, &second.input_policies)
        .unwrap()
        .unwrap();
    assert!(resolved.no_copy && resolved.no_export && !resolved.history_retention_allowed);
    let mut reference = NativeDerivedInputPolicy::from_archive(&first).unwrap();
    let policy = reference.text_policy.as_mut().unwrap();
    policy.no_copy = false;
    policy.no_export = false;
    policy.history_retention_allowed = true;
    policy.derived_visible_text_allowed = true;
    policy.policy_sha256 = source::policy_hash(policy).unwrap();
    reference.validate().unwrap();
    assert!(reference.validate_archive(&first).is_err());
}

#[test]
fn self_references_are_rejected_by_the_original_native_output_identity() {
    let first = native_archive("first", vec![owner_message()], &[]);
    let mut second = native_archive("second", vec![answer(&first, "first")], &[&first]);
    second.input_policy_graph.parents[0].physical_call_id =
        second.source_set.physical_call_id.clone();
    assert!(second.validate().is_err());
}

#[test]
fn three_hundred_distinct_policies_preserve_full_history_and_the_earliest_deadline() {
    let messages = (0..300)
        .map(|index| {
            let mut message = model_bound_user_message(
                format!("owner-{index}"),
                format!("Original fact {index}"),
                destination(),
            )
            .unwrap();
            message
                .data_envelope
                .as_mut()
                .unwrap()
                .retention
                .expires_at_unix_ms = Some(1000 + index);
            message
        })
        .collect();
    let parent = native_archive("long-history", messages, &[]);
    assert_eq!(parent.input_policies.policies.len(), 300);
    let (snapshot, registration) = root(&parent, "long-history");
    let graph = NativeInputPolicyGraph::from_archives(
        &snapshot,
        &registration,
        "owner",
        "root",
        &[&parent],
    )
    .unwrap();
    let resolved = graph
        .complete_text_policy(&snapshot, &registration)
        .unwrap()
        .unwrap();
    assert!(resolved.history_retention_allowed);
    assert_eq!(resolved.content_hard_expires_at_unix_ms, Some(1000));
}

#[test]
fn a_fifty_answer_chain_does_not_copy_ancestor_snapshots_into_later_archives() {
    let mut previous = native_archive("answer-0", vec![owner_message()], &[]);
    let baseline = serde_json::to_vec(&previous).unwrap().len();
    for index in 1..=50 {
        let id = format!("answer-{index}");
        let old_id = format!("answer-{}", index - 1);
        let next = native_archive(&id, vec![answer(&previous, &old_id)], &[&previous]);
        assert_eq!(next.input_policy_graph.parents.len(), 1);
        assert!(serde_json::to_vec(&next).unwrap().len() < baseline + 4096);
        next.input_policy_graph.parents[0]
            .validate_archive(&previous)
            .unwrap();
        previous = next;
    }
}

#[test]
fn archive_decode_preserves_snapshot_and_contract_boundary_checks() {
    let original = native_archive("boundary", vec![owner_message()], &[]);
    for case in 0..4 {
        let mut changed = original.clone();
        match case {
            0 => changed.input_policies.source_contract_revision += 1,
            1 => changed.input_policies.policy_revision += 1,
            2 => changed.sources.input_snapshot.envelopes[0]
                .allowed_destinations
                .clear(),
            _ => changed.input_policies.leaves.clear(),
        }
        assert!(
            ArchivedAssistantOutput::decode(&serde_json::to_string(&changed).unwrap()).is_err(),
            "case {case}"
        );
    }
    assert_eq!(
        ArchivedAssistantOutput::decode(&serde_json::to_string(&original).unwrap()).unwrap(),
        original
    );
}
