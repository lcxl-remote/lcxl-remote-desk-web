use super::*;
use crate::chat::{ChatMessage, ToolCallRef};
use crate::session::TurnState;
use desk_agent_protocol::{AgentScope, ExecutionMode};

const NOW: u64 = 1792049770485;

fn fixture() -> (
    PersistedAgentSession,
    ScheduleProposal,
    ApprovalDelegation,
    DestinationIdentity,
) {
    let destination = DestinationIdentity::Model {
        connection_id: "model-private-id".into(),
        connection_revision: 1,
        model_id: "model-private-name".into(),
        profile_revision: 1,
    };
    let mut session = PersistedAgentSession::new(
        "run",
        "owner",
        "device",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-10-15T07:36:10Z",
    );
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::AiAssistant);
    session.input_revision = 1;
    session.control_revision = 1;
    session.latest_input_seq = 1;
    session.current_turn_id = Some("turn-private-id".into());
    session.turn_state = TurnState::Running;
    session.lease_token = 1;
    let user = crate::model_message_labels::model_bound_user_message(
        "user".into(),
        "Every morning at 8, check the disk usage and report it.".into(),
        destination.clone(),
    )
    .unwrap();
    let mut assistant = ChatMessage::assistant_tool_calls(
        "assistant",
        "I propose a daily check",
        vec![ToolCallRef {
            id: "call-private-id".into(),
            name: super::super::proposal::REQUEST_SCHEDULE.into(),
            arguments_json: json!({"kind":"fresh_task","title":"Disk check"}).to_string(),
        }],
    );
    assistant.turn_id = session.current_turn_id.clone();
    let bytes = crate::model_egress::message_content_bytes(&assistant).unwrap();
    assistant.data_envelope = Some(
        crate::subagent::projection::envelope(
            "assistant",
            &String::from_utf8(bytes).unwrap(),
            "test",
            &[user.data_envelope.clone().unwrap()],
        )
        .unwrap(),
    );
    let receipt_text =
        json!({"schedule_id":"sc7","kind":"fresh_task","state":"pending_review"}).to_string();
    let mut receipt = ChatMessage::tool_result(
        "schedule-proposal:call-private-id",
        "call-private-id",
        &receipt_text,
    );
    receipt.turn_id = session.current_turn_id.clone();
    receipt.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        assistant.data_envelope.as_ref(),
        "call-private-id",
        &receipt_text,
        "schedule_proposal",
    )
    .unwrap();
    session.conversation = vec![user, assistant, receipt];
    session.pending_schedule_review = Some("sc7".into());
    let proposal = ScheduleProposal {
        schedule_id: "sc7".into(),
        revision: 1,
        owner_id: "owner".into(),
        target_device_id: "device".into(),
        title: "Disk check".into(),
        prompt: "Check the disk usage and report it.".into(),
        spec: serde_json::from_value(
            json!({"schema_version":1,"rule":{"kind":"daily","utc_time":"00:00:00"}}),
        )
        .unwrap(),
        proposal_conversation_id: "run".into(),
        input_revision: 1,
        created_at_unix_ms: NOW,
    };
    let delegation = ApprovalDelegation::new_usage_only(
        "delegation-private-id".into(),
        "run".into(),
        "owner".into(),
        "device".into(),
        "switch-private-id".into(),
        NOW,
    )
    .unwrap();
    (session, proposal, delegation, destination)
}

#[test]
fn a_waiting_fresh_task_proposal_is_reviewed_without_device_authority() {
    let (session, proposal, delegation, destination) = fixture();
    let candidate = candidate(&session, &proposal, &delegation, NOW).unwrap();
    authorize(
        &candidate,
        &session,
        &proposal,
        &delegation,
        &destination,
        NOW,
    )
    .unwrap();
    let view = crate::approval_review::ReviewModelView::from_candidate(&candidate).unwrap();
    let json = serde_json::to_string(&view).unwrap();
    assert!(json.contains("Check the disk usage and report it."));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["source_kind"], "schedule_proposal");
    let action: serde_json::Value =
        serde_json::from_str(value["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(action["operation"], "enable_scheduled_task");
    assert_eq!(action["device_actions_authorized"], false);
    for secret in [
        "delegation-private-id",
        "call-private-id",
        "turn-private-id",
        "model-private-name",
    ] {
        assert!(!json.contains(secret), "private binding leaked: {secret}");
    }
}

#[test]
fn only_the_waiting_turn_of_the_proposing_conversation_can_be_reviewed() {
    let (session, proposal, delegation, _) = fixture();
    for case in 0..7 {
        let mut session = session.clone();
        let mut proposal = proposal.clone();
        match case {
            0 => session.pending_schedule_review = None,
            1 => session.turn_state = TurnState::Idle,
            2 => session.input_revision += 1,
            3 => proposal.proposal_conversation_id = "other".into(),
            4 => proposal.owner_id = "someone-else".into(),
            5 => session.trigger_origin = crate::session::TriggerOrigin::IssueSubscription,
            6 => session.conversation.pop().map(|_| ()).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            candidate(&session, &proposal, &delegation, NOW).is_err(),
            "case {case}"
        );
    }
    // After the review window only the owner can decide.
    assert!(candidate(&session, &proposal, &delegation, NOW + AI_REVIEW_WINDOW_MS).is_err());
}

#[test]
fn a_changed_proposal_cannot_reuse_its_review() {
    let (session, mut proposal, delegation, _) = fixture();
    let candidate = candidate(&session, &proposal, &delegation, NOW).unwrap();
    proposal.prompt = "Delete old logs as well".into();
    assert!(validate_current(&candidate, &session, &proposal, &delegation, NOW).is_err());
}
