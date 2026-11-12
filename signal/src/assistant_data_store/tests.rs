use super::*;
use crate::config::{connection::DatabaseConnection, test_support::Database};
use crate::entity::agent_assistant_quota as quota;
use desk_agent_protocol::ai_assistant::{
    memory::{
        AssistantDataControlReceipt, MemoryClearRequest, MemoryDeleteRequest, MemoryListRequest,
        MemoryListResult,
    },
    user_instructions::{AssistantDataPolicy, AssistantDataSettings, AssistantDataSettingsUpdate},
};
use sea_orm::{ColumnTrait, QueryFilter, Schema, TransactionTrait};
use sha2::{Digest, Sha256};

mod citations;
mod conversation_search;
mod conversation_window;
mod current_memory;
mod history_capacity;
mod history_scan;
mod history_view;
mod keyword_search;
mod memory_history;
mod memory_search;
mod memory_source;
mod model_gate;
mod model_output;
mod model_provider;
mod model_sources;
mod native_export;
mod native_reclaim;
mod native_result;
mod output_archive;
mod owner_conversations;
mod owner_memory;
mod owner_search;
mod owner_window;
mod policy_limits;
mod processing;
mod projection;
mod retained_copy;
mod retrieval_native;
mod retrieval_result;
mod retrieval_task;
mod source_lifecycle;
mod suppression;
mod tool_ports;

struct TestStore {
    db: DatabaseConnection,
}

impl TestStore {
    fn check(owner: i32) -> Result<(), AssistantStoreError> {
        if owner == crate::control_authorizer::SINGLE_ACCOUNT_USER_ID {
            Ok(())
        } else {
            Err(AssistantStoreError::Inaccessible)
        }
    }
    async fn settings(
        &self,
        owner: i32,
        _: &AssistantDataPolicy,
    ) -> Result<AssistantDataSettings, AssistantStoreError> {
        Self::check(owner)?;
        let txn = self.db.begin().await?;
        ensure_runtime_on(&txn, &owner.to_string()).await?;
        txn.commit().await?;
        Ok(crate::assistant_data_config::settings(&self.db).await)
    }
    async fn update_settings(
        &self,
        owner: i32,
        request: &AssistantDataSettingsUpdate,
        _: &AssistantDataPolicy,
    ) -> Result<AssistantDataSettings, AssistantStoreError> {
        Self::check(owner)?;
        crate::assistant_data_config::update_settings(&self.db, request)
            .await
            .map_err(|error| match error {
                crate::assistant_data_config::WriteError::Conflict { current_revision } => {
                    AssistantStoreError::Stale { current_revision }
                }
                crate::assistant_data_config::WriteError::Invalid
                | crate::assistant_data_config::WriteError::PolicyInvalid(_) => {
                    AssistantStoreError::Invalid("invalid test settings")
                }
                crate::assistant_data_config::WriteError::Backend(error) => {
                    AssistantStoreError::Backend(error)
                }
            })
    }
    async fn memories(
        &self,
        owner: i32,
        request: &MemoryListRequest,
        _: &AssistantDataPolicy,
        now: u64,
    ) -> Result<MemoryListResult, AssistantStoreError> {
        Self::check(owner)?;
        memory::memories(&self.db, request, now).await
    }
    async fn forget_memory(
        &self,
        owner: i32,
        request: &MemoryDeleteRequest,
        _: &AssistantDataPolicy,
        now: u64,
    ) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
        Self::check(owner)?;
        memory_control::forget_memory(&self.db, request, now).await
    }
    async fn clear_memories(
        &self,
        owner: i32,
        request: &MemoryClearRequest,
        _: &AssistantDataPolicy,
        now: u64,
    ) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
        Self::check(owner)?;
        memory_control::clear_memories(&self.db, request, now).await
    }
    async fn owner_source(
        &self,
        owner: i32,
        source_id: &str,
        _: &AssistantDataPolicy,
        now: u64,
    ) -> Result<
        desk_agent_protocol::ai_assistant::source::AssistantSourceResolution,
        AssistantStoreError,
    > {
        Self::check(owner)?;
        owner_source::owner_source(&self.db, source_id, now).await
    }
}

pub(crate) async fn recovery_database() -> crate::config::connection::DatabaseConnection {
    let store = setup().await;
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    store.db
}

async fn setup() -> TestStore {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    ensure_tables(&db).await;
    TestStore { db }
}

pub(crate) async fn ensure_tables(db: &crate::config::connection::DatabaseConnection) {
    let schema = Schema::new(db.get_database_backend());
    desk_assistant_data_store::entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        db.get_database_backend(),
    )))
    .sync(&**db)
    .await
    .unwrap();
    for mut table in [
        schema.create_table_from_entity(crate::entity::agent_assistant_device_binding::Entity),
        schema.create_table_from_entity(crate::entity::agent_action_item::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_delegation::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_review::Entity),
        schema.create_table_from_entity(crate::entity::agent_capability_dispatch_outbox::Entity),
        schema.create_table_from_entity(crate::entity::agent_capability_grant::Entity),
        schema.create_table_from_entity(crate::entity::agent_exec_task::Entity),
        schema.create_table_from_entity(crate::entity::agent_goal_open_request::Entity),
        schema.create_table_from_entity(crate::entity::agent_goal_run::Entity),
        schema.create_table_from_entity(crate::entity::agent_grant_reservation::Entity),
        schema.create_table_from_entity(crate::entity::agent_permission_resume::Entity),
        schema.create_table_from_entity(crate::entity::agent_run_event::Entity),
        schema.create_table_from_entity(crate::entity::agent_schedule::Entity),
        schema.create_table_from_entity(crate::entity::agent_schedule_run::Entity),
        schema.create_table_from_entity(crate::entity::agent_session::Entity),
        schema.create_table_from_entity(crate::entity::agent_task_budget_reservation::Entity),
        schema.create_table_from_entity(crate::entity::agent_file_recovery_cleanup::Entity),
        schema.create_table_from_entity(crate::entity::agent_file_recovery_scope::Entity),
        schema.create_table_from_entity(crate::entity::agent_attachment::Entity),
        schema.create_table_from_entity(crate::entity::agent_delegation_group::Entity),
        schema.create_table_from_entity(crate::entity::agent_subagent_run::Entity),
        schema.create_table_from_entity(crate::entity::agent_subagent_inbox::Entity),
        schema.create_table_from_entity(crate::entity::agent_delegation_reservation::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_trace::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_trace_part::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_attempt::Entity),
        schema.create_table_from_entity(crate::entity::model_egress_receipt::Entity),
    ] {
        table.if_not_exists();
        db.execute(&table).await.unwrap();
    }
    use crate::entity::agent_assistant_citation as citation;
    let citation_slot = sea_orm::sea_query::Index::create()
        .if_not_exists()
        .name("uq_assistant_test_citation")
        .table(citation::Entity)
        .col(citation::Column::ActorId)
        .col(citation::Column::AnswerMessageId)
        .col(citation::Column::AnswerRevision)
        .unique()
        .to_owned();
    db.execute(&citation_slot).await.unwrap();
    use crate::entity::agent_assistant_history_coverage as coverage;
    let coverage_slot = sea_orm::sea_query::Index::create()
        .if_not_exists()
        .name("uq_assistant_test_coverage_slot")
        .table(coverage::Entity)
        .col(coverage::Column::ActorId)
        .col(coverage::Column::ConversationId)
        .col(coverage::Column::DayBucket)
        .col(coverage::Column::Reason)
        .unique()
        .to_owned();
    db.execute(&coverage_slot).await.unwrap();
    use crate::entity::agent_assistant_source_suppression as suppression;
    let suppression_slot = sea_orm::sea_query::Index::create()
        .if_not_exists()
        .name("uq_assistant_test_source_suppression")
        .table(suppression::Entity)
        .col(suppression::Column::ActorId)
        .col(suppression::Column::SourceId)
        .col(suppression::Column::SourceRevision)
        .col(suppression::Column::OriginId)
        .unique()
        .to_owned();
    db.execute(&suppression_slot).await.unwrap();
}

pub(crate) async fn seed_personal_memory(db: &DatabaseConnection, label: &str) -> String {
    let store = TestStore { db: db.clone() };
    owner_memory::seed(
        &store,
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        label,
    )
    .await
    .0
}
