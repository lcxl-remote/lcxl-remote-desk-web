//! Manual history deletion is distinct from native execution/file reclamation.
//! Every source purpose checks the original conversation and root identities.

use super::AssistantStoreError;
use crate::entity::{agent_assistant_origin as origin, agent_file_recovery_cleanup as removal};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, sea_query::Expr,
};
mod retained;
use crate::config::connection::DatabaseTransaction;
pub(crate) use retained::delete_retained_root_on;

pub(crate) async fn origin_readable_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    original: &origin::Model,
) -> Result<bool, AssistantStoreError> {
    if original.actor_id != actor || original.deleted_at_ms.is_some() {
        return Ok(false);
    }
    conversation_readable_on(
        db,
        actor,
        &original.conversation_id,
        &original.root_conversation_id,
    )
    .await
}

pub(crate) async fn conversation_readable_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
    root: &str,
) -> Result<bool, AssistantStoreError> {
    if actor.is_empty() || conversation.is_empty() || root.is_empty() {
        return Ok(false);
    }
    // Do not filter by actor: a conflicting native tombstone must not disappear
    // and make a reused or incorrectly attributed conversation look readable.
    let rows = removal::Entity::find()
        .filter(removal::Column::ConversationId.is_in([conversation, root]))
        .limit(3)
        .all(db)
        .await?;
    let maximum = if conversation == root { 1 } else { 2 };
    if rows.len() > maximum {
        return Ok(false);
    }
    Ok(rows
        .iter()
        .all(|row| row.actor_id == actor && row.history_deleted_at_unix_ms.is_none()))
}

/// The original owner/device/root fence and native close operation are already
/// held in the same deletion transaction. No memory bodies are enumerated here.
pub(crate) async fn mark_manual_delete_on(
    db: &DatabaseTransaction,
    actor: &str,
    device: &str,
    conversation: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    if now <= 0 {
        return Err(AssistantStoreError::Invalid(
            "invalid history deletion time",
        ));
    }
    let row = removal::Entity::find_by_id(conversation)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Invalid(
            "missing original conversation removal",
        ))?;
    if row.actor_id != actor || row.device_id != device {
        return Err(AssistantStoreError::Inaccessible);
    }
    if let Some(deleted) = row.history_deleted_at_unix_ms {
        if deleted <= 0 || deleted > now {
            return Err(AssistantStoreError::Inaccessible);
        }
        return Ok(());
    }
    let changed = removal::Entity::update_many()
        .col_expr(
            removal::Column::HistoryDeletedAtUnixMs,
            Expr::value(Some(now)),
        )
        .filter(removal::Column::ConversationId.eq(conversation))
        .filter(removal::Column::ActorId.eq(actor))
        .filter(removal::Column::DeviceId.eq(device))
        .filter(removal::Column::HistoryDeletedAtUnixMs.is_null())
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }

    if device.is_empty() {
        desk_assistant_data_store::device_roster::delete(db, actor, conversation).await?;
    }

    desk_assistant_data_store::source_lifecycle::revoke_deleted_conversation_on(db, actor).await
}
