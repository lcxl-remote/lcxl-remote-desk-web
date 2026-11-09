use super::*;
use crate::{
    ai_assistant::{seam::AssistantDataProjectionRef, test_support::personal_session},
    model_message_labels::model_bound_user_message,
    native_ids::{self, Kind},
    prompt::ResponseFormatSpec,
};
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettings;

fn fixture() -> (PersistedAgentSession, ModelRequest, AssistantDataToolAccess) {
    let mut session = personal_session("owner", "root", "device");
    session.lease_token = 1;
    session.input_revision = 1;
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let message = model_bound_user_message(
        "question".into(),
        "new independent question".into(),
        destination,
    )
    .unwrap();
    let mut request = ModelRequest::text_only(vec![message], ResponseFormatSpec::None);
    request.context_conversation_id = Some("root".into());
    let access = AssistantDataToolAccess {
        settings: AssistantDataSettings::default(),
        policy_revision: 1,
        memory_state_generation: 1,
        control_generation: 1,
    };
    (session, request, access)
}

#[test]
fn plain_eligible_request_gets_a_complete_metadata_manifest_without_read_or_execution_grants() {
    let (session, request, access) = fixture();
    let manifest =
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).unwrap();
    assert_eq!(manifest.selected_envelopes.len(), 1);
    assert_eq!(
        manifest.selected_envelopes[0].envelope_id,
        request.messages[0]
            .data_envelope
            .as_ref()
            .unwrap()
            .envelope_id
    );
    assert!(manifest.read_projection_ids.is_empty() && manifest.current_memories.is_empty());
    manifest.validate_for(&session, &request).unwrap();
}

#[test]
fn session_scope_generation_and_selected_label_changes_are_rejected() {
    let (session, request, access) = fixture();
    for case in 0..6 {
        let mut session = session.clone();
        let mut request = request.clone();
        let mut access = access.clone();
        match case {
            0 => session.assistant_data.as_mut().unwrap().temporary = true,
            1 => request.context_conversation_id = Some("another-root".into()),
            2 => access.control_generation += 1,
            3 => access.memory_state_generation += 1,
            _ => {
                let mut changed = request.messages[0].clone();
                changed.message_id = "duplicated-label".into();
                changed
                    .data_envelope
                    .as_mut()
                    .unwrap()
                    .retention
                    .expires_at_unix_ms = Some(1000);
                request.messages.push(changed);
            }
        }
        assert!(
            AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[])
                .is_err(),
            "case {case}"
        );
    }
}

#[test]
fn the_final_manifest_check_also_rejects_conflicting_labels_with_an_unchanged_body_digest() {
    let (session, mut request, access) = fixture();
    let manifest =
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).unwrap();
    let mut duplicate = request.messages[0].clone();
    duplicate.message_id = "duplicate-identity".into();
    duplicate
        .data_envelope
        .as_mut()
        .unwrap()
        .retention
        .expires_at_unix_ms = Some(1000);
    assert_eq!(
        duplicate.data_envelope.as_ref().unwrap().content,
        request.messages[0].data_envelope.as_ref().unwrap().content
    );
    request.messages.push(duplicate);
    assert!(manifest.validate_for(&session, &request).is_err());
}

#[test]
fn candidate_identity_is_not_a_proof_and_unselected_session_caches_do_not_become_selected_reads() {
    let (mut session, mut request, access) = fixture();
    let envelope = request.messages[0].data_envelope.as_mut().unwrap();
    envelope.provenance.source_provider_id = "assistant-data-source".into();
    envelope.provenance.source_tool_name = "current_memories".into();
    let id = native_ids::stable(Kind::AssistantReadProjection, &"candidate-only");
    let data = session.assistant_data.as_mut().unwrap();
    data.read_uses.push(AssistantDataProjectionRef {
        projection_id: id.clone(),
        source_envelope_id: envelope.envelope_id.clone(),
        purpose: SourceReadPurpose::ModelMemory,
        retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
        scope: Box::default(),
        policy_revision: 1,
        control_generation: 1,
        memory_state_generation: 1,
        expires_at_unix_ms: None,
    });
    assert_eq!(
        AssistantModelDataManifest::selected_read_ids(&session, &request).unwrap(),
        vec![id]
    );
    assert!(
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).is_err()
    );
    request.messages.clear();
    let manifest =
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).unwrap();
    assert!(manifest.read_projection_ids.is_empty() && manifest.selected_envelopes.is_empty());
}

#[test]
fn complete_manifest_preserves_bounded_long_history_instead_of_imposing_a_new_message_cap() {
    let (session, mut request, access) = fixture();
    let destination = request.messages[0]
        .data_envelope
        .as_ref()
        .unwrap()
        .allowed_destinations[0]
        .clone();
    for index in 1..300 {
        request.messages.push(
            model_bound_user_message(
                format!("question-{index}"),
                "retained original question".into(),
                destination.clone(),
            )
            .unwrap(),
        );
    }
    let manifest =
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).unwrap();
    assert_eq!(manifest.selected_envelopes.len(), 300);
    assert!(
        manifest
            .selected_envelopes
            .iter()
            .any(|item| item.envelope_id == "user-message-question-299")
    );
    let mut missing_last = manifest;
    missing_last
        .selected_envelopes
        .retain(|item| item.envelope_id != "user-message-question-299");
    assert!(missing_last.validate_for(&session, &request).is_err());
}

#[test]
fn main_assistant_budget_reservations_do_not_turn_its_data_request_into_a_child_request() {
    use crate::subagent::{
        budget::Usage,
        reservation::{DelegationCallKind, DelegationCallReservation},
    };
    let (mut session, mut request, access) = fixture();
    session.control_revision = 1;
    session.delegation_group_id = Some("group".into());
    let manifest =
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).unwrap();
    let reservation = DelegationCallReservation {
        reservation_id: "budget".into(),
        logical_call_id: "model-call".into(),
        root_conversation_id: session.conversation_id.clone(),
        group_id: "group".into(),
        conversation_id: session.conversation_id.clone(),
        task_id: None,
        kind: DelegationCallKind::Model,
        arguments_sha256: "a".repeat(64),
        source_epoch: 1,
        input_revision: session.input_revision,
        control_revision: session.control_revision,
        planning_lease_token: Some(session.lease_token),
        review_authority: None,
        upper: Usage {
            model_calls: 1,
            tool_calls: 0,
            tokens: 100,
        },
        source_goal_upper: None,
    };
    reservation.validate().unwrap();
    request.delegation_call = Some(reservation.clone());
    assert_eq!(
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).unwrap(),
        manifest
    );
    manifest.validate_for(&session, &request).unwrap();
    assert_eq!(request.delegation_call.as_ref(), Some(&reservation));
    for case in 0..6 {
        let mut changed = request.clone();
        let call = changed.delegation_call.as_mut().unwrap();
        match case {
            0 => call.group_id = "another-group".into(),
            1 => {
                call.root_conversation_id = "another-root".into();
                call.conversation_id = "another-root".into();
            }
            2 => call.input_revision += 1,
            3 => call.control_revision += 1,
            4 => call.planning_lease_token = Some(session.lease_token + 1),
            _ => call.kind = DelegationCallKind::ApprovalReview,
        }
        assert!(
            manifest.validate_for(&session, &changed).is_err(),
            "case {case}"
        );
    }
    session.surface = crate::session::AgentSessionSurface::TerminalAiAssistant;
    assert!(
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).is_err()
    );
}

#[test]
fn saved_snapshots_remain_available_to_callbacks_without_account_read_grants() {
    let (mut session, mut request, access) = fixture();
    session.trigger_origin = crate::session::TriggerOrigin::WorkCompletion {
        kind: crate::session::WorkKind::ComputerAction,
    };
    let message = &mut request.messages[0];
    let envelope = message.data_envelope.as_mut().unwrap();
    envelope.provenance.source_provider_id = "assistant-conversation-snapshot".into();
    envelope.provenance.source_object_id = Some(native_ids::stable(
        Kind::AssistantReadProjection,
        &"saved-snapshot",
    ));
    session.conversation.push(message.clone());
    let manifest =
        AssistantModelDataManifest::for_direct_request(&session, &request, &access, &[]).unwrap();
    manifest.validate_for(&session, &request).unwrap();
    assert!(manifest.read_projection_ids.is_empty());
    assert!(!crate::ai_assistant::data_access::eligible_origin(&session));
    request.messages[0].text.push_str(" forged");
    assert!(manifest.validate_for(&session, &request).is_err());
}
