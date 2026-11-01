//! Model-side reads use the current file snapshot and the original native run.

use super::{AssistantStoreError, ensure_runtime_on, reconcile_settings_on, unsigned};
use crate::{
    config::{ConfigConnection, connection::DatabaseConnection},
    entity::agent_session,
};
use desk_diagnose_core::{
    ai_assistant::data_access::{
        AssistantDataToolAccess, retained_context_eligible, validate_persisted_access,
    },
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};

pub(crate) async fn context_access(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    _now: u64,
) -> Result<Option<AssistantDataToolAccess>, AssistantStoreError> {
    if !retained_context_eligible(session) {
        return Ok(None);
    }
    if session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let txn = db.begin().await?;
    let config = txn.config_read().await;
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let binding = native_ids::stable(
        Kind::AssistantDeviceBinding,
        &(&config.metadata.instance, &session.device_id),
    );
    if data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
        || data.source_device_ref != binding
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    ensure_runtime_on(&txn, &session.actor_id).await?;
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&session.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&session.device_id))
        .one(&txn)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.version != session.version || unsigned(row.lease_token)? != session.lease_token {
        return Err(AssistantStoreError::Inaccessible);
    }
    validate_persisted_access(&row.state_json, session).map_err(AssistantStoreError::Invalid)?;
    let state = reconcile_settings_on(&txn, &session.actor_id).await?;
    let access = AssistantDataToolAccess {
        settings: config.assistant_data_settings.clone(),
        policy_revision: config.assistant_data_policy.revision,
        memory_state_generation: unsigned(state.memory_state_generation)?,
        control_generation: unsigned(state.control_generation)?,
    };
    access.validate().map_err(AssistantStoreError::Invalid)?;
    txn.commit().await?;
    Ok(Some(access))
}
