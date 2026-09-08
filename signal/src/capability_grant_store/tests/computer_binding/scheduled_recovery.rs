use super::*;
use desk_diagnose_core::session::{ExecutionState, TriggerOrigin};

pub(super) mod integration;

// These tests exercise the transaction-local projection. Schedule lease and
// occurrence checks remain the responsibility of the enclosing store tests.
async fn reconcile(f: &Fixture, session: &mut PersistedAgentSession) -> Result<(), DbErr> {
    let txn = f.store.db.begin().await?;
    crate::capability_grant_store::scheduled_recovery::reconcile_on(
        &txn,
        session,
        Utc::now().timestamp_millis() as u64,
    )
    .await?;
    txn.commit().await
}

async fn fixture(path: &std::path::Path) -> Fixture {
    let mut f = Fixture::new(file_db(path).await).await;
    // Freeze the scheduled origin before binding any native receipt.
    f.session.trigger_origin = TriggerOrigin::ScheduledContinuation;
    f.bind().await;
    f
}

#[tokio::test]
async fn scheduled_recovery_reuses_exact_receipt_without_consuming_or_dispatching() {
    for verified in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(&dir.path().join("receipt.db")).await;
        let native = if verified {
            completion::verified(&f.plan)
        } else {
            completion::failed(&f.plan)
        };
        f.store
            .accept_computer_completion(
                "host-original",
                "device-1",
                &f.plan.execution_generation,
                &native,
            )
            .await
            .unwrap();
        let outbox = f.outbox().await;
        let work = agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap();
        let grant = agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap();
        let mut session = f.session.clone();
        reconcile(&f, &mut session).await.unwrap();
        assert!(session.unclosed_tool_call_ids().is_empty());
        assert_eq!(session.execution_state, ExecutionState::None);
        let result = f
            .store
            .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .unwrap();
        let message = session
            .conversation
            .iter()
            .find(|m| m.message_id == result.work.completion_event_id)
            .unwrap();
        assert_eq!(message.text, result.output.content);
        assert_eq!(
            message.data_envelope.as_ref(),
            Some(&result.receipt.envelope)
        );
        let first = session.clone();
        reconcile(&f, &mut session).await.unwrap();
        assert_eq!(session, first);
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
async fn scheduled_recovery_preserves_running_then_accepts_original_late_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(&dir.path().join("running.db")).await;
    f.accept().await.unwrap();
    let mut row: agent_capability_dispatch_outbox::ActiveModel = f.outbox().await.into();
    row.created_at = Set(Utc::now() - chrono::Duration::seconds(9));
    row.update(&f.store.db).await.unwrap();
    assert!(
        f.store
            .promote_computer_background(
                &f.plan.execution_generation,
                "run-1",
                "actor-1",
                "device-1",
            )
            .await
            .unwrap()
    );
    let outbox = f.outbox().await;
    let mut session = f.session.clone();
    reconcile(&f, &mut session).await.unwrap();
    assert!(
        matches!(&session.execution_state, ExecutionState::Executing { action }
        if action.execution_id == f.plan.execution_generation)
    );
    let first = session.clone();
    reconcile(&f, &mut session).await.unwrap();
    assert_eq!(session, first);
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
    reconcile(&f, &mut session).await.unwrap();
    assert_eq!(session.execution_state, ExecutionState::None);
    assert!(session.unclosed_tool_call_ids().is_empty());
    let first = session.clone();
    reconcile(&f, &mut session).await.unwrap();
    assert_eq!(session, first);
}

#[tokio::test]
async fn scheduled_recovery_missing_acceptance_is_unknown_and_wrong_turn_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(&dir.path().join("unknown.db")).await;
    let outbox = f.outbox().await;
    let mut session = f.session.clone();
    reconcile(&f, &mut session).await.unwrap();
    assert!(
        matches!(&session.execution_state, ExecutionState::OutcomeUnknown { action, .. }
        if action.execution_id == f.plan.execution_generation)
    );
    let first = session.clone();
    reconcile(&f, &mut session).await.unwrap();
    assert_eq!(session, first);
    let mut replaced = f.session.clone();
    replaced.lease_token += 1;
    assert!(reconcile(&f, &mut replaced).await.is_err());
    assert_eq!(f.outbox().await, outbox);
    assert_eq!(
        agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
            .remaining_uses,
        0
    );
}
