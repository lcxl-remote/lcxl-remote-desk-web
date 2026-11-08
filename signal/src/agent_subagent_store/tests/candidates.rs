use super::*;
use desk_diagnose_core::{
    session::{TriggerOrigin, TurnState},
    subagent::{SubAgentWaitReason, runtime::RuntimeTurn},
};

async fn candidate(db: &DatabaseConnection) -> (PersistedAgentSession, RuntimeTurn) {
    let (parent, calls) = super::creation::runnable_parent(db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let runtime = store
        .child_runtime_candidate(&task.task_id)
        .await
        .unwrap()
        .unwrap();
    (parent, runtime)
}

#[tokio::test]
async fn scanning_prepares_no_lease_and_stale_resource_deferral_cannot_override_a_claim() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    runtime.validate().unwrap();
    assert_eq!(runtime.origin(), TriggerOrigin::DelegatedTask);
    assert_eq!(runtime.session().lease_token, 0);
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    let queued = store.queued_task_candidates(0, 1).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert!(
        store
            .queued_task_candidates(queued[0].id, 1)
            .await
            .unwrap()
            .is_empty()
    );
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(run),
            &run.binding.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("one lease");
    };
    assert!(claimed.session.lease_token > 0);
    assert!(
        !store
            .defer_child_candidate(&runtime, Some(SubAgentWaitReason::ModelCapacity), None)
            .await
            .unwrap()
    );
    assert_eq!(
        load(&db, &run.binding.task_id).await.state,
        SubAgentState::Running
    );
    assert!(
        store
            .child_runtime_candidate(&run.binding.task_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn temporary_resource_wait_preserves_original_task_and_requires_target_availability() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    assert!(
        store
            .defer_child_candidate(&runtime, Some(SubAgentWaitReason::DeviceUnavailable), None)
            .await
            .unwrap()
    );
    let waiting = load(&db, &run.binding.task_id).await;
    assert_eq!(waiting.state, SubAgentState::WaitingResource);
    assert_eq!(waiting.binding, run.binding);
    assert!(
        !store
            .retry_resource_task(
                &run.binding.task_id,
                Some(&run.device_id),
                desk_diagnose_core::subagent::device_wait::DeviceWaitStatus::Ready
            )
            .await
            .unwrap()
    );
    run_row::Entity::update_many()
        .set(run_row::ActiveModel {
            next_attempt_at_ms: Set(Some(0)),
            ..Default::default()
        })
        .filter(run_row::Column::TaskId.eq(&run.binding.task_id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        !store
            .retry_resource_task(
                &run.binding.task_id,
                Some(&run.device_id),
                desk_diagnose_core::subagent::device_wait::DeviceWaitStatus::Waiting
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .retry_resource_task(
                &run.binding.task_id,
                Some(&run.device_id),
                desk_diagnose_core::subagent::device_wait::DeviceWaitStatus::Ready
            )
            .await
            .unwrap()
    );
    let resumed = store
        .child_runtime_candidate(&run.binding.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.session().agent_role, runtime.session().agent_role);
    assert_eq!(resumed.session().lease_token, 0);
    assert_eq!(resumed.source(), runtime.source());
}

#[tokio::test]
async fn model_capacity_failure_releases_the_lease_without_completing_or_resetting_the_child() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    let SubAgentClaimOutcome::Claimed(mut claimed) = store
        .claim_child(
            &super::creation::claim_params(run),
            &run.binding.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("one lease");
    };
    claimed
        .session
        .finish_turn(TurnState::Failed, chrono::Utc::now().to_rfc3339());
    store
        .settle_turn_for_task(
            &mut claimed.session,
            Some("delegated_model_unavailable"),
            false,
        )
        .await
        .unwrap();
    let waiting = load(&db, &run.binding.task_id).await;
    assert_eq!(waiting.state, SubAgentState::WaitingResource);
    assert!(waiting.failure_reason.is_none());
    assert!(waiting.terminal_report.is_none());
    assert_eq!(waiting.binding, run.binding);
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.lease_deadline.is_none());
    assert_eq!(
        PersistedAgentSession::decode_json(&row.state_json)
            .unwrap()
            .agent_role,
        runtime.session().agent_role
    );
    assert!(
        store
            .queued_task_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deadline_settlement_is_independent_of_resource_waits_and_never_revives_a_task() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    store
        .defer_child_candidate(&runtime, Some(SubAgentWaitReason::ModelCapacity), None)
        .await
        .unwrap();
    let after_deadline =
        chrono::DateTime::from_timestamp_millis(run.binding.deadline_ms + 1).unwrap();
    assert!(
        store
            .expire_task_at(&run.binding.task_id, after_deadline)
            .await
            .unwrap()
    );
    assert!(
        !store
            .expire_task_at(&run.binding.task_id, after_deadline)
            .await
            .unwrap()
    );
    let failed = load(&db, &run.binding.task_id).await;
    assert_eq!(failed.state, SubAgentState::Failed);
    assert_eq!(
        failed.failure_reason.as_deref(),
        Some("delegation_deadline_reached")
    );
    assert!(
        !store
            .retry_resource_task(
                &run.binding.task_id,
                Some(&run.device_id),
                desk_diagnose_core::subagent::device_wait::DeviceWaitStatus::Ready
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .child_runtime_candidate(&run.binding.task_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_wait_scan_skips_unreadable_active_and_duplicate_parents_without_losing_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let db = database_at(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("scan.db").display()
    ))
    .await;
    let (mut parent, _) = super::parent_wait::waiting_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let original = group_row::Entity::find().one(&db).await.unwrap().unwrap();
    let (candidates, next) = store.root_wait_candidates(0, 1).await.unwrap();
    assert!(candidates.is_empty());
    assert_eq!(next, Some(original.id));
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();

    for format in [
        None,
        Some(i32::from(desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION) - 1),
    ] {
        session_row::Entity::update_many()
            .col_expr(
                session_row::Column::ConversationSchemaVersion,
                sea_orm::sea_query::Expr::value(format),
            )
            .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
            .exec(&db)
            .await
            .unwrap();
        let (candidates, next) = store.root_wait_candidates(0, 32).await.unwrap();
        assert!(candidates.is_empty());
        assert!(next.is_none());
    }
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::ConversationSchemaVersion,
            sea_orm::sea_query::Expr::value(Some(i32::from(
                desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
            ))),
        )
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();

    let original_session = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut legacy: serde_json::Value = serde_json::from_str(&original_session.state_json).unwrap();
    legacy["conversation_id"] = "legacy-root".into();
    legacy["assistant_data"] = serde_json::json!({"closed_control_message_ids": []});
    let legacy_json = serde_json::to_string(&legacy).unwrap();
    assert!(PersistedAgentSession::decode_json(&legacy_json).is_err());
    let mut old: session_row::ActiveModel = original_session.clone().into();
    old.id = sea_orm::NotSet;
    old.conversation_id = Set("legacy-root".into());
    old.state_json = Set(legacy_json.clone());
    let old = old.insert(&db).await.unwrap();

    let original_child = run_row::Entity::find().one(&db).await.unwrap().unwrap();
    for (name, root, actor) in [
        ("legacy-a", "legacy-root", parent.actor_id.as_str()),
        ("legacy-b", "legacy-root", parent.actor_id.as_str()),
        ("missing", "missing-root", parent.actor_id.as_str()),
        (
            "wrong-owner",
            parent.conversation_id.as_str(),
            "other-owner",
        ),
        (
            "wrong-owner-b",
            parent.conversation_id.as_str(),
            "other-owner",
        ),
        (
            "duplicate-a",
            parent.conversation_id.as_str(),
            parent.actor_id.as_str(),
        ),
        (
            "duplicate-b",
            parent.conversation_id.as_str(),
            parent.actor_id.as_str(),
        ),
    ] {
        let mut hint: group_row::ActiveModel = original.clone().into();
        hint.id = sea_orm::NotSet;
        hint.group_id = Set(name.into());
        hint.root_conversation_id = Set(root.into());
        hint.actor_id = Set(actor.into());
        hint.insert(&db).await.unwrap();
        let mut child: run_row::ActiveModel = original_child.clone().into();
        child.id = sea_orm::NotSet;
        child.task_id = Set(format!("hint-{name}"));
        child.origin_ref = Set(format!("hint-key-{name}"));
        child.child_conversation_id = Set(format!("hint-child-{name}"));
        child.group_id = Set(name.into());
        child.insert(&db).await.unwrap();
    }
    let mut after = original.id;
    for _ in 0..2 {
        let (candidates, next) = store.root_wait_candidates(after, 2).await.unwrap();
        assert!(candidates.is_empty());
        let next = next.expect("a skipped full page still advances");
        assert!(next > after);
        after = next;
    }
    let (candidates, next) = store.root_wait_candidates(after, 2).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].root_conversation_id, parent.conversation_id);
    let (last, end) = store.root_wait_candidates(next.unwrap(), 2).await.unwrap();
    assert!(last.is_empty());
    assert!(end.is_none());
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        let (selected, _) = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(bytes)
                .spawn_scoped(scope, || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(desk_diagnose_core::future::boxed(|| {
                            store.root_wait_candidates(0, 32)
                        }))
                })
                .unwrap()
                .join()
                .unwrap()
                .unwrap()
        });
        assert_eq!(selected.len(), 1);
    }
    let retained = session_row::Entity::find_by_id(old.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.state_json, legacy_json);
    let current = session_row::Entity::find_by_id(original_session.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.state_json, original_session.state_json);
    assert_eq!(current.lease_token, original_session.lease_token);
}

#[tokio::test]
async fn wal_snapshot_and_legacy_delete_finish_after_a_writer_without_background_recovery() {
    use sea_orm::ConnectOptions;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("assistant.db");
    let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
    options.max_connections(4).sqlx_logging(false);
    options.map_sqlx_sqlite_opts(|options| {
        options
            .journal_mode(sea_orm::sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
    });
    let db = crate::config::test_support::Database::connect(options)
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let (parent, _) = super::parent_wait::waiting_parent(&db).await;
    let original = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut legacy: serde_json::Value = serde_json::from_str(&original.state_json).unwrap();
    legacy["conversation_id"] = "legacy-removal".into();
    legacy["assistant_data"] = serde_json::json!({"closed_control_message_ids": []});
    let mut old: session_row::ActiveModel = original.clone().into();
    old.id = sea_orm::NotSet;
    old.conversation_id = Set("legacy-removal".into());
    old.conversation_schema_version = Set(None);
    old.state_json = Set(serde_json::to_string(&legacy).unwrap());
    let old = old.insert(&db).await.unwrap();

    let writer = crate::db::begin_write(&db, session_row::Entity)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    let (candidates, _) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        store.root_wait_candidates(0, 32),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(candidates.is_empty());

    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let snapshot = sessions.read_assistant_snapshot_for_subject(
        &parent.conversation_id,
        &parent.actor_id,
        &parent.device_id,
    );
    let removal = sessions.delete_for_subject(&old.conversation_id, &old.actor_id, &old.device_id);
    tokio::pin!(snapshot, removal);
    assert!(futures_util::poll!(&mut snapshot).is_pending());
    assert!(futures_util::poll!(&mut removal).is_pending());
    writer.commit().await.unwrap();
    let (snapshot, removal) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(snapshot, removal)
    })
    .await
    .unwrap();
    assert!(snapshot.unwrap().is_some());
    removal.unwrap();
    assert!(
        session_row::Entity::find_by_id(old.id)
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    let retained = session_row::Entity::find_by_id(original.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.state_json, original.state_json);
    assert_eq!(retained.version, original.version);
    assert_eq!(retained.lease_token, original.lease_token);
}
