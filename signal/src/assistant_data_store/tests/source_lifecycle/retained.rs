use super::super::super::source_lifecycle::delete_retained_root_on;
use super::*;
use crate::entity::{
    agent_assistant_history_conversation as conversations, agent_session as execution,
};
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;

#[tokio::test]
async fn the_existing_owner_delete_endpoint_accepts_a_retained_root_after_execution_reclamation() {
    let fixture = Fixture::new().await;
    prepare_endpoint_tables(&fixture.store).await;
    let sessions =
        crate::agent_session_store::SignalAgentSessionStore::new(fixture.store.db.clone());
    assert!(
        sessions
            .delete_for_subject("retained-root", "1", &fixture.native_device)
            .await
            .unwrap()
            .is_none()
    );
    let marker = removal::Entity::find_by_id("retained-root")
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        marker
            .history_deleted_at_unix_ms
            .is_some_and(|time| time > 0)
    );
    assert_eq!(
        execution::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        fixture
            .store
            .owner_source(1, &fixture.source, &AssistantDataPolicy::default(), 2100)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    let state = runtime_on(&fixture.store.db, "1").await.unwrap();
    assert!(
        sessions
            .delete_for_subject("retained-root", "1", &fixture.native_device)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(runtime_on(&fixture.store.db, "1").await.unwrap(), state);
}

async fn prepare_endpoint_tables(store: &TestStore) {
    let schema = sea_orm::Schema::new(store.db.get_database_backend());
    for mut table in [
        schema.entity_table(crate::entity::agent_approval_trace::Entity),
        schema.entity_table(crate::entity::agent_approval_trace_part::Entity),
    ] {
        store.db.execute(table.if_not_exists()).await.unwrap();
    }
}

struct Fixture {
    store: TestStore,
    device: String,
    native_device: String,
    source: String,
}

impl Fixture {
    async fn new() -> Self {
        let store = setup().await;
        let device = super::super::keyword_search::device(&store, "retained-device").await;
        let source = super::super::keyword_search::capture(
            &store,
            &device,
            "retained-root",
            "retained-input",
            "Original research report",
            &[],
        )
        .await;
        let native_device = native_device(&store, &device).await;
        native_removal_on(&store.db, "retained-root", "1").await;
        removal::Entity::update_many()
            .col_expr(removal::Column::DeviceId, Expr::value(&native_device))
            .filter(removal::Column::ConversationId.eq("retained-root"))
            .exec(&store.db)
            .await
            .unwrap();
        Self {
            store,
            device,
            native_device,
            source,
        }
    }

    async fn delete(&self, actor: &str, device: &str) -> Result<bool, AssistantStoreError> {
        let txn = crate::db::begin_write(&self.store.db, removal::Entity)
            .await
            .unwrap();
        let removed = delete_retained_root_on(&txn, actor, device, "retained-root", 2000).await?;
        txn.commit().await.unwrap();
        Ok(removed)
    }
}

#[tokio::test]
async fn an_expired_execution_session_can_be_removed_without_recreating_it_or_reopening_model_switches()
 {
    let fixture = Fixture::new().await;
    let policy = AssistantDataPolicy::default();
    let settings = fixture.store.settings(1, &policy).await.unwrap();
    fixture
        .store
        .update_settings(
            1,
            &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettingsUpdate {
                expected_revision: settings.revision,
                automatic_memory_enabled: false,
                model_history_search_enabled: false,

                explanation_version: 1,
            },
            &policy,
        )
        .await
        .unwrap();
    assert_eq!(
        execution::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    let before = removal::Entity::find_by_id("retained-root")
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.delete("1", &fixture.native_device).await.unwrap());
    let after = removal::Entity::find_by_id("retained-root")
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.history_deleted_at_unix_ms, Some(2000));
    assert_eq!(after.device_id, before.device_id);
    assert_eq!(after.created_at_unix_ms, before.created_at_unix_ms);
    assert_eq!(after.completed_at_unix_ms, before.completed_at_unix_ms);
    assert_eq!(after.attempts, before.attempts);
    assert_eq!(
        execution::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        fixture
            .store
            .owner_source(1, &fixture.source, &policy, 2100)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    let applied = runtime_on(&fixture.store.db, "1").await.unwrap();
    assert!(fixture.delete("1", &fixture.native_device).await.unwrap());
    assert_eq!(runtime_on(&fixture.store.db, "1").await.unwrap(), applied);
}

#[tokio::test]
async fn retained_deletion_requires_the_original_actor_device_and_a_main_conversation() {
    let fixture = Fixture::new().await;
    assert!(!fixture.delete("2", &fixture.native_device).await.unwrap());
    assert!(!fixture.delete("1", "another-native-device").await.unwrap());
    conversations::Entity::update_many()
        .col_expr(
            conversations::Column::RootConversationId,
            Expr::value("another-root"),
        )
        .col_expr(
            conversations::Column::ParentConversationId,
            Expr::value(Some("another-root")),
        )
        .filter(conversations::Column::ConversationId.eq("retained-root"))
        .exec(&fixture.store.db)
        .await
        .unwrap();
    assert!(!fixture.delete("1", &fixture.native_device).await.unwrap());
    assert!(
        removal::Entity::find_by_id("retained-root")
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap()
            .history_deleted_at_unix_ms
            .is_none()
    );
}

#[tokio::test]
async fn a_conflicting_live_execution_row_cannot_be_hidden_by_an_actor_filter() {
    let fixture = Fixture::new().await;
    seed_conflicting_session(&fixture.store, &fixture.native_device).await;
    assert!(!fixture.delete("1", &fixture.native_device).await.unwrap());
    let row = execution::Entity::find()
        .filter(execution::Column::ConversationId.eq("retained-root"))
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.actor_id, "foreign-owner");
    assert_eq!(row.state_json, "opaque original native row");
    assert!(
        removal::Entity::find_by_id("retained-root")
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap()
            .history_deleted_at_unix_ms
            .is_none()
    );
}

#[tokio::test]
async fn a_current_device_identity_mismatch_cannot_authorize_old_retained_history_deletion() {
    let fixture = Fixture::new().await;
    change_device_identity(&fixture.store, &fixture.device).await;
    assert!(!fixture.delete("1", &fixture.native_device).await.unwrap());
    assert!(
        removal::Entity::find_by_id("retained-root")
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap()
            .history_deleted_at_unix_ms
            .is_none()
    );
}

async fn native_device(store: &TestStore, public: &str) -> String {
    use crate::entity::agent_assistant_device_binding as binding;
    binding::Entity::find()
        .filter(binding::Column::BindingId.eq(public))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
        .audience
}

async fn seed_conflicting_session(store: &TestStore, device: &str) {
    let now = chrono::Utc::now();
    execution::ActiveModel {
        conversation_id: Set("retained-root".into()),
        actor_id: Set("foreign-owner".into()),
        device_id: Set(device.into()),
        state_json: Set("opaque original native row".into()),
        version: Set(1),
        lease_token: Set(0),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
}

async fn change_device_identity(store: &TestStore, public: &str) {
    use crate::entity::agent_assistant_device_binding as binding;
    binding::Entity::update_many()
        .col_expr(
            binding::Column::InstanceId,
            Expr::value("another-original-instance"),
        )
        .filter(binding::Column::BindingId.eq(public))
        .exec(&store.db)
        .await
        .unwrap();
}
