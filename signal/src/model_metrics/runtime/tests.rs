use super::*;
use desk_diagnose_core::model_observability::*;
use sea_orm::{EntityTrait, Statement};

#[tokio::test]
async fn fresh_file_and_same_version_reopen_preserve_the_original_capture_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model-metrics.sqlite");
    let business = crate::config::test_support::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("signal.sqlite").display()
    ))
    .await
    .unwrap();
    business
        .execute_raw(Statement::from_string(
            business.get_database_backend(),
            "CREATE TABLE ai_usage_hourly (old_total BIGINT NOT NULL)".to_string(),
        ))
        .await
        .unwrap();
    business
        .execute_raw(Statement::from_string(
            business.get_database_backend(),
            "INSERT INTO ai_usage_hourly VALUES (987654321)".to_string(),
        ))
        .await
        .unwrap();
    let creating = AtomicBool::new(false);
    let store = open_current_store(
        &path,
        &creating,
        crate::config::test_support::context().await,
    )
    .await
    .unwrap();
    assert!(!creating.load(Ordering::Relaxed));
    let first = model_metric_state::Entity::find_by_id(1)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let reopened = open_current_store(
        &path,
        &AtomicBool::new(false),
        crate::config::test_support::context().await,
    )
    .await
    .unwrap();
    let second = model_metric_state::Entity::find_by_id(1)
        .one(&reopened.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.available_from_ms, second.available_from_ms);
    assert_eq!(first.schema_version, second.schema_version);
    assert_eq!(
        store.load_settings().await.unwrap(),
        reopened.load_settings().await.unwrap()
    );
    assert!(
        model_metric_event::Entity::find()
            .all(&reopened.db)
            .await
            .unwrap()
            .is_empty()
    );
    let old = business
        .query_one_raw(Statement::from_string(
            business.get_database_backend(),
            "SELECT old_total FROM ai_usage_hourly".to_string(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.try_get::<i64>("", "old_total").unwrap(), 987654321);
}

#[tokio::test]
async fn existing_missing_schema_corrupt_file_and_unusable_directory_are_not_rebuilt() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("existing.sqlite");
    let db = crate::config::test_support::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        missing.display()
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE retired_observation_fixture (total BIGINT NOT NULL)".to_string(),
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_string(
        db.get_database_backend(),
        "INSERT INTO retired_observation_fixture VALUES (123456789)".to_string(),
    ))
    .await
    .unwrap();
    assert!(
        open_current_store(
            &missing,
            &AtomicBool::new(false),
            crate::config::test_support::context().await
        )
        .await
        .is_err()
    );
    let total = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT total FROM retired_observation_fixture".to_string(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(total.try_get::<i64>("", "total").unwrap(), 123456789);
    assert!(
        model_metric_state::Entity::find_by_id(1)
            .one(&db)
            .await
            .is_err()
    );
    let corrupt = directory.path().join("corrupt.sqlite");
    tokio::fs::write(&corrupt, b"not a SQLite database")
        .await
        .unwrap();
    let unavailable = directory
        .path()
        .join("absent-directory/model-metrics.sqlite");
    for path in [&corrupt, &unavailable] {
        assert!(
            tokio::time::timeout(
                Duration::from_secs(4),
                open_current_store(
                    path,
                    &AtomicBool::new(false),
                    crate::config::test_support::context().await
                )
            )
            .await
            .unwrap()
            .is_err()
        );
    }
    assert_eq!(
        tokio::fs::read(&corrupt).await.unwrap(),
        b"not a SQLite database"
    );
}

#[tokio::test]
async fn wrong_component_format_remains_unavailable_without_altering_or_importing_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model-metrics.sqlite");
    let store = open_current_store(
        &path,
        &AtomicBool::new(false),
        crate::config::test_support::context().await,
    )
    .await
    .unwrap();
    store
        .db
        .execute_raw(Statement::from_string(
            store.db.get_database_backend(),
            "UPDATE model_metric_state SET schema_version = -1".to_string(),
        ))
        .await
        .unwrap();
    let prior = model_metric_state::Entity::find_by_id(1)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        open_current_store(
            &path,
            &AtomicBool::new(false),
            crate::config::test_support::context().await
        )
        .await
        .is_err()
    );
    let after = model_metric_state::Entity::find_by_id(1)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after, prior);
}

fn sample_event(now: i64) -> ObservationEvent {
    ObservationEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: "disk-fixture.terminal".into(),
        object_id: "disk-fixture".into(),
        call_id: None,
        phase: ObservationPhase::Terminal,
        sequence: 0,
        started_at_ms: now,
        occurred_at_ms: now,
        relation: None,
        attribution: Attribution {
            provider_id: "local.agent.1".into(),
            model_id: "local.agent.1".into(),
            model_name: "fixture".into(),
            configuration_revision: "1".into(),
            contract_revision: DEFINITION_VERSION.to_string(),
            purpose: Purpose::Agent,
            surface: Surface::Assistant,
            origin: Origin::User,
            configuration_scope: ConfigurationScope::Local,
            protocol: Protocol::OpenAiChatCompletions,
        },
        payload: ObservationPayload::Call(CallSnapshot {
            outcome: RequestOutcome::Returned,
            output: OutputOutcome::Accepted,
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn a_real_sqlite_write_lock_or_page_limit_is_a_bounded_observation_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model-metrics.sqlite");
    let store = open_current_store(
        &path,
        &AtomicBool::new(false),
        crate::config::test_support::context().await,
    )
    .await
    .unwrap();
    let locking = crate::config::test_support::Database::connect(format!(
        "sqlite://{}?mode=rw",
        path.display()
    ))
    .await
    .unwrap();
    let lock = locking.begin().await.unwrap();
    lock.execute_raw(Statement::from_string(
        lock.get_database_backend(),
        "UPDATE model_metric_state SET schema_version = schema_version WHERE id = 1".to_string(),
    ))
    .await
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        store.persist(&[sample_event(now)], now),
    )
    .await;
    assert!(
        result.unwrap().is_err(),
        "busy_timeout bounds only the metrics writer"
    );
    lock.rollback().await.unwrap();
    assert_eq!(store.persist(&[sample_event(now)], now).await.unwrap(), 0);
    store.aggregate(now).await.unwrap();

    let full_path = directory.path().join("limited.sqlite");
    let mut full_options =
        ConnectOptions::new(format!("sqlite://{}?mode=rwc", full_path.display()));
    full_options.max_connections(1).min_connections(1);
    let full_db = crate::config::test_support::Database::connect(full_options)
        .await
        .unwrap();
    create_schema(&full_db).await.unwrap();
    let full_store = Store::new(full_db.clone(), false, "limited".into());
    full_store.initialize_settings(now).await.unwrap();
    // A single connection keeps the SQLite connection-local page limit active.
    let pages = full_db
        .query_one_raw(Statement::from_string(
            full_db.get_database_backend(),
            "PRAGMA page_count".to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "page_count")
        .unwrap();
    full_db
        .execute_raw(Statement::from_string(
            full_db.get_database_backend(),
            format!("PRAGMA max_page_count = {pages}"),
        ))
        .await
        .unwrap();
    let mut large = sample_event(now);
    large.event_id = "full-fixture.terminal".into();
    large.object_id = "full-fixture".into();
    large.attribution.model_name = "m".repeat(128);
    let events: Vec<_> = (0..100)
        .map(|index| {
            let mut event = large.clone();
            event.object_id = format!("full-fixture-{index}");
            event.event_id = format!("{}.terminal", event.object_id);
            event
        })
        .collect();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), full_store.persist(&events, now))
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        model_metric_event::Entity::find()
            .all(&full_db)
            .await
            .unwrap()
            .is_empty(),
        "failed metric transaction commits no partial events"
    );
}
