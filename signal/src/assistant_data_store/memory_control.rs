//! Native owner transactions and model group closure adapters.

use super::{AssistantStoreError, ensure_runtime_on};
use desk_agent_protocol::ai_assistant::memory::{
    AssistantDataControlReceipt, MemoryClearRequest, MemoryDeleteRequest,
};
pub(crate) use desk_assistant_data_store::memory_control::*;
use sea_orm::TransactionTrait;
pub(crate) mod native;

pub(crate) async fn forget_memory(
    db: &crate::config::connection::DatabaseConnection,
    request: &MemoryDeleteRequest,
    now: u64,
) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
    request.validate().map_err(AssistantStoreError::Invalid)?;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    ensure_runtime_on(&txn, &actor).await?;
    let result = delete_on(&txn, &actor, request, now).await?;
    txn.commit().await?;
    Ok(result)
}

pub(crate) async fn clear_memories(
    db: &crate::config::connection::DatabaseConnection,
    request: &MemoryClearRequest,
    now: u64,
) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
    request.validate().map_err(AssistantStoreError::Invalid)?;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    ensure_runtime_on(&txn, &actor).await?;
    let result = clear_on(&txn, &actor, request, now).await?;
    txn.commit().await?;
    Ok(result)
}
