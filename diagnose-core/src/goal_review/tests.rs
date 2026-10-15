use super::*;
use crate::chat::{ChatMessage, ToolCallRef};
use crate::goal::{GoalLimits, GoalModelBinding};
use crate::session::TurnState;
use desk_agent_protocol::{AgentScope, ExecutionMode};

fn fixture() -> (
    PersistedAgentSession,
    GoalOpenRequest,
    ApprovalDelegation,
    GoalBudgetPolicy,
    DestinationIdentity,
    u64,
) {
    let now = 1792049770485;
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
    session.turn_state = TurnState::Idle;
    session.lease_token = 1;
    let user = crate::model_message_labels::model_bound_user_message(
        "user".into(),
        "Please keep working until the report is finished.".into(),
        destination.clone(),
    )
    .unwrap();
    let goal_text = "Finish the requested report";
    let mut assistant = ChatMessage::assistant_tool_calls(
        "assistant",
        "I propose this goal",
        vec![ToolCallRef {
            id: "call-private-id".into(),
            name: crate::goal_tools::REQUEST_GOAL_TOOL_NAME.into(),
            arguments_json: json!({"goal_text":goal_text}).to_string(),
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
    session.conversation = vec![user, assistant];
    let request = GoalOpenRequest::new(
        "request-private-id".into(),
        "run".into(),
        "owner".into(),
        "device".into(),
        "user".into(),
        1,
        goal_text.into(),
        GoalLimits::default(),
        GoalModelBinding::from_destination(&destination).unwrap(),
        now,
    )
    .unwrap();
    let delegation = ApprovalDelegation::new_usage_only(
        "delegation-private-id".into(),
        "run".into(),
        "owner".into(),
        "device".into(),
        "switch-private-id".into(),
        now,
    )
    .unwrap();
    (
        session,
        request,
        delegation,
        crate::goal_budget::initial(),
        destination,
        now,
    )
}

#[test]
fn idle_goal_uses_the_shared_reviewer_without_any_device_authority() {
    let (session, request, delegation, policy, destination, now) = fixture();
    let candidate = candidate(&session, &request, &policy, &delegation, None, now).unwrap();
    authorize(
        &candidate,
        &session,
        &request,
        &policy,
        None,
        &delegation,
        &destination,
        now,
    )
    .unwrap();
    let view = crate::approval_review::ReviewModelView::from_candidate(&candidate).unwrap();
    let json = serde_json::to_string(&view).unwrap();
    assert!(json.contains("Finish the requested report"));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let action: serde_json::Value =
        serde_json::from_str(value["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(action["device_actions_authorized"], false);
    for secret in [
        "request-private-id",
        "delegation-private-id",
        "call-private-id",
        "turn-private-id",
        "model-private-name",
    ] {
        assert!(!json.contains(secret), "private binding leaked: {secret}");
    }
    assert!(
        candidate.context.current_authority[0]
            .active_grant_ids
            .is_empty()
    );
}

#[test]
fn an_active_turn_or_new_input_cannot_be_reviewed_as_the_idle_proposal() {
    let (mut session, request, delegation, policy, _, now) = fixture();
    session.turn_state = TurnState::Running;
    assert!(candidate(&session, &request, &policy, &delegation, None, now).is_err());
    session.turn_state = TurnState::Idle;
    session.input_revision += 1;
    assert!(candidate(&session, &request, &policy, &delegation, None, now).is_err());
}

#[test]
fn changed_limits_goal_text_or_control_cannot_reuse_a_review() {
    let (mut session, mut request, delegation, mut policy, _, now) = fixture();
    let candidate = candidate(&session, &request, &policy, &delegation, None, now).unwrap();
    policy.revision += 1;
    assert!(
        validate_current(
            &candidate,
            &session,
            &request,
            &policy,
            None,
            &delegation,
            now
        )
        .is_err()
    );
    policy.revision -= 1;
    session.control_revision += 1;
    assert!(
        validate_current(
            &candidate,
            &session,
            &request,
            &policy,
            None,
            &delegation,
            now
        )
        .is_err()
    );
    session.control_revision -= 1;
    request.goal_text = "Do something else".into();
    assert!(
        validate_current(
            &candidate,
            &session,
            &request,
            &policy,
            None,
            &delegation,
            now
        )
        .is_err()
    );
}

#[test]
fn expired_or_withdrawn_proposals_never_enter_the_reviewer() {
    let (session, mut request, delegation, policy, _, now) = fixture();
    assert!(
        candidate(
            &session,
            &request,
            &policy,
            &delegation,
            None,
            request.expires_at_unix_ms
        )
        .is_err()
    );
    request
        .close(GoalOpenRequestState::Withdrawn, "withdraw".into(), now + 1)
        .unwrap();
    assert!(candidate(&session, &request, &policy, &delegation, None, now).is_err());
}

#[test]
fn goal_revision_review_preserves_the_same_goal_and_spent_budget() {
    use crate::dynamic_run::AgentRunEventKind;
    use crate::goal::{GoalControl, GoalOpenRequestEvent, GoalWaitReason};
    let (mut session, mut original, delegation, policy, destination, now) = fixture();
    let binding = original.model_binding.clone();
    let mut goal = original
        .approve(1, &binding, "goal".into(), "open".into(), now + 1, None)
        .unwrap();
    goal.used.model_calls = 3;
    let (_, epoch) = goal.claim_slice(now + 2).unwrap();
    goal.finish_slice(
        1,
        1,
        epoch,
        &GoalControl::Wait {
            reason: GoalWaitReason::User,
            reference_id: "clarify".into(),
        },
        false,
        false,
        1,
        vec![],
        now + 3,
    )
    .unwrap();
    goal.revise_input(2, now + 4).unwrap();
    session.input_revision = 2;
    let mut request = GoalOpenRequest::new_revision(
        "revision".into(),
        &goal,
        "user".into(),
        2,
        "Finish the requested report".into(),
        binding.clone(),
        now + 5,
    )
    .unwrap();
    let candidate = candidate(
        &session,
        &request,
        &policy,
        &delegation,
        Some(&goal),
        now + 5,
    )
    .unwrap();
    authorize(
        &candidate,
        &session,
        &request,
        &policy,
        Some(&goal),
        &delegation,
        &destination,
        now + 5,
    )
    .unwrap();
    let view = crate::approval_review::ReviewModelView::from_candidate(&candidate).unwrap();
    let view = serde_json::to_value(view).unwrap();
    assert!(
        view["action_json"]
            .as_str()
            .unwrap()
            .contains("revise_goal")
    );
    let action: serde_json::Value =
        serde_json::from_str(view["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        action["budget_limits"]["deadlineMs"].as_u64(),
        policy.limits.deadline_ms
    );
    let spent = goal.used;
    let event_id = GoalOpenRequestEvent::id_for(
        &request.request_id,
        GoalOpenRequestState::Approved,
        AgentRunEventKind::GoalOpenDecided,
    )
    .unwrap();
    request
        .approve_revision(&mut goal, 2, &binding, event_id, now + 6)
        .unwrap();
    assert_eq!(goal.goal_id, "goal");
    assert_eq!(goal.goal_revision, 2);
    assert_eq!(goal.used, spent);
    let mut event = GoalOpenRequestEvent::new(
        &request,
        AgentRunEventKind::GoalOpenDecided,
        1,
        session.updated_at.clone(),
    )
    .unwrap();
    event.review_decision = Some(GoalReviewDecision {
        candidate_id: candidate.candidate_id,
        action_sha256: candidate.action_sha256,
        source: crate::dynamic_run::PermissionDecisionSource::SystemDecision {
            reason_code: "fault".into(),
            reason: "failed".into(),
        },
        reason: "failed".into(),
    });
    assert!(
        event.validate().is_err(),
        "a system failure must never claim it approved the goal"
    );
}

#[test]
fn closing_delegation_rejects_a_frozen_goal_review() {
    let (session, request, mut delegation, policy, destination, now) = fixture();
    let candidate = candidate(&session, &request, &policy, &delegation, None, now).unwrap();
    delegation
        .close(crate::approval_delegation::ApprovalDelegationStatus::Closed)
        .unwrap();
    assert!(
        authorize(
            &candidate,
            &session,
            &request,
            &policy,
            None,
            &delegation,
            &destination,
            now + 1
        )
        .is_err()
    );
}
