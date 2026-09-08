use super::*;
use crate::{
    entity::{agent_exec_task, agent_schedule, agent_schedule_run},
    schedule_store::{ContinuationClaim, ScheduleStore},
};
use desk_agent_protocol::schedule::{
    ScheduleCreationSource, ScheduleDraft, ScheduleRule, ScheduleSpec, ScheduledTaskKind,
};
use desk_diagnose_core::{
    seam::SessionSeam,
    session::{TurnState, WorkKind},
};

mod commands;

pub(in crate::capability_grant_store::tests::computer_binding) async fn claim_original(
    db: &DatabaseConnection,
    session: &mut PersistedAgentSession,
) {
    let schema = Schema::new(db.get_database_backend());
    for statement in [
        schema.create_table_from_entity(agent_schedule::Entity),
        schema.create_table_from_entity(agent_schedule_run::Entity),
        schema.create_table_from_entity(agent_exec_task::Entity),
    ] {
        db.execute(&statement).await.unwrap();
    }
    assert_eq!(session.actor_id, "1");
    // Create the occurrence before preparing or signing any original action.
    let mut proposal = session.conversation.pop().unwrap();
    session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
    session.handled_input_seq = session.latest_input_seq;
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    sessions.save(session).await.unwrap();
    let store = ScheduleStore::new(db.clone());
    let now = store.database_time().await.unwrap();
    let at = chrono::DateTime::from_timestamp_millis(now + 3000).unwrap();
    let draft = ScheduleDraft {
        time_confirmation: None,
        client_create_key: "native-recovery".into(),
        kind: ScheduledTaskKind::ConversationResume,
        target_device_id: session.device_id.clone(),
        title: "Resume original input".into(),
        prompt: "Continue the original task".into(),
        locale: None,
        model_id: None,
        spec: ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Once {
                at: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
        },
        source_conversation_id: Some(session.conversation_id.clone()),
        requirement_revision: Some(session.input_revision),
        creation_source: ScheduleCreationSource::Manual,
    };
    let task = store.create_draft(1, &draft, now).await.unwrap();
    let task = store
        .activate_conversation_resume(1, &task.schedule_id, task.revision)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let queued = store
        .materialize_due(&task.schedule_id, task.revision)
        .await
        .unwrap()
        .unwrap();
    let claimed = store
        .claim_conversation_resume(ContinuationClaim {
            owner: 1,
            run_id: &queued.run_id,
            node_id: "original-native-executor",
            lease_seconds: 90,
            policy_revision: session.policy_revision,
            scope: session.scope_snapshot.clone(),
        })
        .await
        .unwrap();
    *session = claimed.session;
    proposal.turn_id = session.current_turn_id.clone();
    session.conversation.push(proposal);
    sessions.save(session).await.unwrap();
}

async fn run(f: &Fixture) -> agent_schedule_run::Model {
    agent_schedule_run::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap()
}

async fn session(f: &Fixture) -> agent_session::Model {
    agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap()
}

async fn expire(f: &Fixture, cancelled: bool) {
    let now = Utc::now();
    let mut work: agent_schedule_run::ActiveModel = run(f).await.into();
    work.lease_deadline = Set(Some(now.timestamp_millis() - 1));
    if cancelled {
        work.cancel_requested_at = Set(Some(now.timestamp_millis()));
    }
    work.update(&f.store.db).await.unwrap();
    let mut row: agent_session::ActiveModel = session(f).await.into();
    row.lease_deadline = Set(Some(now - chrono::Duration::seconds(1)));
    row.update(&f.store.db).await.unwrap();
}

#[tokio::test]
async fn scheduled_native_recovery_commits_original_occurrence_once_and_rolls_back_together() {
    for (unknown, cancelled) in [(false, false), (false, true), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let f =
            Fixture::new_with_schedule(file_db(&dir.path().join("schedule.db")).await, "1", true)
                .await;
        f.bind().await;
        if !unknown {
            f.store
                .accept_computer_completion(
                    "host-original",
                    "device-1",
                    &f.plan.execution_generation,
                    &completion::verified(&f.plan),
                )
                .await
                .unwrap();
        }
        let store = ScheduleStore::new(f.store.db.clone());
        let original = run(&f).await;
        // A still-held paired lease cannot be recovered.
        assert!(
            store
                .recover_committed_continuation(1, &original.run_id)
                .await
                .is_err()
        );
        expire(&f, cancelled).await;
        let before = session(&f).await;
        let before_run = run(&f).await;
        let task = store.read(1, &original.schedule_id).await.unwrap();
        let work = agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap();
        let outbox = f.outbox().await;
        let grant = agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap();
        let mut overflow: agent_schedule::ActiveModel = task.clone().into();
        overflow.revision = Set(i64::MAX);
        overflow.update(&f.store.db).await.unwrap();
        assert!(
            store
                .recover_committed_continuation(1, &original.run_id)
                .await
                .is_err()
        );
        assert_eq!(session(&f).await, before);
        assert_eq!(run(&f).await, before_run);
        let restored: agent_schedule::ActiveModel = task.into();
        restored.reset_all().update(&f.store.db).await.unwrap();
        let settled = store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            settled.status,
            if unknown {
                "outcome_unknown"
            } else if cancelled {
                "cancelled"
            } else {
                "failed"
            }
        );
        assert!(settled.failure_accounted);
        assert_eq!(settled.attempt, 1);
        assert_eq!(settled.lease_epoch, original.lease_epoch);
        let after = session(&f).await;
        assert_eq!(after.version, before.version + 1);
        assert_eq!(after.lease_token, before.lease_token);
        assert!(after.lease_deadline.is_none());
        let projected = PersistedAgentSession::decode_json(&after.state_json).unwrap();
        assert_eq!(projected.turn_state, TurnState::Failed);
        assert!(projected.unclosed_tool_call_ids().is_empty());
        assert_eq!(
            matches!(
                projected.execution_state,
                ExecutionState::OutcomeUnknown { .. }
            ),
            unknown
        );
        let task = store.read(1, &original.schedule_id).await.unwrap();
        assert!(task.active_run_id.is_none());
        let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json).unwrap();
        assert_eq!(
            failures
                .pause_reasons
                .contains(&desk_agent_protocol::schedule::SchedulePauseReason::UnknownSideEffect),
            unknown
        );
        assert_eq!(
            store
                .recover_committed_continuation(1, &original.run_id)
                .await
                .unwrap()
                .unwrap(),
            settled
        );
        assert_eq!(session(&f).await, after);
        assert_eq!(f.outbox().await, outbox);
        assert_eq!(
            agent_action_item::Entity::find()
                .one(&f.store.db)
                .await
                .unwrap(),
            work
        );
        assert_eq!(
            agent_capability_grant::Entity::find()
                .one(&f.store.db)
                .await
                .unwrap(),
            grant
        );
    }
}

#[tokio::test]
async fn scheduled_native_recovery_keeps_active_slot_until_original_background_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let f =
        Fixture::new_with_schedule(file_db(&dir.path().join("waiting.db")).await, "1", true).await;
    f.bind().await;
    f.accept().await.unwrap();
    let mut outbox: agent_capability_dispatch_outbox::ActiveModel = f.outbox().await.into();
    outbox.created_at = Set(Utc::now() - chrono::Duration::seconds(9));
    outbox.update(&f.store.db).await.unwrap();
    assert!(
        f.store
            .promote_computer_background(&f.plan.execution_generation, "run-1", "1", "device-1",)
            .await
            .unwrap()
    );
    expire(&f, false).await;
    let before = session(&f).await;
    let original = run(&f).await;
    let store = ScheduleStore::new(f.store.db.clone());
    let outbox = f.outbox().await;
    let waiting = store
        .recover_committed_continuation(1, &original.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(waiting, original);
    let held = session(&f).await;
    assert_eq!(held.version, before.version + 1);
    assert_eq!(held.lease_token, before.lease_token);
    assert!(held.lease_deadline.is_none());
    let projected = PersistedAgentSession::decode_json(&held.state_json).unwrap();
    assert_eq!(projected.turn_state, TurnState::Running);
    assert!(
        matches!(&projected.execution_state, ExecutionState::Executing { action }
        if action.execution_id == f.plan.execution_generation)
    );
    assert_eq!(
        store
            .read(1, &original.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        Some(original.run_id.as_str())
    );
    assert!(
        store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .is_err()
    );
    assert_eq!(session(&f).await, held);
    assert_eq!(f.outbox().await, outbox);
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            &completion::verified(&f.plan),
        )
        .await
        .unwrap();
    let receipt = f.outbox().await;
    let settled = store
        .recover_committed_continuation(1, &original.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.status, "failed");
    assert_eq!(settled.error_kind.as_deref(), Some("executor_interrupted"));
    assert_eq!(settled.attempt, original.attempt);
    assert_eq!(settled.lease_epoch, original.lease_epoch);
    assert!(
        store
            .read(1, &original.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .is_none()
    );
    let final_row = session(&f).await;
    let final_session = PersistedAgentSession::decode_json(&final_row.state_json).unwrap();
    assert_eq!(final_row.version, held.version + 1);
    assert_eq!(final_session.execution_state, ExecutionState::None);
    assert_eq!(final_session.turn_state, TurnState::Failed);
    assert_eq!(
        store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .unwrap()
            .unwrap(),
        settled
    );
    assert_eq!(session(&f).await, final_row);
    assert_eq!(f.outbox().await, receipt);
}

#[tokio::test]
async fn scheduled_prepared_recovery_releases_only_unused_reservation_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new_at_stage(
        file_db(&dir.path().join("prepared.db")).await,
        "1",
        true,
        true,
    )
    .await;
    let store = ScheduleStore::new(f.store.db.clone());
    expire(&f, false).await;
    let original = run(&f).await;
    let before = session(&f).await;
    let work = agent_action_item::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let reservation = agent_grant_reservation::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let grant = agent_capability_grant::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(grant.remaining_uses, 0);
    assert_eq!(reservation.state, RESERVATION_STATUS_RESERVED);
    assert_eq!(
        agent_capability_dispatch_outbox::Entity::find()
            .count(&f.store.db)
            .await
            .unwrap(),
        0
    );
    let task = store.read(1, &original.schedule_id).await.unwrap();
    let mut overflow: agent_schedule::ActiveModel = task.clone().into();
    overflow.revision = Set(i64::MAX);
    overflow.update(&f.store.db).await.unwrap();
    assert!(
        store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .is_err()
    );
    assert_eq!(session(&f).await, before);
    assert_eq!(run(&f).await, original);
    assert_eq!(
        agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap(),
        work
    );
    assert_eq!(
        agent_grant_reservation::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap(),
        reservation
    );
    assert_eq!(
        agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap(),
        grant
    );
    let restored: agent_schedule::ActiveModel = task.into();
    restored.reset_all().update(&f.store.db).await.unwrap();
    // A committed use is not evidence that the action was never dispatched.
    let mut committed: agent_grant_reservation::ActiveModel = reservation.clone().into();
    committed.state = Set(RESERVATION_STATUS_COMMITTED.into());
    committed.update(&f.store.db).await.unwrap();
    assert!(
        store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .is_err()
    );
    assert_eq!(session(&f).await, before);
    assert_eq!(
        agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap(),
        grant
    );
    let restored: agent_grant_reservation::ActiveModel = reservation.into();
    restored.reset_all().update(&f.store.db).await.unwrap();
    let settled = store
        .recover_committed_continuation(1, &original.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.status, "failed");
    assert_eq!(settled.attempt, 1);
    let after = session(&f).await;
    let projected = PersistedAgentSession::decode_json(&after.state_json).unwrap();
    assert!(projected.unclosed_tool_call_ids().is_empty());
    assert_eq!(projected.execution_state, ExecutionState::None);
    assert_eq!(
        agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
            .status,
        CAPABILITY_WORK_SUPERSEDED
    );
    assert_eq!(
        agent_grant_reservation::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
            .state,
        RESERVATION_STATUS_RELEASED
    );
    let restored = agent_capability_grant::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.remaining_uses, 1);
    assert_eq!(
        store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .unwrap()
            .unwrap(),
        settled
    );
    assert_eq!(session(&f).await, after);
    assert_eq!(
        agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap(),
        restored
    );
    assert_eq!(
        agent_capability_dispatch_outbox::Entity::find()
            .count(&f.store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn unbound_dispatch_intent_pauses_without_refund_or_redispatch() {
    for intent_only in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let f = Fixture::new_before_binding(
            file_db(&dir.path().join("unbound.db")).await,
            "1",
            true,
            false,
            intent_only,
        )
        .await;
        let store = ScheduleStore::new(f.store.db.clone());
        let original = run(&f).await;
        expire(&f, true).await;
        let before = session(&f).await;
        let before_run = run(&f).await;
        let before_outbox = f.outbox().await;
        let before_action = agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap();
        let grant = agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap();
        let reservation = agent_grant_reservation::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap();
        let task = store.read(1, &original.schedule_id).await.unwrap();
        let mut overflow: agent_schedule::ActiveModel = task.clone().into();
        overflow.revision = Set(i64::MAX);
        overflow.update(&f.store.db).await.unwrap();
        assert!(
            store
                .recover_committed_continuation(1, &original.run_id)
                .await
                .is_err()
        );
        assert_eq!(session(&f).await, before);
        assert_eq!(run(&f).await, before_run);
        assert_eq!(f.outbox().await, before_outbox);
        assert_eq!(
            agent_action_item::Entity::find()
                .one(&f.store.db)
                .await
                .unwrap(),
            Some(before_action.clone())
        );
        let restored: agent_schedule::ActiveModel = task.into();
        restored.reset_all().update(&f.store.db).await.unwrap();
        let settled = store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settled.status, "outcome_unknown");
        assert_eq!(settled.attempt, original.attempt);
        assert_eq!(settled.lease_epoch, original.lease_epoch);
        let after = session(&f).await;
        assert_eq!(after.lease_token, before.lease_token);
        let projected = PersistedAgentSession::decode_json(&after.state_json).unwrap();
        assert!(projected.unclosed_tool_call_ids().is_empty());
        assert_eq!(
            projected.execution_state.waitable_task().unwrap().kind,
            WorkKind::CapabilityProvider
        );
        assert_eq!(f.outbox().await.state, DISPATCH_OUTBOX_OUTCOME_UNKNOWN);
        let action = agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(action.status, CAPABILITY_WORK_OUTCOME_UNKNOWN);
        assert_eq!(action.attempt, before_action.attempt);
        assert_eq!(
            agent_capability_grant::Entity::find()
                .one(&f.store.db)
                .await
                .unwrap(),
            grant
        );
        assert_eq!(
            agent_grant_reservation::Entity::find()
                .one(&f.store.db)
                .await
                .unwrap(),
            reservation
        );
        assert!(!matches!(
            f.store
                .claim_dispatch(
                    &before_outbox.dispatch_id,
                    Utc::now().timestamp_millis() as u64
                )
                .await
                .unwrap(),
            DispatchClaimResult::Claimed(_)
        ));
        let task = store.read(1, &original.schedule_id).await.unwrap();
        assert!(task.active_run_id.is_none());
        assert!(task.next_run_at.is_none());
        store
            .recover_committed_continuation(1, &original.run_id)
            .await
            .unwrap();
        assert_eq!(session(&f).await, after);
    }
}
