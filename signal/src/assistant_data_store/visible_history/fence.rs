//! Original actor/device/conversation and raw-state fences for ordinary saves.

use super::*;
use crate::config::ConfigConnection;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

pub(super) async fn prepare_on(
    db: &DatabaseTransaction,
    expected: &PersistedAgentSession,
) -> Result<
    (
        Box<agent_session::Model>,
        Box<PersistedAgentSession>,
        AssistantDataPolicy,
    ),
    AssistantStoreError,
> {
    if expected.surface != desk_diagnose_core::session::AgentSessionSurface::AiAssistant
        || !expected.agent_role.is_main()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if expected.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    check_device_on(db, expected).await?;
    let policy = db.config_read().await.assistant_data_policy.clone();

    let row = read_on(db, expected).await?;
    let persisted = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AssistantStoreError::Invalid("invalid original assistant save session"))?,
    );
    let old = persisted
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let next = expected
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    if persisted.actor_id != expected.actor_id
        || persisted.device_id != expected.device_id
        || persisted.conversation_id != expected.conversation_id
        || persisted.agent_role != expected.agent_role
        || persisted.surface != expected.surface
        || persisted.version != expected.version
        || persisted.lease_token != expected.lease_token
        || persisted.input_revision != expected.input_revision
        || persisted.control_revision != expected.control_revision
        || persisted.current_turn_id != expected.current_turn_id
        || persisted.trigger_origin != expected.trigger_origin
        || old.source_device_ref != next.source_device_ref
        || old.source_instance_id != next.source_instance_id
        || old.temporary != next.temporary
        || old.current_actor_input_seq != next.current_actor_input_seq
        || old.current_input_recorded_at_unix_ms != next.current_input_recorded_at_unix_ms
        || old.current_input_timezone != next.current_input_timezone
        || old.instructions != next.instructions
        || row.version != expected.version
        || unsigned(row.lease_token)? != expected.lease_token
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    old.validate().map_err(AssistantStoreError::Invalid)?;
    next.validate().map_err(AssistantStoreError::Invalid)?;
    Ok((Box::new(row), persisted, policy))
}

pub(super) async fn finish_on(
    db: &DatabaseTransaction,
    expected: &PersistedAgentSession,
    original: &agent_session::Model,
    needs_live_lease: bool,
) -> Result<Option<u64>, AssistantStoreError> {
    check_device_on(db, expected).await?;
    let row = read_on(db, expected).await?;
    if row.id != original.id
        || row.version != original.version
        || row.lease_token != original.lease_token
        || row.state_json != original.state_json
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if needs_live_lease {
        let deadline = row
            .lease_deadline
            .ok_or(AssistantStoreError::Inaccessible)?;
        if deadline.timestamp_millis() <= chrono::Utc::now().timestamp_millis() {
            return Err(AssistantStoreError::Inaccessible);
        }
        return Ok(Some(unsigned(deadline.timestamp_millis())?));
    }
    Ok(None)
}

async fn read_on(
    db: &DatabaseTransaction,
    expected: &PersistedAgentSession,
) -> Result<agent_session::Model, AssistantStoreError> {
    agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(&expected.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&expected.device_id))
        .filter(agent_session::Column::ConversationId.eq(&expected.conversation_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)
}

async fn check_device_on(
    db: &DatabaseTransaction,
    expected: &PersistedAgentSession,
) -> Result<(), AssistantStoreError> {
    let data = expected
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let config = db.config_read().await;
    let binding = desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::AssistantDeviceBinding,
        &(&config.metadata.instance, &expected.device_id),
    );
    if expected.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        || data.source_device_ref != binding
        || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
