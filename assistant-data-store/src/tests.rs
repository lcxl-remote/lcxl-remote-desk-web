use super::*;
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, EntityTrait, QueryFilter, Schema, TransactionTrait,
};
mod memory_index;
mod memory_quota;
mod projection;
mod retrieval_cache;

#[test]
fn unavailable_sources_are_tool_errors_but_authorization_and_storage_failures_are_not() {
    let source = AssistantStoreError::SourceUnavailable.into_agent_error();
    assert!(source.safe_for_model);
    assert_eq!(
        source.kind,
        desk_agent_protocol::AgentErrorKind::InvalidInput
    );
    for error in [
        AssistantStoreError::Inaccessible,
        AssistantStoreError::Invalid("native gate rejected"),
        AssistantStoreError::Backend(DbErr::Custom("unavailable".into())),
    ] {
        assert!(!error.into_agent_error().safe_for_model);
    }
}

struct TestStore {
    db: sea_orm::DatabaseConnection,
}
async fn setup() -> TestStore {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        db.get_database_backend(),
    )))
    .sync(&db)
    .await
    .unwrap();
    TestStore { db }
}
fn destination(model: &str) -> desk_agent_protocol::data_lineage::DestinationIdentity {
    desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: model.into(),
        profile_revision: 1,
    }
}

#[tokio::test]
async fn schema_reopen_preserves_committed_assistant_rows_and_metadata_charge() {
    let store = setup().await;
    ensure_quota_on(&store.db, "owner").await.unwrap();
    crate::projection::prepare_on(
        &store.db,
        &projection::proof("{}", "schema"),
        "{}",
        &AssistantDataPolicy::default(),
        1001,
    )
    .await
    .unwrap();
    let before = entity::agent_assistant_quota::Entity::find()
        .all(&store.db)
        .await
        .unwrap();
    entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        store.db.get_database_backend(),
    )))
    .sync(&store.db)
    .await
    .unwrap();
    assert_eq!(
        entity::agent_assistant_quota::Entity::find()
            .all(&store.db)
            .await
            .unwrap(),
        before
    );
    assert!(
        crate::projection::load_on(
            &store.db,
            "1",
            &projection::proof("{}", "schema").usage.projection_id
        )
        .await
        .unwrap()
        .is_some()
    );
}
