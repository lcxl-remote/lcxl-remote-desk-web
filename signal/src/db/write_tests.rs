//! Independent WAL connections exercise writer admission, rollback and read concurrency.
use super::*;

mod counter {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "write_counter")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub value: i32,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

async fn value(db: &impl ConnectionTrait) -> i32 {
    counter::Entity::find_by_id(1)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .value
}

#[tokio::test]
async fn write_admission_waits_before_snapshot_and_does_not_block_readers() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("writers.db").display()
    );
    let first = Database::connect(&url).await.unwrap();
    first
        .execute_unprepared("PRAGMA journal_mode=WAL")
        .await
        .unwrap();
    first.execute_unprepared("CREATE TABLE write_counter(id INTEGER PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO write_counter VALUES(1,0)").await.unwrap();
    let second = Database::connect(&url).await.unwrap();
    let writer = begin_write(&first, counter::Entity).await.unwrap();
    writer
        .execute_unprepared("UPDATE write_counter SET value=1 WHERE id=1")
        .await
        .unwrap();
    // Readers see committed data while another connection owns the writer.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), value(&second))
            .await
            .unwrap(),
        0
    );
    let next = begin_write(&second, counter::Entity);
    tokio::pin!(next);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut next)
            .await
            .is_err()
    );
    writer.commit().await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(2), next)
        .await
        .unwrap()
        .unwrap();
    // Writer admission precedes the snapshot, so the new committed value is visible.
    assert_eq!(value(&next).await, 1);
    next.execute_unprepared("UPDATE write_counter SET value=2 WHERE id=1")
        .await
        .unwrap();
    next.rollback().await.unwrap();
    assert_eq!(value(&second).await, 1);
}

#[tokio::test]
async fn writer_reservation_never_changes_rows_or_fires_row_triggers() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    db.execute_unprepared("CREATE TABLE write_counter(id INTEGER PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO write_counter VALUES(1,7); CREATE TABLE changes(id INTEGER); CREATE TRIGGER counter_changed AFTER UPDATE ON write_counter BEGIN INSERT INTO changes VALUES(NEW.id); END;").await.unwrap();
    begin_write(&db, counter::Entity)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    assert_eq!(value(&db).await, 7);
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM changes",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i64>("", "n").unwrap(), 0);
}
