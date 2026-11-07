use super::*;
use crate::entity::{agent_goal_run as goal_row, agent_session};
use desk_diagnose_core::{
    ai_assistant::data_access::bind_opened_task,
    focus_epoch::GoalSegmentIdentity,
    goal::{GoalLimits, GoalModelBinding, GoalOpening, GoalRun},
    session::{PersistedAgentSession, TriggerOrigin},
};
use sea_orm::sea_query::Expr;

async fn fixture(label: &str) -> (TestStore, Box<PersistedAgentSession>, GoalRun, u64) {
    let (store, _, proof, _) = super::model_gate::fixture(label).await;
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let mut goal = GoalRun::new(
        "goal".into(),
        session.conversation_id.clone(),
        session.actor_id.clone(),
        desk_diagnose_core::dynamic_run::AssistantInputTarget::fixed(session.device_id.clone()),
        session.device_id.clone(),
        "Find the report".into(),
        "owner-message".into(),
        GoalOpening::OwnerRequest,
        GoalModelBinding::from_destination(&proof.source_envelope.allowed_destinations[0]).unwrap(),
        session.input_revision,
        now,
        GoalLimits::default(),
    )
    .unwrap();
    goal.deadline_unix_ms = now + 100_000;
    bind_opened_task(&mut session, &goal.goal_id, goal.deadline_unix_ms).unwrap();
    let txn = store.db.begin().await.unwrap();
    crate::agent_goal_store::insert_on(&txn, &goal)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    (store, session, goal, now)
}

// These cover the task-state SELECT and native lease component. Original
// owner/device/session and producer admission is tested by retrieval_native.
#[tokio::test]
async fn queued_goal_and_current_owner_correction_use_original_task_without_a_background_lease() {
    let (store, mut session, mut goal, now) = fixture("read-task-owner").await;
    let txn = store.db.begin().await.unwrap();
    assert_eq!(
        super::super::model_gate::task_deadline_on(&txn, &session, now)
            .await
            .unwrap(),
        Some(goal.deadline_unix_ms)
    );
    let old = (goal.state_version, goal.lease_epoch);
    goal.revise_input(session.input_revision + 1, now + 1)
        .unwrap();
    assert!(
        crate::agent_goal_store::replace_on(&txn, &goal, old.0, old.1, None, None)
            .await
            .unwrap()
    );
    assert!(
        super::super::model_gate::task_deadline_on(&txn, &session, now + 1)
            .await
            .is_err()
    );
    session.input_revision = goal.input_revision;
    assert_eq!(
        super::super::model_gate::task_deadline_on(&txn, &session, now + 1)
            .await
            .unwrap(),
        Some(goal.deadline_unix_ms)
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn claimed_goal_and_permission_resume_require_the_exact_slice_and_its_live_native_holder() {
    let (store, mut session, mut goal, now) = fixture("read-task-segment").await;
    let previous = (goal.state_version, goal.lease_epoch);
    goal.claim_slice(now + 1).unwrap();
    session.trigger_origin = TriggerOrigin::GoalContinuation;
    session.focus_epoch.goal_segment = Some(GoalSegmentIdentity {
        goal_id: goal.goal_id.clone(),
        source_message_id: goal.source_message_id.clone(),
        segment_seq: goal.slice_seq,
        goal_revision: goal.goal_revision,
        lease_epoch: goal.lease_epoch,
    });
    let txn = store.db.begin().await.unwrap();
    assert!(
        crate::agent_goal_store::replace_on(
            &txn,
            &goal,
            previous.0,
            previous.1,
            Some("planner"),
            Some(now + 5000)
        )
        .await
        .unwrap()
    );
    assert_eq!(
        super::super::model_gate::task_deadline_on(&txn, &session, now + 2)
            .await
            .unwrap(),
        Some(now + 5000)
    );
    session.trigger_origin = TriggerOrigin::PermissionDecision;
    assert_eq!(
        super::super::model_gate::task_deadline_on(&txn, &session, now + 2)
            .await
            .unwrap(),
        Some(now + 5000)
    );
    session
        .focus_epoch
        .goal_segment
        .as_mut()
        .unwrap()
        .lease_epoch += 1;
    assert!(
        super::super::model_gate::task_deadline_on(&txn, &session, now + 2)
            .await
            .is_err()
    );
    session
        .focus_epoch
        .goal_segment
        .as_mut()
        .unwrap()
        .lease_epoch -= 1;
    assert!(
        super::super::model_gate::task_deadline_on(&txn, &session, now + 5000)
            .await
            .is_err()
    );
    goal_row::Entity::update_many()
        .col_expr(goal_row::Column::LeaseOwner, Expr::value(None::<String>))
        .exec(&txn)
        .await
        .unwrap();
    assert!(
        super::super::model_gate::task_deadline_on(&txn, &session, now + 2)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn native_subject_cached_state_and_same_transaction_removal_are_not_replaced_by_session_labels()
 {
    for variant in 0..3 {
        let (store, session, _, now) = fixture(&format!("read-task-native-{variant}")).await;
        let txn = store.db.begin().await.unwrap();
        super::super::model_gate::task_deadline_on(&txn, &session, now)
            .await
            .unwrap();
        match variant {
            0 => {
                goal_row::Entity::update_many()
                    .col_expr(goal_row::Column::DeviceId, Expr::value("another-device"))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            1 => {
                goal_row::Entity::update_many()
                    .col_expr(goal_row::Column::Status, Expr::value("cancelled"))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            _ => {
                goal_row::Entity::delete_many().exec(&txn).await.unwrap();
            }
        }
        assert!(
            super::super::model_gate::task_deadline_on(&txn, &session, now)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
    }
}
