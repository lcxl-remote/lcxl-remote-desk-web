//! Owner deletion of retained history does not resurrect execution state.

use super::*;
use crate::entity::{
    agent_assistant_history_conversation as history, agent_file_recovery_cleanup as removal,
    agent_session as session,
};

pub(crate) async fn delete_retained_root_on(
    db: &DatabaseTransaction,
    actor: &str,
    device: &str,
    conversation: &str,
    now: i64,
) -> Result<bool, AssistantStoreError> {
    let Some(marker) = removal::Entity::find_by_id(conversation).one(db).await? else {
        return Ok(false);
    };
    if marker.actor_id != actor || marker.device_id != device || marker.created_at_unix_ms <= 0 {
        return Ok(false);
    }
    let Some(root) = history::Entity::find()
        .filter(history::Column::ConversationId.eq(conversation))
        .one(db)
        .await?
    else {
        return Ok(false);
    };
    if root.actor_id != actor
        || root.root_conversation_id != conversation
        || root.parent_conversation_id.is_some()
        || root.revision <= 0
        || root.deletion_generation <= 0
        || root.first_recorded_at_ms <= 0
        || root.last_recorded_at_ms < root.first_recorded_at_ms
        || !matches!(root.status.as_str(), "active" | "expired" | "user_deleted")
    {
        return Ok(false);
    }
    if !device_matches_on(db, actor, device, &root.device_ref).await? {
        return Ok(false);
    }
    // Do not hide another subject's execution row with an actor filter. The
    // archived path can remove history only after the original session is gone.
    if session::Entity::find()
        .filter(session::Column::ConversationId.eq(conversation))
        .one(db)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    let current = history::Entity::find()
        .filter(history::Column::ConversationId.eq(conversation))
        .one(db)
        .await?;
    if current.as_ref() != Some(&root)
        || !device_matches_on(db, actor, device, &root.device_ref).await?
    {
        return Ok(false);
    }
    mark_manual_delete_on(db, actor, device, conversation, now).await?;
    Ok(true)
}

async fn device_matches_on(
    db: &DatabaseTransaction,
    actor: &str,
    native_device: &str,
    source_device: &str,
) -> Result<bool, AssistantStoreError> {
    use crate::{config::ConfigConnection, entity::agent_assistant_device_binding as binding};
    if actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Ok(false);
    }
    if native_device.is_empty() {
        return Ok(source_device.is_empty());
    }
    let instance = db.config_read().await.metadata.instance.clone();
    Ok(binding::Entity::find()
        .filter(binding::Column::BindingId.eq(source_device))
        .filter(binding::Column::InstanceId.eq(instance))
        .filter(binding::Column::Audience.eq(native_device))
        .one(db)
        .await?
        .is_some())
}
