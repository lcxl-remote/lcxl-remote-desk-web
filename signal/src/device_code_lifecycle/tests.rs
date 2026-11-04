use super::fixtures::{db, publish};
use super::*;
use crate::{
    controller::{
        connection::device_connection,
        device_code::{
            DeviceCodeCreateParams, DeviceCodeUpdateParams, create_code, delete_code, delete_codes,
            update_code,
        },
    },
    entity::device_code,
};
use actix_web::{HttpResponse, web};
use desk_signal_facade::model::connection::SharedConnectionMap;
use sea_orm::{ConnectionTrait, EntityTrait};
use std::time::Duration;

async fn wait_users(lifecycle: &DeviceCodeLifecycle, client: &str, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let users = lifecycle
                .slots
                .lock()
                .unwrap()
                .get(client)
                .map(|slot| slot.users)
                .unwrap_or(0);
            if users == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn body(response: HttpResponse) -> serde_json::Value {
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    serde_json::from_slice(
        &actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_waiters_and_holders_release_slots_without_cross_identity_blocking() {
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    assert!(Arc::ptr_eq(global(), global()));
    for round in 0..1000 {
        let client = format!("client-{round}");
        let held = lifecycle.lock(&client).await;
        let other = lifecycle.clone();
        let name = client.clone();
        let task = tokio::spawn(async move { other.lock(&name).await });
        wait_users(&lifecycle, &client, 2).await;
        let independent =
            tokio::time::timeout(Duration::from_millis(200), lifecycle.lock("independent"))
                .await
                .unwrap();
        drop(independent);
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        wait_users(&lifecycle, &client, 1).await;
        drop(held);
        assert!(lifecycle.slots.lock().unwrap().is_empty());
    }
}

#[actix_web::test]
async fn single_and_batch_deletion_cannot_split_preparation_from_publication() {
    for batch in [false, true] {
        let db = db().await;
        let lifecycle = Arc::new(DeviceCodeLifecycle::default());
        let map = web::Data::new(SharedConnectionMap::new_oss());
        let (prepared, code) = lifecycle.prepare_registration(&db, "device").await.unwrap();
        let row = device_code::Entity::find().one(&db).await.unwrap().unwrap();
        let (other_db, other_map, other_lifecycle) = (db.clone(), map.clone(), lifecycle.clone());
        let deleting = tokio::spawn(async move {
            if batch {
                delete_codes(&other_db, &other_map, &other_lifecycle, vec![row.id]).await
            } else {
                delete_code(&other_db, &other_map, &other_lifecycle, row.id).await
            }
        });
        wait_users(&lifecycle, "device", 2).await;
        let independent = tokio::time::timeout(
            Duration::from_millis(200),
            lifecycle.prepare_registration(&db, "other"),
        )
        .await
        .unwrap()
        .unwrap();
        drop(independent);
        let (_response, handler) = publish(map.clone(), "device", "c1", code).await;
        drop(prepared);
        let outcome = deleting.await.unwrap();
        assert_eq!(outcome.is_ok(), batch);
        assert!(
            device_code::Entity::find_by_id(row.id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(!handler.connection_state.lifecycle.is_retired());
        assert_eq!(
            body(device_connection(&db, &map, &lifecycle, "device").await).await["data"]["connection_id"],
            "c1"
        );
        assert!(lifecycle.slots.lock().unwrap().is_empty());
    }
}

#[actix_web::test]
async fn offline_unknown_deleted_and_unavailable_directory_have_distinct_results() {
    let db = db().await;
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    let map = web::Data::new(SharedConnectionMap::new_oss());
    let (guard, code) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    drop(guard);
    assert_eq!(
        body(device_connection(&db, &map, &lifecycle, "device").await).await["code"],
        desk_utils::error::DeskErrorCode::REMOTE_DESK_OFFLINE.code()
    );
    assert_eq!(
        body(device_connection(&db, &map, &lifecycle, "unknown").await).await["code"],
        desk_utils::error::DeskErrorCode::DEVICE_NOT_FOUND.code()
    );
    let row = device_code::Entity::find().one(&db).await.unwrap().unwrap();
    delete_code(&db, &map, &lifecycle, row.id).await.unwrap();
    assert_eq!(
        body(device_connection(&db, &map, &lifecycle, "device").await).await["code"],
        desk_utils::error::DeskErrorCode::DEVICE_NOT_FOUND.code()
    );
    let (_response, _handler) = publish(map.clone(), "online", "c1", code).await;
    db.execute_unprepared("DROP TABLE device_code")
        .await
        .unwrap();
    assert_eq!(
        body(device_connection(&db, &map, &lifecycle, "device").await).await["code"],
        desk_utils::error::DeskErrorCode::SYSTEM_ERROR.code()
    );
    // The online path depends only on the already-published identity index.
    assert_eq!(
        body(device_connection(&db, &map, &lifecycle, "online").await).await["code"],
        0
    );
    assert!(lifecycle.slots.lock().unwrap().is_empty());
}

#[actix_web::test]
async fn rotation_is_ordered_with_publication_and_does_not_allow_stale_deletion() {
    let db = db().await;
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    let map = web::Data::new(SharedConnectionMap::new_oss());
    let (prepared, code) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    let row = device_code::Entity::find().one(&db).await.unwrap().unwrap();
    let (other_db, other_lifecycle) = (db.clone(), lifecycle.clone());
    let rotating = tokio::spawn(async move {
        update_code(
            &other_db,
            &other_lifecycle,
            row.id,
            DeviceCodeUpdateParams {
                device_code: Some("NEW23456".into()),
                capabilities: None,
            },
        )
        .await
    });
    wait_users(&lifecycle, "device", 2).await;
    let (_response, old) = publish(map.clone(), "device", "c1", code).await;
    drop(prepared);
    let changed = rotating.await.unwrap().unwrap();
    assert_eq!(changed.generation, 1);
    let (next, code) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    assert_eq!(code, "NEW23456");
    let (_response, current) = publish(map.clone(), "device", "c2", code).await;
    drop(next);
    assert!(old.connection_state.lifecycle.is_retired());
    assert!(!current.connection_state.lifecycle.is_retired());
    assert!(delete_code(&db, &map, &lifecycle, row.id).await.is_err());
    assert_eq!(
        device_code::Entity::find_by_id(row.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .generation,
        1
    );
    assert!(lifecycle.slots.lock().unwrap().is_empty());
}

#[actix_web::test]
async fn failed_or_cancelled_preparation_never_retires_the_current_connection() {
    let db = db().await;
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    let map = web::Data::new(SharedConnectionMap::new_oss());
    let (prepared, code) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    let (_response, current) = publish(map.clone(), "device", "c1", code).await;
    drop(prepared);
    let (abandoned, _) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    drop(abandoned);
    assert!(!current.connection_state.lifecycle.is_retired());
    // Cancel while the actual registration SELECT waits for the only DB connection.
    let transaction = sea_orm::TransactionTrait::begin(&db).await.unwrap();
    let (other_db, other_lifecycle) = (db.clone(), lifecycle.clone());
    let blocked = tokio::spawn(async move {
        other_lifecycle
            .prepare_registration(&other_db, "device")
            .await
    });
    wait_users(&lifecycle, "device", 1).await;
    blocked.abort();
    assert!(matches!(blocked.await, Err(error) if error.is_cancelled()));
    transaction.rollback().await.unwrap();
    assert!(!current.connection_state.lifecycle.is_retired());
    db.execute_unprepared("DROP TABLE device_code")
        .await
        .unwrap();
    assert!(lifecycle.prepare_registration(&db, "device").await.is_err());
    assert!(!current.connection_state.lifecycle.is_retired());
    assert!(lifecycle.slots.lock().unwrap().is_empty());
}

#[actix_web::test]
async fn manual_creation_and_delete_first_registration_preserve_identity_records() {
    let db = db().await;
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    let map = SharedConnectionMap::new_oss();
    let (preparing, _) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    let row = device_code::Entity::find().one(&db).await.unwrap().unwrap();
    let (other_db, other_lifecycle) = (db.clone(), lifecycle.clone());
    let creating = tokio::spawn(async move {
        create_code(
            &other_db,
            &other_lifecycle,
            DeviceCodeCreateParams {
                client_id: "device".into(),
                device_code: None,
                capabilities: None,
            },
        )
        .await
    });
    wait_users(&lifecycle, "device", 2).await;
    drop(preparing);
    assert!(creating.await.unwrap().is_err());
    delete_code(&db, &map, &lifecycle, row.id).await.unwrap();
    let (next, _) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    let replacement = device_code::Entity::find().one(&db).await.unwrap().unwrap();
    assert_ne!(replacement.id, row.id);
    assert_eq!(replacement.client_id, "device");
    drop(next);
    assert!(lifecycle.slots.lock().unwrap().is_empty());
}

#[actix_web::test]
async fn bulk_deletion_rolls_back_all_chunks_and_releases_every_identity_on_failure() {
    use sea_orm::{PaginatorTrait, Set};
    let db = db().await;
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    let map = SharedConnectionMap::new_oss();
    let now = chrono::Utc::now();
    for chunk in 0..3 {
        device_code::Entity::insert_many((0..100).map(|i| {
            let i = chunk * 100 + i;
            device_code::ActiveModel {
                client_id: Set(format!("device-{i}")),
                device_code: Set(format!("CODE{i}")),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
        }))
        .exec(&db)
        .await
        .unwrap();
    }
    db.execute_unprepared("CREATE TRIGGER reject_delete BEFORE DELETE ON device_code WHEN OLD.id = 300 BEGIN SELECT RAISE(FAIL, 'delete failure'); END;").await.unwrap();
    assert!(
        delete_codes(&db, &map, &lifecycle, (1..=300).collect())
            .await
            .is_err()
    );
    assert_eq!(device_code::Entity::find().count(&db).await.unwrap(), 300);
    assert!(lifecycle.slots.lock().unwrap().is_empty());
    db.execute_unprepared("DROP TRIGGER reject_delete")
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        delete_codes(&db, &map, &lifecycle, (1..=300).collect()),
        delete_codes(&db, &map, &lifecycle, (1..=300).rev().collect())
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(device_code::Entity::find().count(&db).await.unwrap(), 0);
    assert!(lifecycle.slots.lock().unwrap().is_empty());
}

struct PausedDelete {
    db: crate::config::connection::DatabaseConnection,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ConnectionTrait for PausedDelete {
    fn get_database_backend(&self) -> sea_orm::DbBackend {
        self.db.get_database_backend()
    }
    async fn execute_raw(
        &self,
        statement: sea_orm::Statement,
    ) -> Result<sea_orm::ExecResult, sea_orm::DbErr> {
        if statement.sql.starts_with("DELETE") {
            self.reached.notify_one();
            self.resume.notified().await;
        }
        self.db.execute_raw(statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.db.execute_unprepared(sql).await
    }
    async fn query_one_raw(
        &self,
        statement: sea_orm::Statement,
    ) -> Result<Option<sea_orm::QueryResult>, sea_orm::DbErr> {
        self.db.query_one_raw(statement).await
    }
    async fn query_all_raw(
        &self,
        statement: sea_orm::Statement,
    ) -> Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        self.db.query_all_raw(statement).await
    }
}

#[actix_web::test]
async fn database_wait_never_holds_the_connection_table_lock() {
    let db = db().await;
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    let map = Arc::new(SharedConnectionMap::new_oss());
    drop(lifecycle.prepare_registration(&db, "device").await.unwrap());
    let row = device_code::Entity::find().one(&db).await.unwrap().unwrap();
    let gate = Arc::new(PausedDelete {
        db,
        reached: Default::default(),
        resume: Default::default(),
    });
    let (other_db, other_map, other_lifecycle) = (gate.clone(), map.clone(), lifecycle.clone());
    let deleting = tokio::spawn(async move {
        delete_code(other_db.as_ref(), &other_map, &other_lifecycle, row.id).await
    });
    tokio::time::timeout(Duration::from_secs(2), gate.reached.notified())
        .await
        .unwrap();
    let table = tokio::time::timeout(Duration::from_millis(200), map.write())
        .await
        .unwrap();
    drop(table);
    let (independent, code) = tokio::time::timeout(
        Duration::from_millis(200),
        lifecycle.prepare_registration(&gate.db, "other-device"),
    )
    .await
    .unwrap()
    .unwrap();
    let (_response, other) = publish(
        web::Data::from(map.clone()),
        "other-device",
        "unrelated",
        code,
    )
    .await;
    drop(independent);
    gate.resume.notify_one();
    deleting.await.unwrap().unwrap();
    assert!(!other.connection_state.lifecycle.is_retired());
    assert!(lifecycle.slots.lock().unwrap().is_empty());
}

#[actix_web::test]
async fn deletion_of_an_observed_old_generation_cannot_remove_a_rotated_record() {
    let db = db().await;
    let lifecycle = Arc::new(DeviceCodeLifecycle::default());
    let map = Arc::new(SharedConnectionMap::new_oss());
    let (held, _) = lifecycle.prepare_registration(&db, "device").await.unwrap();
    let row = device_code::Entity::find().one(&db).await.unwrap().unwrap();
    let (other_db, other_map, other_lifecycle) = (db.clone(), map.clone(), lifecycle.clone());
    let deleting =
        tokio::spawn(
            async move { delete_code(&other_db, &other_map, &other_lifecycle, row.id).await },
        );
    wait_users(&lifecycle, "device", 2).await;
    crate::controller::device_code::apply_device_code_update(&db, row.id, "ROTATED2", None)
        .await
        .unwrap()
        .unwrap();
    drop(held);
    let failure = deleting.await.unwrap().unwrap_err();
    let body =
        actix_web::body::to_bytes(actix_web::ResponseError::error_response(&failure).into_body())
            .await
            .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["code"],
        desk_utils::error::DeskErrorCode::REVISION_CONFLICT.code()
    );
    let current = device_code::Entity::find_by_id(row.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.generation, 1);
    assert_eq!(current.device_code, "ROTATED2");
    assert!(lifecycle.slots.lock().unwrap().is_empty());
}

struct PausedLookup {
    db: crate::config::connection::DatabaseConnection,
    queries: std::sync::atomic::AtomicUsize,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ConnectionTrait for PausedLookup {
    fn get_database_backend(&self) -> sea_orm::DbBackend {
        self.db.get_database_backend()
    }
    async fn execute_raw(
        &self,
        statement: sea_orm::Statement,
    ) -> Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.db.execute_raw(statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.db.execute_unprepared(sql).await
    }
    async fn query_one_raw(
        &self,
        statement: sea_orm::Statement,
    ) -> Result<Option<sea_orm::QueryResult>, sea_orm::DbErr> {
        let result = self.db.query_one_raw(statement).await?;
        if self
            .queries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            self.reached.notify_one();
            self.resume.notified().await;
        }
        Ok(result)
    }
    async fn query_all_raw(
        &self,
        statement: sea_orm::Statement,
    ) -> Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        self.db.query_all_raw(statement).await
    }
}

#[actix_web::test]
async fn redemption_snapshot_cannot_cross_deletion_and_recreation_even_when_the_code_is_reused() {
    use std::sync::atomic::Ordering;
    let db = db().await;
    let lifecycle = global().clone();
    let client = "redemption-race-device";
    let map = web::Data::new(SharedConnectionMap::new_oss());
    let (held, code) = lifecycle.prepare_registration(&db, client).await.unwrap();
    drop(held);
    let old = device_code::Entity::find().one(&db).await.unwrap().unwrap();
    let gate = Arc::new(PausedLookup {
        db: db.clone(),
        queries: Default::default(),
        reached: Default::default(),
        resume: Default::default(),
    });
    let (lookup_db, lookup_map, lookup_code) = (gate.clone(), map.clone(), code.clone());
    let resolving = tokio::spawn(async move {
        crate::device_code_store::resolve_online(lookup_db.as_ref(), &lookup_map, &lookup_code)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), gate.reached.notified())
        .await
        .unwrap();
    delete_code(&db, &map, &lifecycle, old.id).await.unwrap();
    let replacement = create_code(
        &db,
        &lifecycle,
        DeviceCodeCreateParams {
            client_id: client.into(),
            device_code: Some(code.clone()),
            capabilities: None,
        },
    )
    .await
    .unwrap();
    assert_ne!(replacement.id, old.id);
    assert_eq!(replacement.generation, old.generation);
    let (prepared, current_code) = lifecycle.prepare_registration(&db, client).await.unwrap();
    let (_response, _handler) = publish(map.clone(), client, "c2", current_code).await;
    drop(prepared);
    gate.resume.notify_one();
    assert!(
        resolving.await.unwrap().unwrap().is_none(),
        "an old code-row snapshot must never authorize the recreated connection"
    );
    assert_eq!(gate.queries.load(Ordering::SeqCst), 2);
    let current = crate::device_code_store::resolve_online(&db, &map, &code)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.0, "c2");
    assert_eq!(current.1.id, replacement.id);
    assert!(!lifecycle.slots.lock().unwrap().contains_key(client));
}
