use super::*;
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, EntityTrait, QueryFilter, Schema, TransactionTrait,
};
mod main_request;
mod memory_index;
mod memory_quota;
mod projection;
mod retrieval_cache;

#[tokio::test]
async fn device_roster_preserves_retained_generations_and_fences_stale_writes() {
    let TestStore { db } = setup().await;
    let txn = db.begin().await.unwrap();
    let initial = device_roster::replace(&txn, "owner", "root", 0, vec!["a".into(), "b".into()])
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(initial.revision, 1);
    assert_eq!(initial.members["a"], 1);
    assert_eq!(initial.members["b"], 1);

    let txn = db.begin().await.unwrap();
    let reordered = device_roster::replace(&txn, "owner", "root", 1, vec!["b".into(), "a".into()])
        .await
        .unwrap();
    assert_eq!(initial, reordered);
    let removed = device_roster::replace(&txn, "owner", "root", 1, vec!["b".into()])
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(removed.revision, 2);
    assert_eq!(removed.members["b"], 1);
    assert!(matches!(
        device_roster::load(&db, "another-owner", "root").await,
        Err(AssistantStoreError::Inaccessible)
    ));

    let txn = db.begin().await.unwrap();
    assert!(matches!(
        device_roster::replace(&txn, "owner", "root", 1, vec![]).await,
        Err(AssistantStoreError::Stale {
            current_revision: 2
        })
    ));
    let rejoined = device_roster::replace(&txn, "owner", "root", 2, vec!["a".into(), "b".into()])
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(rejoined.revision, 3);
    assert_eq!(rejoined.members["a"], 3);
    assert_eq!(rejoined.members["b"], 1);
    assert_eq!(
        device_roster::load(&db, "owner", "root").await.unwrap(),
        Some(rejoined)
    );
}

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

#[tokio::test]
async fn reports_follow_the_owner_model_within_one_gateway_after_backfill() {
    use crate::entity::agent_subagent_inbox as inbox;
    use sea_orm::{ActiveModelTrait, QueryOrder, Set};
    let store = setup().await;
    let old = destination("old-model");
    let row = |event: &str, stored: Option<String>| inbox::ActiveModel {
        event_id: Set(event.into()),
        root_conversation_id: Set("root".into()),
        group_id: Set("group".into()),
        task_id: Set("task".into()),
        actor_id: Set("1".into()),
        device_id: Set(String::new()),
        state_revision: Set(1),
        report_seq: Set(Some(1)),
        report_model_destination: Set(stored),
        event_kind: Set("report_completed".into()),
        event_json: Set("{}".into()),
        parent_input_revision: Set(1),
        parent_control_revision: Set(1),
        created_at: Set(1),
        ..Default::default()
    };
    // A row written before reports stored their gateway identity.
    row("legacy", Some(serde_json::to_string(&old).unwrap()))
        .insert(&store.db)
        .await
        .unwrap();
    row("protected", None).insert(&store.db).await.unwrap();
    assert_eq!(
        worker_report::normalize_report_gateways_on(&store.db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        worker_report::normalize_report_gateways_on(&store.db)
            .await
            .unwrap(),
        0
    );
    let stored: Vec<_> = inbox::Entity::find()
        .order_by_asc(inbox::Column::Id)
        .all(&store.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.report_model_destination)
        .collect();
    // The owner's current model on the same gateway matches the stored value.
    let current = worker_report::report_gateway(&destination("new-model")).unwrap();
    assert_eq!(stored, [Some(current.clone()), None]);
    let mut other_connection = destination("new-model");
    if let desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_revision,
        ..
    } = &mut other_connection
    {
        *connection_revision = 2;
    }
    assert_ne!(
        worker_report::report_gateway(&other_connection).unwrap(),
        current
    );
}
