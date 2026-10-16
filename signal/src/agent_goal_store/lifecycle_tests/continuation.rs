use super::*;
use desk_diagnose_core::session::{ActionIdentity, ExecutionState};

async fn read_session(db: &DatabaseConnection, cid: &str) -> PersistedAgentSession {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(cid))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

async fn store_fixture(db: &DatabaseConnection, session: &PersistedAgentSession) {
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .exec(db)
        .await
        .unwrap();
}

fn params(session: &PersistedAgentSession, turn: &str, now: DateTime<Utc>) -> ClaimTurnParams {
    ClaimTurnParams {
        conversation_id: session.conversation_id.clone(),
        actor_id: session.actor_id.clone(),
        device_id: session.device_id.clone(),
        policy_revision: 1,
        current_pdp_scope: session.scope_snapshot.clone(),
        turn_id: turn.into(),
        request_id: None,
        connection_id: None,
        trigger_origin: TriggerOrigin::GoalContinuation,
        now: now.to_rfc3339(),
    }
}

async fn claimed_fixture(
    db: &DatabaseConnection,
    cid: &str,
    now: DateTime<Utc>,
) -> ClaimedGoalSlice {
    seed_session(db, cid).await;
    let mut session = read_session(db, cid).await;
    session.latest_input_seq = 1;
    session.conversation.push(
        desk_diagnose_core::model_message_labels::model_bound_user_message(
            "message-1".into(),
            "Finish the report".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap(),
    );
    store_fixture(db, &session).await;
    let goal = goal(cid, now.timestamp_millis() as u64 - 1_000);
    insert(db, &goal).await;
    let txn = crate::db::begin_write(db, agent_session::Entity)
        .await
        .unwrap();
    let requirement = session.conversation[0].clone();
    crate::agent_subagent_store::initialize_input_group_on(
        &txn,
        &mut session,
        requirement,
        None,
        Some(&goal),
        now.timestamp_millis(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    store_fixture(db, &session).await;
    claim_slice(
        db,
        &params(&session, "random-first-turn", now),
        &goal.goal_id,
        Some(goal.state_version),
        "coordinator-a",
    )
    .await
    .unwrap()
    .unwrap()
}

async fn deliver_fixture(
    db: &DatabaseConnection,
    action: &ActionIdentity,
    cid: &str,
    now: DateTime<Utc>,
) -> String {
    let mut session = read_session(db, cid).await;
    let original = crate::goal_work::original_on(db, &session, action)
        .await
        .unwrap()
        .unwrap();
    let event_id = original.event_id.unwrap();
    assert!(session.apply_completion(
        &event_id,
        &action.execution_id,
        &format!("call-{}", action.action_request_id),
        &action.action_request_id,
        "verified result",
        &now.to_rfc3339()
    ));
    assert!(
        session.add_pending_auto_trigger(desk_diagnose_core::session::PendingAutoTrigger {
            work_id: action.work_id,
            kind: action.kind,
            execution_id: action.execution_id.clone(),
            tool_call_id: format!("call-{}", action.action_request_id),
            event_id: event_id.clone(),
            chain_id: session.chain_id.clone(),
            resolution_org_id: None,
            since: now.to_rfc3339(),
        })
    );
    // Formatting may already have externalized the output before the scheduler wakes.
    for message in &mut session.conversation {
        message.pending_delivery_format = None;
    }
    store_fixture(db, &session).await;
    event_id
}

#[tokio::test]
async fn native_wait_wakes_on_any_member_and_claims_with_other_work_running() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "wait-any", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    let b = add_work(&db, &claimed.session, "b").await;
    claimed.session.execution_state = ExecutionState::from_states(vec![
        ExecutionState::Executing { action: a.clone() },
        ExecutionState::Executing { action: b.clone() },
    ]);
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wait.members.len(), 2);
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting for results".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    assert_eq!(waiting.state, GoalState::Waiting(GoalWaitReason::Work));
    let deferred = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.state, waiting.state);
    complete_work(&db, &b).await;
    let event = deliver_fixture(&db, &b, "wait-any", now).await;
    let queued = wake_for_work_completion(&db, &deferred, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued.state, GoalState::Queued);
    assert_eq!(
        queued.work_wait.as_ref().unwrap().ready_event_ids,
        vec![event.clone()]
    );
    let saved = read_session(&db, "wait-any").await;
    assert_eq!(saved.execution_state.tasks(), vec![&a]);
    assert_eq!(saved.pending_auto_triggers.len(), 1);
    let next = claim_slice(
        &db,
        &params(&saved, "another-turn", now),
        &queued.goal_id,
        Some(queued.state_version),
        "coordinator-b",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(next.goal.state, GoalState::Running);
    assert_eq!(
        next.session.pending_auto_triggers.len(),
        1,
        "claim must not acknowledge model reaction"
    );
    assert_eq!(
        next.goal.work_wait.as_ref().unwrap().ready_event_ids,
        vec![event]
    );
    assert!(
        claim_slice(
            &db,
            &params(&saved, "competing-turn", now),
            &queued.goal_id,
            Some(queued.state_version),
            "coordinator-c"
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        pause_if_unresolved_work(&db, &queued, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::goal_work::prepare_on(&db, &next.goal, &next.session, "wait-again")
            .await
            .unwrap()
            .unwrap()
            .members
            == vec![a]
    );
}

#[tokio::test]
async fn completion_between_wait_preparation_and_settlement_is_not_lost() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "register-race", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    complete_work(&db, &a).await;
    crate::goal_work::validate_wait_on(&db, &claimed.goal, &claimed.session, &wait)
        .await
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    deliver_fixture(&db, &a, "register-race", now).await;
    let mut saved = read_session(&db, "register-race").await;
    saved.pending_auto_triggers.clear();
    store_fixture(&db, &saved).await;
    assert_eq!(
        wake_for_work_completion(&db, &waiting, now)
            .await
            .unwrap()
            .unwrap()
            .state,
        GoalState::Queued
    );
}

#[tokio::test]
async fn normal_work_is_not_recovery_but_mixed_unknown_work_is() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "normal-and-unknown", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let queued = settle_slice(&db, &mut claimed.session, &GoalSegmentEnd::Yield, &[], now)
        .await
        .unwrap();
    assert_eq!(queued.state, GoalState::Queued);
    assert!(
        pause_if_unresolved_work(&db, &queued, now)
            .await
            .unwrap()
            .is_none()
    );
    let mut session = read_session(&db, "normal-and-unknown").await;
    session.execution_state = ExecutionState::Concurrent {
        tasks: vec![desk_diagnose_core::session::BackgroundExecution {
            action: a,
            unknown: None,
        }],
        interrupted_since: Some(now.to_rfc3339()),
    };
    store_fixture(&db, &session).await;
    let paused = pause_if_unresolved_work(&db, &queued, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Recovery));
    let mut saved = read_session(&db, "normal-and-unknown").await;
    let action = saved.execution_state.tasks()[0].clone();
    saved.execution_state = ExecutionState::Executing { action };
    store_fixture(&db, &saved).await;
    let resumed = apply_owner_action(
        &db,
        "normal-and-unknown",
        &saved.actor_id,
        &saved.device_id,
        &paused.goal_id,
        paused.state_version,
        GoalOwnerAction::Resume,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resumed.state, GoalState::Queued);
}

#[tokio::test]
async fn late_result_keeps_original_goal_owner_after_cancellation_and_new_goal() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "late-owner", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    let cancelled = apply_owner_action(
        &db,
        "late-owner",
        &claimed.session.actor_id,
        &claimed.session.device_id,
        &waiting.goal_id,
        waiting.state_version,
        GoalOwnerAction::Cancel,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    let mut new_goal = goal("late-owner", now.timestamp_millis() as u64);
    new_goal.goal_id = "new-goal".into();
    insert(&db, &new_goal).await;
    complete_work(&db, &a).await;
    deliver_fixture(&db, &a, "late-owner", now).await;
    let saved = read_session(&db, "late-owner").await;
    let owner = crate::goal_work::owner_on(&db, &saved, &a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.goal_id, waiting.goal_id);
    assert_eq!(owner.state, GoalState::Cancelled);
    assert!(
        wake_for_work_completion(&db, &cancelled, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        wake_for_work_completion(&db, &new_goal, now)
            .await
            .unwrap()
            .is_none()
    );
    let mut forged = a;
    forged.execution_id = "different-generation".into();
    assert!(
        crate::goal_work::owner_on(&db, &saved, &forged)
            .await
            .is_err()
    );
}

use crate::agent_background_task_store::{BackgroundTaskCreate, SignalBackgroundTaskStore};
use crate::entity::agent_action_item;
use desk_agent_protocol::capability_provider::{
    CapabilityEffect, CapabilityTaskRef, ExecutionPolicy,
};
use desk_diagnose_core::dynamic_run::{
    BACKGROUND_TASK_SCHEMA_VERSION, BackgroundTaskRecord, BackgroundTaskState,
};
use desk_diagnose_core::session::WorkKind;
async fn add_work(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    task: &str,
) -> ActionIdentity {
    let now = Utc::now().to_rfc3339();
    let row = SignalBackgroundTaskStore::new(db.clone())
        .create(BackgroundTaskCreate {
            actor_id: session.actor_id.clone(),
            target_device_id: session.device_id.clone(),
            policy_revision: 1,
            record: BackgroundTaskRecord {
                schema_version: BACKGROUND_TASK_SCHEMA_VERSION,
                task: CapabilityTaskRef {
                    task_id: task.into(),
                    call_id: format!("call-{task}"),
                    run_id: session.conversation_id.clone(),
                    provider_id: "file.workspace".into(),
                    capability_id: "file.report.create".into(),
                    input_revision: 1,
                    generation: 1,
                },
                turn_id: session.current_turn_id.clone().unwrap(),
                tool_name: "create_report".into(),
                canonical_input_digest_sha256: "a".repeat(64),
                effect: CapabilityEffect::WriteArtifact,
                execution_policy: ExecutionPolicy::Adaptive {
                    foreground_budget_ms: 5000,
                },
                supports_cancel: true,
                state: BackgroundTaskState::Running,
                progress_sequence: 0,
                started_at: now.clone(),
                updated_at: now,
                terminal_at: None,
                cancel_request_id: None,
                result_envelope_ids: vec![],
            },
        })
        .await
        .unwrap();
    ActionIdentity::new(
        row.id,
        task,
        format!("{task}:1"),
        WorkKind::CapabilityProvider,
    )
}
async fn complete_work(db: &DatabaseConnection, action: &ActionIdentity) {
    let row = agent_action_item::Entity::find_by_id(action.work_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let record: BackgroundTaskRecord = serde_json::from_str(&row.payload_json).unwrap();
    SignalBackgroundTaskStore::new(db.clone())
        .apply_completion(
            &desk_agent_protocol::capability_provider::CapabilityCompletionEvent {
                task: record.task,
                sequence: 1,
                completion:
                    desk_agent_protocol::capability_provider::CapabilityCompletionClass::Succeeded,
                result_envelope_ids: vec![format!("result-{}", action.action_request_id)],
            },
            &Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_paused_goal_keeps_receipts_without_waking_planning() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "owner-pause", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    let paused = apply_owner_action(
        &db,
        "owner-pause",
        &claimed.session.actor_id,
        &claimed.session.device_id,
        &waiting.goal_id,
        waiting.state_version,
        GoalOwnerAction::Pause,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    complete_work(&db, &a).await;
    let event = deliver_fixture(&db, &a, "owner-pause", now).await;
    assert!(
        wake_for_work_completion(&db, &paused, now)
            .await
            .unwrap()
            .is_none()
    );
    let saved = read_session(&db, "owner-pause").await;
    assert!(
        saved
            .conversation
            .iter()
            .any(|message| message.message_id == event)
    );
    assert_eq!(saved.pending_auto_triggers.len(), 1);
}

#[tokio::test]
async fn ordinary_task_is_tracked_but_cannot_be_adopted_by_a_goal_wait() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "unrelated-work", now).await;
    let mut origin = claimed.session.clone();
    origin.current_turn_id = Some("ordinary-turn-outside-goal".into());
    let a = add_work(&db, &origin, "ordinary").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    assert!(
        crate::goal_work::all_tracked_on(&db, &claimed.session)
            .await
            .unwrap()
    );
    assert!(
        crate::goal_work::owner_on(&db, &claimed.session, &a)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn ready_wait_and_unreacted_receipt_survive_a_model_outage() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "model-outage", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    complete_work(&db, &a).await;
    let event = deliver_fixture(&db, &a, "model-outage", now).await;
    let queued = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    let saved = read_session(&db, "model-outage").await;
    let mut next = claim_slice(
        &db,
        &params(&saved, "model-retry-turn", now),
        &queued.goal_id,
        Some(queued.state_version),
        "coordinator-b",
    )
    .await
    .unwrap()
    .unwrap();
    next.session
        .finish_turn(TurnState::Failed, now.to_rfc3339());
    let model_wait = settle_slice(
        &db,
        &mut next.session,
        &GoalSegmentEnd::WaitForModel {
            retry_after_unix_ms: None,
        },
        &[],
        now,
    )
    .await
    .unwrap();
    assert_eq!(model_wait.state, GoalState::Waiting(GoalWaitReason::Model));
    assert_eq!(
        model_wait.work_wait.as_ref().unwrap().ready_event_ids,
        vec![event]
    );
    assert_eq!(
        read_session(&db, "model-outage")
            .await
            .pending_auto_triggers
            .len(),
        1
    );

    let paused = apply_owner_action(
        &db,
        "model-outage",
        &next.session.actor_id,
        &next.session.device_id,
        &model_wait.goal_id,
        model_wait.state_version,
        GoalOwnerAction::Pause,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    let resumed = apply_owner_action(
        &db,
        "model-outage",
        &next.session.actor_id,
        &next.session.device_id,
        &paused.goal_id,
        paused.state_version,
        GoalOwnerAction::Resume,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        resumed.work_wait.as_ref().unwrap().ready,
        "owner resume must retain unreacted results"
    );
}

#[tokio::test]
async fn an_unknown_native_outcome_does_not_wait_forever_after_metadata_delivery() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "unknown-outcome", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    unknown_work(&db, &a).await;
    let mut saved = read_session(&db, "unknown-outcome").await;
    saved.execution_state = ExecutionState::None;
    store_fixture(&db, &saved).await;
    let paused = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Recovery));
    assert_eq!(
        paused.status_reason.as_deref(),
        Some("reconcile_interrupted_work")
    );
}

async fn unknown_work(db: &DatabaseConnection, action: &ActionIdentity) {
    let row = agent_action_item::Entity::find_by_id(action.work_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut record: BackgroundTaskRecord = serde_json::from_str(&row.payload_json).unwrap();
    record.state = BackgroundTaskState::OutcomeUnknown;
    record.terminal_at = Some(Utc::now().to_rfc3339());
    record.updated_at = record.terminal_at.clone().unwrap();
    record.validate().unwrap();
    agent_action_item::Entity::update_many()
        .set(agent_action_item::ActiveModel {
            status: Set("background_outcome_unknown".into()),
            payload_json: Set(serde_json::to_string(&record).unwrap()),
            result_json: Set(Some("{}".into())),
            ..Default::default()
        })
        .filter(agent_action_item::Column::Id.eq(action.work_id))
        .exec(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn provider_publication_precedes_model_reaction_and_is_replayable() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "published-not-reacted", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    complete_work(&db, &a).await;
    let store = SignalBackgroundTaskStore::new(db.clone());
    store.publish_once().await.unwrap();
    let row = agent_action_item::Entity::find_by_id(a.work_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.completion_delivery_state, "published");
    let first = read_session(&db, "published-not-reacted").await;
    assert_eq!(first.pending_auto_triggers.len(), 1);
    store.publish_once().await.unwrap();
    let second = read_session(&db, "published-not-reacted").await;
    assert_eq!(second.conversation, first.conversation);
    let queued = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued.state, GoalState::Queued);
    assert!(queued.work_wait.unwrap().ready);
}

#[tokio::test]
async fn background_progress_can_advance_the_clock_without_losing_a_held_goal_turn() {
    use desk_diagnose_core::seam::SessionSeam;
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "clock-rebase", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .save(&mut claimed.session)
        .await
        .unwrap();
    let row = agent_action_item::Entity::find_by_id(a.work_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let record: BackgroundTaskRecord = serde_json::from_str(&row.payload_json).unwrap();
    SignalBackgroundTaskStore::new(db.clone())
        .apply_progress(
            &desk_agent_protocol::capability_provider::CapabilityProgressEvent {
                task: record.task,
                sequence: 1,
                message_key: Some("report.progress".into()),
                completed_units: None,
                total_units: None,
            },
            &Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
    let previous = claimed.session.last_event_seq;
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .save(&mut claimed.session)
        .await
        .unwrap();
    assert_eq!(claimed.session.last_event_seq, previous + 1);
    let mut current = claimed.session.clone();
    current.version += 1;
    assert!(
        !crate::goal_work::rebase_background_events_on(&db, &current, &mut claimed.session)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn retention_preserves_claim_association_for_work_after_goal_cancellation() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "retained-origin", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let queued = settle_slice(&db, &mut claimed.session, &GoalSegmentEnd::Yield, &[], now)
        .await
        .unwrap();
    let cancelled = apply_owner_action(
        &db,
        "retained-origin",
        &claimed.session.actor_id,
        &claimed.session.device_id,
        &queued.goal_id,
        queued.state_version,
        GoalOwnerAction::Cancel,
        now,
    )
    .await
    .unwrap()
    .unwrap();

    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::UpdatedAt,
            Expr::value(now - chrono::Duration::days(40)),
        )
        .filter(agent_session::Column::ConversationId.eq("retained-origin"))
        .exec(&db)
        .await
        .unwrap();
    crate::usage_retention::cleanup_once(&db, now)
        .await
        .unwrap();

    let saved = read_session(&db, "retained-origin").await;
    let owner = crate::goal_work::owner_on(&db, &saved, &a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.goal_id, cancelled.goal_id);
    assert_eq!(owner.state, GoalState::Cancelled);
    assert!(
        crate::goal_work::claim_for_turn(&db, "retained-origin", "random-first-turn")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_new_wait_can_track_earlier_work_of_the_resumed_goal() {
    let db = db().await;
    let now = Utc::now();
    let mut claimed = claimed_fixture(&db, "resumed-dependency", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let old_wait =
        crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "before-stop")
            .await
            .unwrap()
            .unwrap();
    claimed.session.control_revision += 1;
    assert!(!old_wait.matches(&claimed.goal, claimed.session.control_revision));
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "after-resume")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wait.members, vec![a]);
    assert_eq!(wait.control_revision, claimed.session.control_revision);
    crate::goal_work::validate_wait_on(&db, &claimed.goal, &claimed.session, &wait)
        .await
        .unwrap();
}
