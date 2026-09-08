use super::*;

struct Allow;
#[async_trait::async_trait(?Send)]
impl super::super::ScheduleAuthorizer for Allow {
    async fn authorize(
        &self,
        _: &sea_orm::DatabaseTransaction,
        _: &entity::Model,
    ) -> Result<(), ScheduleStoreError> {
        Ok(())
    }
}
struct Deny;
#[async_trait::async_trait(?Send)]
impl super::super::ScheduleAuthorizer for Deny {
    async fn authorize(
        &self,
        _: &sea_orm::DatabaseTransaction,
        _: &entity::Model,
    ) -> Result<(), ScheduleStoreError> {
        Err(ScheduleStoreError::Invalid)
    }
}

#[tokio::test]
async fn resume_requires_current_authority_and_starts_a_fresh_future_epoch() {
    use desk_agent_protocol::schedule::SchedulePauseReason;
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let (store, task) = fixture(
        ScheduleRule::Daily {
            utc_time: "06:00:00".into(),
        },
        now - 172_800_000,
    )
    .await;
    let paused = store.pause(1, &task.schedule_id, 1, now).await.unwrap();
    assert!(matches!(
        store
            .resume(1, &task.schedule_id, paused.revision, &Deny)
            .await,
        Err(ScheduleStoreError::Invalid)
    ));
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), paused);
    let resumed = store
        .resume(1, &task.schedule_id, paused.revision, &Allow)
        .await
        .unwrap();
    assert_eq!(resumed.status, "active");
    assert!(resumed.next_run_at.unwrap() > now);
    let failure: FailureState = serde_json::from_str(&resumed.failure_state_json).unwrap();
    assert_eq!(failure.recovery_epoch, 2);
    assert!(failure.pause_reasons.is_empty());
    let mut failure = failure;
    failure
        .pause_reasons
        .insert(SchedulePauseReason::UnknownSideEffect);
    entity::Entity::update_many()
        .set(entity::ActiveModel {
            status: Set("paused".into()),
            next_run_at: Set(None),
            failure_state_json: Set(json(&failure).unwrap()),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(matches!(
        store
            .resume(1, &task.schedule_id, resumed.revision, &Allow)
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn past_once_cannot_resume_until_explicitly_rescheduled() {
    use desk_agent_protocol::schedule::ScheduleSpec;
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let at = now - 60_000;
    let (store, task) = fixture(
        ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        },
        at,
    )
    .await;
    let paused = store.pause(1, &task.schedule_id, 1, now).await.unwrap();
    assert!(matches!(
        store
            .resume(1, &task.schedule_id, paused.revision, &Allow)
            .await,
        Err(ScheduleStoreError::Invalid)
    ));
    let spec = ScheduleSpec {
        schema_version: 1,
        rule: ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(now + 600_000)
                .unwrap()
                .to_rfc3339(),
        },
    };
    let edited = store
        .change_time(1, &task.schedule_id, paused.revision, &spec)
        .await
        .unwrap();
    let resumed = store
        .resume(1, &task.schedule_id, edited.revision, &Allow)
        .await
        .unwrap();
    assert_eq!(resumed.next_run_at, Some(now + 600_000));
}

#[tokio::test]
async fn edits_preserve_running_snapshot_and_do_not_reactivate_a_changed_requirement() {
    use desk_agent_protocol::schedule::ScheduledRunStatus;
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let at = now - 30_000;
    let (store, task) = fixture(
        ScheduleRule::Interval {
            every_seconds: 60,
            anchor_at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        },
        at,
    )
    .await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let claimed = store.claim_queued(&work.run_id, "node", 90).await.unwrap();
    let current = store.read(1, &task.schedule_id).await.unwrap();
    assert!(
        store
            .change_prompt(
                1,
                &task.schedule_id,
                current.revision,
                &"x".repeat(desk_diagnose_core::schedule::MAX_SCHEDULE_PROMPT_BYTES + 1)
            )
            .await
            .is_err()
    );
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), current);
    let revised = store
        .change_prompt(1, &task.schedule_id, current.revision, "A different task")
        .await
        .unwrap();
    assert_eq!(current.task_revision, 1);
    assert!(current.revision > current.task_revision);
    assert_eq!(revised.task_revision, 2);
    assert_eq!(revised.status, "draft");
    let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json).unwrap();
    assert_eq!(snapshot.task_revision, 1);
    assert!(revised.contract_revision.is_none());
    assert!(revised.authorization_revision.is_none());
    let old = run::Entity::find()
        .filter(run::Column::RunId.eq(&work.run_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.task_snapshot_json, work.task_snapshot_json);
    store
        .finish_run(
            &work.run_id,
            "node",
            claimed.lease_epoch,
            ScheduledRunStatus::Succeeded,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.read(1, &task.schedule_id).await.unwrap().status,
        "draft"
    );
}

#[tokio::test]
async fn no_op_time_save_and_rename_keep_utc_and_authorization_unchanged() {
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let (store, task) = fixture(
        ScheduleRule::Daily {
            utc_time: "06:00:00".into(),
        },
        now + 120_000,
    )
    .await;
    let before = store.read(1, &task.schedule_id).await.unwrap();
    let spec = parse_json(&before.spec_json).unwrap();
    let unchanged = store
        .change_time(1, &task.schedule_id, before.revision, &spec)
        .await
        .unwrap();
    assert_eq!(unchanged, before);
    let renamed = store
        .rename(1, &task.schedule_id, before.revision, "New display name")
        .await
        .unwrap();
    assert_eq!(renamed.task_revision, before.task_revision);
    assert_eq!(renamed.spec_json, before.spec_json);
    assert_eq!(renamed.next_run_at, before.next_run_at);
    assert_eq!(
        renamed.authorization_revision,
        before.authorization_revision
    );
    assert!(matches!(
        store
            .change_time(1, &task.schedule_id, before.revision, &spec)
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn deleting_pending_task_keeps_history_and_prevents_recreation_by_retry() {
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let at = now - 30_000;
    let (store, task) = fixture(
        ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        },
        at,
    )
    .await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let current = store.read(1, &task.schedule_id).await.unwrap();
    let deleted = store
        .delete(1, &task.schedule_id, current.revision)
        .await
        .unwrap();
    assert_eq!(deleted.status, "deleted");
    assert!(deleted.active_run_id.is_none());
    assert!(store.list(1, 0, 100).await.unwrap().is_empty());
    assert_eq!(
        run::Entity::find()
            .filter(run::Column::RunId.eq(work.run_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    assert!(matches!(
        store
            .change_prompt(1, &task.schedule_id, deleted.revision, "Recreate")
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn cancel_unstarted_is_terminal_but_running_only_records_intent() {
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let at = now - 30_000;
    let rule = ScheduleRule::Once {
        at: chrono::DateTime::from_timestamp_millis(at)
            .unwrap()
            .to_rfc3339(),
    };
    let (store, task) = fixture(rule.clone(), at).await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        store.cancel_run(2, &work.run_id).await,
        Err(ScheduleStoreError::NotFound)
    ));
    let cancelled = store.cancel_run(1, &work.run_id).await.unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert!(cancelled.failure_accounted);
    assert!(
        store
            .read(1, &task.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .is_none()
    );
    let (store, task) = fixture(rule, at).await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let claimed = store.claim_queued(&work.run_id, "node", 90).await.unwrap();
    let cancelled = store.cancel_run(1, &work.run_id).await.unwrap();
    assert_eq!(cancelled.status, "running");
    assert!(!cancelled.failure_accounted);
    assert!(
        store
            .cancellation_requested(&work.run_id, "node", claimed.lease_epoch)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .read(1, &task.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        Some(work.run_id.as_str())
    );
}

#[tokio::test]
async fn manual_run_has_separate_identity_and_leaves_calendar_unchanged() {
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let future = now + 600_000;
    let (store, task) = fixture(
        ScheduleRule::Daily {
            utc_time: "06:00:00".into(),
        },
        future,
    )
    .await;
    assert!(matches!(
        store.enqueue_manual(1, &task.schedule_id, "manual-1").await,
        Err(ScheduleStoreError::Conflict)
    ));
    entity::Entity::update_many()
        .set(entity::ActiveModel {
            contract_revision: Set(Some(1)),
            authorization_revision: Set(Some(1)),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .exec(&store.db)
        .await
        .unwrap();
    let first = store
        .enqueue_manual(1, &task.schedule_id, "manual-1")
        .await
        .unwrap();
    assert!(first.scheduled_at.is_none());
    assert_eq!(first.source, "manual");
    assert_eq!(
        store
            .enqueue_manual(1, &task.schedule_id, "manual-1")
            .await
            .unwrap(),
        first
    );
    assert!(matches!(
        store.enqueue_manual(1, &task.schedule_id, "manual-2").await,
        Err(ScheduleStoreError::Conflict)
    ));
    let saved = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(saved.next_run_at, Some(future));
    assert_eq!(saved.active_run_id.as_deref(), Some(first.run_id.as_str()));
}

#[tokio::test]
async fn finalization_counts_once_and_threshold_prevents_the_next_claim() {
    use desk_agent_protocol::schedule::{SchedulePauseReason, ScheduledRunStatus};
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let at = now - 30_000;
    let (store, task) = fixture(
        ScheduleRule::Interval {
            every_seconds: 60,
            anchor_at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        },
        at,
    )
    .await;
    let mut failure = FailureState::default();
    failure.consecutive_failures = 2;
    entity::Entity::update_many()
        .set(entity::ActiveModel {
            failure_state_json: Set(json(&failure).unwrap()),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .exec(&store.db)
        .await
        .unwrap();
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let claimed = store
        .claim_queued(&work.run_id, "node-a", 90)
        .await
        .unwrap();
    assert!(matches!(
        store
            .finish_run(
                &work.run_id,
                "node-b",
                claimed.lease_epoch,
                ScheduledRunStatus::Failed,
                None,
                None
            )
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
    let result = store
        .finish_run(
            &work.run_id,
            "node-a",
            claimed.lease_epoch,
            ScheduledRunStatus::Failed,
            Some("model_error".into()),
            None,
        )
        .await
        .unwrap();
    assert!(result.failure_accounted);
    let duplicate = store
        .finish_run(
            &work.run_id,
            "node-a",
            claimed.lease_epoch,
            ScheduledRunStatus::Failed,
            Some("model_error".into()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result, duplicate);
    let saved = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(saved.status, "paused");
    assert!(saved.next_run_at.is_none());
    assert!(saved.active_run_id.is_none());
    let failure: FailureState = serde_json::from_str(&saved.failure_state_json).unwrap();
    assert_eq!(failure.consecutive_failures, 3);
    assert!(
        failure
            .pause_reasons
            .contains(&SchedulePauseReason::ConsecutiveFailures)
    );
    assert!(store.due_candidates(0, 32).await.unwrap().is_empty());
}

#[tokio::test]
async fn unknown_effect_pauses_without_waiting_for_failure_threshold() {
    use desk_agent_protocol::schedule::{SchedulePauseReason, ScheduledRunStatus};
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let at = now - 30_000;
    let (store, task) = fixture(
        ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        },
        at,
    )
    .await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let claimed = store
        .claim_queued(&work.run_id, "node-a", 90)
        .await
        .unwrap();
    store
        .finish_run(
            &work.run_id,
            "node-a",
            claimed.lease_epoch,
            ScheduledRunStatus::OutcomeUnknown,
            Some("receipt_unknown".into()),
            None,
        )
        .await
        .unwrap();
    let saved = store.read(1, &task.schedule_id).await.unwrap();
    let failure: FailureState = serde_json::from_str(&saved.failure_state_json).unwrap();
    assert_eq!(saved.status, "paused");
    assert_eq!(failure.consecutive_failures, 0);
    assert!(
        failure
            .pause_reasons
            .contains(&SchedulePauseReason::UnknownSideEffect)
    );
}
use desk_agent_protocol::schedule::ScheduleRule;
use sea_orm::{Database, Schema};

async fn fixture(rule: ScheduleRule, next: i64) -> (ScheduleStore, entity::Model) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    let schema = Schema::new(db.get_database_backend());
    db.execute(&schema.create_table_from_entity(entity::Entity))
        .await
        .unwrap();
    db.execute(&schema.create_table_from_entity(run::Entity))
        .await
        .unwrap();
    for index in schema
        .create_index_from_entity(entity::Entity)
        .into_iter()
        .chain(schema.create_index_from_entity(run::Entity))
    {
        db.execute(&index).await.unwrap();
    }
    let store = ScheduleStore::new(db);
    let mut draft = super::super::tests::draft();
    draft.spec.rule = rule;
    let row = store.create_draft(1, &draft, next).await.unwrap();
    entity::Entity::update_many()
        .set(entity::ActiveModel {
            status: Set("active".into()),
            next_run_at: Set(Some(next)),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(row.id))
        .exec(&store.db)
        .await
        .unwrap();
    (store, row)
}

#[tokio::test]
async fn due_materialization_claim_and_renew_are_fenced() {
    let time_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&time_db).await.unwrap();
    let at = now - 120_000;
    let rule = ScheduleRule::Interval {
        every_seconds: 60,
        anchor_at: chrono::DateTime::from_timestamp_millis(at)
            .unwrap()
            .to_rfc3339(),
    };
    let (store, task) = fixture(rule, at).await;
    let candidates = store.due_candidates(0, 32).await.unwrap();
    assert_eq!(candidates.len(), 1);
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(work.status, "queued");
    assert!(work.missed_count >= 2);
    assert_eq!(work.conversation_id, work.run_id);
    assert!(matches!(
        store.materialize_due(&task.schedule_id, 1).await,
        Err(ScheduleStoreError::Conflict)
    ));
    let claimed = store
        .claim_queued(&work.run_id, "node-a", 90)
        .await
        .unwrap();
    assert_eq!(claimed.lease_epoch, 1);
    assert!(matches!(
        store.claim_queued(&work.run_id, "node-b", 90).await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert!(
        !store
            .renew_run(&work.run_id, "node-b", 1, 90)
            .await
            .unwrap()
    );
    assert!(
        !store
            .renew_run(&work.run_id, "node-a", 0, 90)
            .await
            .unwrap()
    );
    assert!(
        store
            .renew_run(&work.run_id, "node-a", 1, 90)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn once_expiry_completes_without_claim_or_failure_charge() {
    let time_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&time_db).await.unwrap();
    let at = now - 7_200_000;
    let (store, task) = fixture(
        ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        },
        at,
    )
    .await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(work.status, "missed");
    assert!(work.failure_accounted);
    let saved = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(saved.status, "completed");
    assert!(saved.active_run_id.is_none());
    assert!(saved.next_run_at.is_none());
    assert!(matches!(
        store.claim_queued(&work.run_id, "node", 90).await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn pause_between_materialization_and_claim_blocks_dispatch() {
    let time_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&time_db).await.unwrap();
    let at = now - 30_000;
    let (store, task) = fixture(
        ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        },
        at,
    )
    .await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let saved = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(saved.status, "triggered");
    store
        .pause(1, &task.schedule_id, saved.revision, now)
        .await
        .unwrap();
    assert!(matches!(
        store.claim_queued(&work.run_id, "node", 90).await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn pending_expiry_releases_slot_and_charges_only_device_timeout_once() {
    for offline in [false, true] {
        let clock_db = Database::connect("sqlite::memory:").await.unwrap();
        let now = database_now(&clock_db).await.unwrap();
        let (store, task) = fixture(
            ScheduleRule::Daily {
                utc_time: "06:00:00".into(),
            },
            now - 1000,
        )
        .await;
        // Use a valid daily occurrence to exercise the actual calendar materializer.
        let spec = parse_json(&task.spec_json).unwrap();
        let due = desk_diagnose_core::schedule::next_after(&spec, now - 86_400_001)
            .unwrap()
            .unwrap();
        entity::Entity::update_many()
            .set(entity::ActiveModel {
                next_run_at: Set(Some(due)),
                grace_seconds: Set(172_800),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .exec(&store.db)
            .await
            .unwrap();
        let work = store
            .materialize_due(&task.schedule_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.status, "queued");
        assert!(matches!(
            store.expire_pending(&work.run_id).await,
            Err(ScheduleStoreError::Conflict)
        ));
        let mut failures = FailureState::default();
        failures.consecutive_failures = 2;
        entity::Entity::update_many()
            .set(entity::ActiveModel {
                failure_state_json: Set(json(&failures).unwrap()),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .exec(&store.db)
            .await
            .unwrap();
        if offline {
            store.wait_for_device(&work.run_id).await.unwrap();
        }
        run::Entity::update_many()
            .set(run::ActiveModel {
                start_deadline: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .exec(&store.db)
            .await
            .unwrap();
        assert_eq!(store.expired_pending(0, 100).await.unwrap().len(), 1);
        let expired = store.expire_pending(&work.run_id).await.unwrap();
        assert_eq!(expired.status, "missed");
        assert!(expired.failure_accounted);
        assert_eq!(
            expired.error_kind.as_deref(),
            Some(if offline {
                "device_offline_timeout"
            } else {
                "queue_timeout"
            })
        );
        assert_eq!(store.expire_pending(&work.run_id).await.unwrap(), expired);
        assert!(store.expired_pending(0, 100).await.unwrap().is_empty());
        let task = store.read(1, &task.schedule_id).await.unwrap();
        assert!(task.active_run_id.is_none());
        let failures: FailureState = serde_json::from_str(&task.failure_state_json).unwrap();
        assert_eq!(failures.consecutive_failures, if offline { 3 } else { 2 });
        assert_eq!(task.status, if offline { "paused" } else { "active" });
        assert_eq!(task.next_run_at.is_none(), offline);
    }
}

#[tokio::test]
async fn expired_execution_lease_is_never_treated_as_unstarted_work() {
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let (store, task) = fixture(
        ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(now)
                .unwrap()
                .to_rfc3339(),
        },
        now,
    )
    .await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let claimed = store
        .claim_queued(&work.run_id, "node-a", 30)
        .await
        .unwrap();
    run::Entity::update_many()
        .set(run::ActiveModel {
            start_deadline: Set(now),
            lease_deadline: Set(Some(now)),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(store.expired_pending(0, 100).await.unwrap().is_empty());
    assert!(matches!(
        store.expire_pending(&work.run_id).await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert!(matches!(
        store.claim_queued(&work.run_id, "node-b", 30).await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert!(matches!(
        store
            .finish_run(
                &work.run_id,
                "node-a",
                claimed.lease_epoch,
                desk_agent_protocol::schedule::ScheduledRunStatus::Succeeded,
                None,
                None
            )
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert_eq!(
        store
            .read(1, &task.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        Some(work.run_id.as_str())
    );
}

#[tokio::test]
async fn offline_wait_holds_only_the_slot_and_can_be_claimed_or_paused() {
    for pause in [false, true] {
        let clock_db = Database::connect("sqlite::memory:").await.unwrap();
        let now = database_now(&clock_db).await.unwrap();
        let (store, task) = fixture(
            ScheduleRule::Once {
                at: chrono::DateTime::from_timestamp_millis(now)
                    .unwrap()
                    .to_rfc3339(),
            },
            now,
        )
        .await;
        let work = store
            .materialize_due(&task.schedule_id, 1)
            .await
            .unwrap()
            .unwrap();
        let waiting = store.wait_for_device(&work.run_id).await.unwrap();
        assert_eq!(waiting.status, "waiting_device");
        assert!(waiting.lease_owner.is_none());
        assert!(waiting.lease_deadline.is_none());
        assert!(waiting.started_at.is_none());
        assert_eq!(waiting.attempt, 0);
        assert_eq!(store.wait_for_device(&work.run_id).await.unwrap(), waiting);
        let task = store.read(1, &task.schedule_id).await.unwrap();
        assert_eq!(task.active_run_id.as_deref(), Some(work.run_id.as_str()));
        if pause {
            store
                .pause(1, &task.schedule_id, task.revision, now)
                .await
                .unwrap();
            assert!(matches!(
                store.claim_queued(&work.run_id, "node", 30).await,
                Err(ScheduleStoreError::Conflict)
            ));
            assert!(matches!(
                store.wait_for_device(&work.run_id).await,
                Err(ScheduleStoreError::Conflict)
            ));
            assert!(
                store
                    .read(1, &task.schedule_id)
                    .await
                    .unwrap()
                    .active_run_id
                    .is_none()
            );
        } else {
            let claimed = store.claim_queued(&work.run_id, "node", 30).await.unwrap();
            assert_eq!(claimed.status, "running");
            assert_eq!(claimed.attempt, 1);
            assert!(matches!(
                store.wait_for_device(&work.run_id).await,
                Err(ScheduleStoreError::Conflict)
            ));
        }
    }
}

#[tokio::test]
async fn fresh_claim_rejects_reused_context_terminal_markers_and_changed_snapshot() {
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    for mismatch in [
        "started",
        "finished",
        "cancelled",
        "accounted",
        "attempt",
        "epoch",
        "lease",
        "deadline",
        "error",
        "result",
        "conversation",
        "turn",
        "owner",
        "snapshot",
    ] {
        let (store, task) = fixture(
            ScheduleRule::Once {
                at: chrono::DateTime::from_timestamp_millis(now)
                    .unwrap()
                    .to_rfc3339(),
            },
            now,
        )
        .await;
        let work = store
            .materialize_due(&task.schedule_id, 1)
            .await
            .unwrap()
            .unwrap();
        let mut change = run::ActiveModel::default();
        match mismatch {
            "started" => change.started_at = Set(Some(now)),
            "finished" => change.finished_at = Set(Some(now)),
            "cancelled" => change.cancel_requested_at = Set(Some(now)),
            "accounted" => change.failure_accounted = Set(true),
            "attempt" => change.attempt = Set(1),
            "epoch" => change.lease_epoch = Set(1),
            "lease" => change.lease_owner = Set(Some("old-node".into())),
            "deadline" => change.lease_deadline = Set(Some(now + 30_000)),
            "error" => change.error_kind = Set(Some("old-error".into())),
            "result" => change.result_ref = Set(Some("old-result".into())),
            "conversation" => change.conversation_id = Set("rehearsal-context".into()),
            "turn" => change.turn_id = Set("old-turn".into()),
            "owner" => change.owner_user_id = Set(2),
            "snapshot" => {
                let mut snapshot: entity::Model =
                    serde_json::from_str(&work.task_snapshot_json).unwrap();
                snapshot.prompt = "different instructions".into();
                change.task_snapshot_json = Set(json(&snapshot).unwrap());
            }
            _ => unreachable!(),
        }
        run::Entity::update_many()
            .set(change)
            .filter(run::Column::Id.eq(work.id))
            .exec(&store.db)
            .await
            .unwrap();
        let before_task = store.read(1, &task.schedule_id).await.unwrap();
        let before_run = run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                store.claim_queued(&work.run_id, "node", 30).await,
                Err(ScheduleStoreError::Conflict)
            ),
            "{mismatch}"
        );
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before_task);
        assert_eq!(
            run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before_run
        );
    }
}

#[tokio::test]
async fn caller_owned_claim_rolls_back_occurrence_and_task_together() {
    let clock_db = Database::connect("sqlite::memory:").await.unwrap();
    let now = database_now(&clock_db).await.unwrap();
    let (store, task) = fixture(
        ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(now)
                .unwrap()
                .to_rfc3339(),
        },
        now,
    )
    .await;
    let work = store
        .materialize_due(&task.schedule_id, 1)
        .await
        .unwrap()
        .unwrap();
    let before = store.read(1, &task.schedule_id).await.unwrap();
    let txn = store.db.begin().await.unwrap();
    let claimed = ScheduleStore::claim_queued_on(&txn, &work.run_id, "node", 30)
        .await
        .unwrap();
    assert_eq!(claimed.status, "running");
    assert_eq!(
        run::Entity::find_by_id(work.id)
            .one(&txn)
            .await
            .unwrap()
            .unwrap(),
        claimed
    );
    // Simulate a subsequent session/authority check failing before publication.
    txn.rollback().await.unwrap();
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
    assert_eq!(
        run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        work
    );
    let txn = store.db.begin().await.unwrap();
    let claimed = ScheduleStore::claim_queued_on(&txn, &work.run_id, "node", 30)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        claimed
    );
    assert_eq!(claimed.attempt, 1);
}
