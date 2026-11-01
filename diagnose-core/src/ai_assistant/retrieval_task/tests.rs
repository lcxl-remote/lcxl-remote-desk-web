use super::*;
use crate::{
    ai_assistant::{data_access::bind_opened_task, test_support::personal_session},
    focus_epoch::GoalSegmentIdentity,
    goal::{GoalLimits, GoalModelBinding, GoalOpening, GoalPauseReason, GoalWaitReason},
    session::TurnState,
};

fn fixture() -> (PersistedAgentSession, GoalRun) {
    let mut session = personal_session("owner", "root", "device");
    session.input_revision = 1;
    session.focus_epoch.input_revision = 1;
    session.turn_state = TurnState::Running;
    let mut goal = GoalRun::new(
        "goal".into(),
        "root".into(),
        "owner".into(),
        "device".into(),
        "Find the original report".into(),
        "owner-message".into(),
        GoalOpening::OwnerRequest,
        GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
        1,
        1000,
        GoalLimits::default(),
    )
    .unwrap();
    goal.deadline_unix_ms = 10000;
    bind_opened_task(&mut session, &goal.goal_id, goal.deadline_unix_ms).unwrap();
    (session, goal)
}

fn claimed() -> (PersistedAgentSession, GoalRun) {
    let (mut session, mut goal) = fixture();
    goal.claim_slice(1100).unwrap();
    session.trigger_origin = TriggerOrigin::GoalContinuation;
    session.focus_epoch.goal_segment = Some(GoalSegmentIdentity {
        goal_id: goal.goal_id.clone(),
        source_message_id: goal.source_message_id.clone(),
        segment_seq: goal.slice_seq,
        goal_revision: goal.goal_revision,
        lease_epoch: goal.lease_epoch,
    });
    (session, goal)
}

#[test]
fn optional_consent_expiry_does_not_remove_the_native_goal_deadline() {
    let (mut session, goal) = fixture();
    session
        .assistant_data
        .as_mut()
        .unwrap()
        .memory_access_scope
        .as_mut()
        .unwrap()
        .expires_at_unix_ms = None;
    session.assistant_data.as_ref().unwrap().validate().unwrap();
    let authority = goal_authority(&session, &goal, 1200).unwrap();
    assert_eq!(
        authority.native_deadline(None, None, 1200).unwrap(),
        goal.deadline_unix_ms
    );
    assert!(goal_authority(&session, &goal, goal.deadline_unix_ms).is_err());
}

#[test]
fn owner_clarification_and_approval_can_read_with_the_original_scope_while_the_goal_is_not_running()
{
    for state in [
        GoalState::Queued,
        GoalState::Waiting(GoalWaitReason::User),
        GoalState::Paused(GoalPauseReason::Recovery),
        GoalState::Blocked,
    ] {
        let (mut session, mut goal) = fixture();
        goal.state = state;
        let original = session.assistant_data.clone();
        for trigger in [TriggerOrigin::User, TriggerOrigin::PermissionDecision] {
            session.trigger_origin = trigger;
            let authority = goal_authority(&session, &goal, 1200).unwrap();
            assert!(!authority.requires_segment_lease);
            assert_eq!(authority.native_deadline(None, None, 1200).unwrap(), 10000);
            assert!(
                authority
                    .native_deadline(Some("old-planner"), Some(1100), 1200)
                    .is_err()
            );
            assert!(authority.native_deadline(None, Some(1300), 1200).is_err());
        }
        assert_eq!(session.assistant_data, original);
    }
}

#[test]
fn current_owner_correction_does_not_resume_a_running_segment_or_refill_the_old_episode() {
    let (mut session, mut goal) = claimed();
    let episode = session
        .assistant_data
        .as_ref()
        .unwrap()
        .retrieval_budget
        .clone();
    goal.revise_input(2, 1200).unwrap();
    session.input_revision = 2;
    session.focus_epoch.goal_segment = None;
    session.focus_epoch.input_revision = 2;
    session.trigger_origin = TriggerOrigin::User;
    let authority = goal_authority(&session, &goal, 1201).unwrap();
    assert!(!authority.requires_segment_lease);
    assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Recovery));
    assert_eq!(
        session.assistant_data.as_ref().unwrap().retrieval_budget,
        episode
    );
    assert_eq!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .history_access_scope
            .as_ref()
            .unwrap()
            .authorized_input_seq,
        1
    );
    session.input_revision = 1;
    assert!(goal_authority(&session, &goal, 1201).is_err());
}

#[test]
fn background_and_its_permission_resume_need_the_exact_original_slice_and_live_native_lease() {
    let (mut session, goal) = claimed();
    for trigger in [
        TriggerOrigin::GoalContinuation,
        TriggerOrigin::PermissionDecision,
    ] {
        session.trigger_origin = trigger;
        let authority = goal_authority(&session, &goal, 1200).unwrap();
        assert!(authority.requires_segment_lease);
        assert_eq!(
            authority
                .native_deadline(Some("planner"), Some(1500), 1200)
                .unwrap(),
            1500
        );
        assert_eq!(
            authority
                .native_deadline(Some("planner"), Some(20000), 1200)
                .unwrap(),
            10000
        );
        for deadline in [None, Some(-1), Some(1200), Some(1199)] {
            assert!(
                authority
                    .native_deadline(Some("planner"), deadline, 1200)
                    .is_err()
            );
        }
        assert!(authority.native_deadline(None, Some(1500), 1200).is_err());
        assert!(
            authority
                .native_deadline(Some(""), Some(1500), 1200)
                .is_err()
        );
    }
}

#[test]
fn no_stale_or_fabricated_segment_can_turn_a_user_or_scheduled_context_into_goal_authority() {
    let (session, goal) = claimed();
    for variant in 0..8 {
        let mut changed = session.clone();
        match variant {
            0 => {
                changed
                    .focus_epoch
                    .goal_segment
                    .as_mut()
                    .unwrap()
                    .segment_seq += 1
            }
            1 => {
                changed
                    .focus_epoch
                    .goal_segment
                    .as_mut()
                    .unwrap()
                    .goal_revision += 1
            }
            2 => {
                changed
                    .focus_epoch
                    .goal_segment
                    .as_mut()
                    .unwrap()
                    .lease_epoch += 1
            }
            3 => {
                changed
                    .focus_epoch
                    .goal_segment
                    .as_mut()
                    .unwrap()
                    .source_message_id = "unapproved-input".into()
            }
            4 => changed.focus_epoch.goal_segment = None,
            5 => changed.trigger_origin = TriggerOrigin::User,
            6 => changed.trigger_origin = TriggerOrigin::ScheduledTask,
            _ => changed.trigger_origin = TriggerOrigin::ScheduledContinuation,
        }
        assert!(
            goal_authority(&changed, &goal, 1200).is_err(),
            "variant {variant}"
        );
    }
    let (mut session, mut goal) = fixture();
    goal.claim_slice(1100).unwrap();
    assert!(goal_authority(&session, &goal, 1200).is_err());
    session.trigger_origin = TriggerOrigin::GoalContinuation;
    assert!(goal_authority(&session, &goal, 1200).is_err());
}

#[test]
fn terminal_goals_wrong_subjects_newer_inputs_and_absolute_expiry_do_not_grant_reads() {
    let (session, original) = fixture();
    for state in [
        GoalState::Completed,
        GoalState::Failed,
        GoalState::Cancelled,
    ] {
        let mut goal = original.clone();
        goal.state = state;
        assert!(goal_authority(&session, &goal, 1200).is_err());
    }
    for variant in 0..5 {
        let mut goal = original.clone();
        match variant {
            0 => goal.goal_id = "other-goal".into(),
            1 => goal.conversation_id = "other-root".into(),
            2 => goal.owner_id = "other-owner".into(),
            3 => goal.device_id = "other-device".into(),
            _ => goal.input_revision += 1,
        }
        assert!(goal_authority(&session, &goal, 1200).is_err());
    }
    assert!(goal_authority(&session, &original, 10000).is_err());
}
