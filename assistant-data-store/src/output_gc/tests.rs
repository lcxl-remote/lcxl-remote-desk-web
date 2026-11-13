use super::*;
#[cfg(test)]
use crate::entity_table::EntityTableExt;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, Database, EntityTrait, Schema};
use std::time::{Duration, Instant};

async fn db() -> sea_orm::DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    db.execute(&Schema::new(db.get_database_backend()).entity_table(quota::Entity))
        .await
        .unwrap();
    db
}

async fn account(db: &sea_orm::DatabaseConnection, actor: &str) -> quota::Model {
    crate::ensure_quota_on(db, actor).await.unwrap();
    let row = quota::Entity::find()
        .filter(quota::Column::ActorId.eq(actor))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut active: quota::ActiveModel = row.into();
    active.output_gc_next_scan_at_ms = Set(None);
    active.update(db).await.unwrap()
}

#[tokio::test]
async fn configured_account_pages_and_cooldown_preserve_concurrent_charges() {
    let db = db().await;
    for actor in 1..=35 {
        account(&db, &actor.to_string()).await;
    }
    assert_eq!(
        due_actors_on(&db, None, 100, 71, 34).await.unwrap().len(),
        34
    );
    let rows = due_actors_on(&db, Some("2"), 100, 71, 34).await.unwrap();
    assert_eq!(rows.len(), 1);
    let (id, actor, previous) = &rows[0];
    quota::Entity::update_many()
        .col_expr(quota::Column::MetadataBytes, Expr::value(9000_i64))
        .col_expr(
            quota::Column::Revision,
            Expr::col(quota::Column::Revision).add(1_i64),
        )
        .filter(quota::Column::Id.eq(*id))
        .exec(&db)
        .await
        .unwrap();
    let a = db.clone();
    let b = db.clone();
    let (a, b) = tokio::join!(
        claim_on(&a, *id, actor, *previous, 100, 71),
        claim_on(&b, *id, actor, *previous, 100, 71)
    );
    assert_eq!(u8::from(a.unwrap()) + u8::from(b.unwrap()), 1);
    let row = quota::Entity::find_by_id(*id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.metadata_bytes, 9000);
    assert_eq!(row.output_gc_next_scan_at_ms, Some(171));
    assert!(
        due_actors_on(&db, Some("2"), 170, 71, 34)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        due_actors_on(&db, Some("2"), 171, 71, 34)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(!claim_on(&db, *id, "3", Some(171), 171, 71).await.unwrap());
    assert!(claim_on(&db, *id, actor, Some(171), 171, 71).await.unwrap());
}

#[tokio::test]
async fn gc_cursor_commit_keeps_concurrent_quota_changes_and_rejects_stale_cursor() {
    let db = db().await;
    let row = account(&db, "1").await;
    let cursor = GcCursor {
        id: row.id,
        actor_id: "1",
        output: (None, None),
    };
    quota::Entity::update_many()
        .col_expr(quota::Column::MetadataBytes, Expr::value(9192_i64))
        .col_expr(
            quota::Column::Revision,
            Expr::col(quota::Column::Revision).add(1_i64),
        )
        .filter(quota::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();

    finish_quota_on(&db, cursor, Some((10, 11)), 200)
        .await
        .unwrap();
    let after = quota::Entity::find_by_id(row.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.metadata_bytes, 8992);
    assert_eq!(
        (after.output_gc_completed_at_ms, after.output_gc_archive_id),
        (Some(10), Some(11))
    );

    assert!(
        finish_quota_on(&db, cursor, Some((10, 11)), 200)
            .await
            .is_err()
    );
    assert_eq!(
        quota::Entity::find_by_id(row.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        after
    );
}

#[test]
fn gc_wakeups_do_not_accelerate_hourly_retention_and_reconfiguration_changes_next_wake() {
    let start = Instant::now();
    let mut cadence = CleanupCadence::new(Duration::from_secs(3600));
    assert!(cadence.take_retention(start));
    for tick in 1..12 {
        let now = start + Duration::from_secs(tick * 300);
        assert!(!cadence.take_retention(now));
        assert_eq!(
            cadence.next_delay(now, Duration::from_secs(300)),
            Duration::from_secs(300)
        );
    }
    let almost = start + Duration::from_secs(3599);
    assert_eq!(
        cadence.next_delay(almost, Duration::from_secs(300)),
        Duration::from_secs(1)
    );
    assert!(cadence.take_retention(start + Duration::from_secs(3600)));
    assert_eq!(
        cadence.next_delay(start + Duration::from_secs(3600), Duration::from_millis(17)),
        Duration::from_millis(17)
    );
    assert_eq!(
        cadence.next_delay(start + Duration::from_secs(3600), Duration::from_secs(7200)),
        Duration::from_secs(3600)
    );
}
