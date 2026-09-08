use super::*;
use sea_orm::{ActiveModelTrait, NotSet, Set};

#[tokio::test]
async fn candidate_scan_is_advisory_and_does_not_reclaim_started_work() {
    let (store, queued, _, _) = super::super::resume_claim::tests::fixture().await;
    assert_eq!(
        store.continuation_candidates(0, 32).await.unwrap(),
        vec![queued.clone()]
    );
    let now = store.database_time().await.unwrap();
    // A retained node name is historical identity, not a live lease, after waiting.
    let mut approval: run::ActiveModel = queued.clone().into();
    approval.status = Set("awaiting_permission".into());
    approval.started_at = Set(Some(now - 120_000));
    approval.start_deadline = Set(now - 60_000);
    approval.attempt = Set(1);
    approval.lease_epoch = Set(2);
    approval.lease_owner = Set(Some("old-node".into()));
    approval.result_ref = Set(Some("permission:request".into()));
    let approval = approval.update(&store.db).await.unwrap();
    assert_eq!(
        store.continuation_candidates(0, 32).await.unwrap(),
        vec![approval.clone()]
    );
    for variant in 0..9 {
        let mut changed: run::ActiveModel = approval.clone().into();
        match variant {
            0 => changed.cancel_requested_at = Set(Some(now)),
            1 => changed.failure_accounted = Set(true),
            2 => changed.finished_at = Set(Some(now)),
            3 => changed.lease_deadline = Set(Some(now - 1)),
            4 => changed.status = Set("running".into()),
            5 => changed.result_ref = Set(Some("directory:request".into())),
            6 => changed.attempt = Set(2),
            7 => changed.started_at = Set(None),
            8 => changed.owner_user_id = Set(2),
            _ => unreachable!(),
        }
        changed.update(&store.db).await.unwrap();
        assert!(
            store
                .continuation_candidates(0, 32)
                .await
                .unwrap()
                .is_empty(),
            "variant {variant}"
        );
        let mut restore: run::ActiveModel = approval.clone().into();
        restore = restore.reset_all();
        restore.update(&store.db).await.unwrap();
    }
    let mut restore: run::ActiveModel = queued.clone().into();
    restore = restore.reset_all();
    restore.start_deadline = Set(now - 1);
    restore.update(&store.db).await.unwrap();
    assert!(
        store
            .continuation_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn keyset_scan_advances_past_deferred_rows_and_excludes_fresh_tasks() {
    let (store, first, _, _) = super::super::resume_claim::tests::fixture().await;
    let original = store
        .read(first.owner_user_id, &first.schedule_id)
        .await
        .unwrap();
    let mut task: entity::ActiveModel = original.clone().into();
    task.id = NotSet;
    task.schedule_id = Set("second-schedule".into());
    task.creation_identity = Set("second-create".into());
    task.active_run_id = Set(Some("second-run".into()));
    let second_task = task.insert(&store.db).await.unwrap();
    let mut second: run::ActiveModel = first.clone().into();
    second.id = NotSet;
    second.run_id = Set("second-run".into());
    second.schedule_id = Set(second_task.schedule_id.clone());
    second.occurrence_identity = Set("second-occurrence".into());
    second.status = Set("waiting_device".into());
    let second = second.insert(&store.db).await.unwrap();
    assert_eq!(
        store.continuation_candidates(0, 1).await.unwrap(),
        vec![first.clone()]
    );
    assert_eq!(
        store.continuation_candidates(first.id, 1).await.unwrap(),
        vec![second.clone()]
    );
    assert!(
        store
            .continuation_candidates(second.id, 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.continuation_candidates(0, 1).await.unwrap(),
        vec![first.clone()]
    );
    let mut fresh: entity::ActiveModel = second_task.into();
    fresh.kind = Set("fresh_task".into());
    fresh.update(&store.db).await.unwrap();
    assert_eq!(
        store.continuation_candidates(0, 32).await.unwrap(),
        vec![first.clone()]
    );
    let mut inactive: entity::ActiveModel = original.into();
    inactive.active_run_id = Set(None);
    inactive.update(&store.db).await.unwrap();
    assert!(
        store
            .continuation_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
    for (cursor, limit) in [(-1, 1), (0, 0), (0, 129)] {
        assert!(matches!(
            store.continuation_candidates(cursor, limit).await,
            Err(ScheduleStoreError::Invalid)
        ));
    }
}
