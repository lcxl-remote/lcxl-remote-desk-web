//! Fence the first interactive input to its explicit rehearsal reservation.
use super::*;
use desk_diagnose_core::chat::ChatMessage;
use sea_orm::{DatabaseTransaction, sea_query::Expr};

pub(crate) async fn validate_rehearsal_input_on(
    txn: &DatabaseTransaction,
    actor: &str,
    device: &str,
    conversation: &str,
    client_conversation: Option<&str>,
    message: &ChatMessage,
) -> Result<(), ScheduleStoreError> {
    let Some(client) = client_conversation.filter(|id| id.starts_with("rehearsal_")) else {
        return Ok(());
    };
    let row = rehearsal::Entity::find()
        .filter(rehearsal::Column::ClientConversationId.eq(client))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if row.owner_user_id.to_string() != actor
        || row.target_device_id != device
        || row.conversation_id != conversation
        || derive_conversation_key(actor, device, Some(client), "") != conversation
        || message.message_id != format!("rehearsal:{}:input", row.rehearsal_id)
        || message.text != row.prompt
        || message.image_data_url.is_some()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let locked = entity::Entity::update_many()
        .col_expr(
            entity::Column::Revision,
            Expr::col(entity::Column::Revision),
        )
        .filter(entity::Column::ScheduleId.eq(&row.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(row.owner_user_id))
        .filter(entity::Column::Status.eq("rehearsing"))
        .filter(entity::Column::TaskRevision.eq(row.task_revision))
        .filter(entity::Column::TargetDeviceId.eq(device))
        .filter(entity::Column::Prompt.eq(&row.prompt))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let current = rehearsal::Entity::find_by_id(row.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if current.status != "running"
        || current.started_at.is_none()
        || current.conversation_id != row.conversation_id
        || current.prompt_sha256 != digest(&message.text)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}
