//! Native actor, current session and task fences for current reads.

use super::{AssistantStoreError, unsigned};
use crate::{
    config::{ConfigConnection, connection::DatabaseTransaction},
    entity::agent_session,
};
use desk_agent_protocol::ai_assistant::{
    source::SourceReadPurpose, user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::{
            AssistantDataToolAccess, retained_context_eligible, validate_session_access,
        },
        memory_access::usable_memory_scope,
    },
    session::{PersistedAgentSession, TurnState},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

pub(crate) struct NativeSessionFence<'a> {
    pub db: &'a DatabaseTransaction,
    pub session: &'a PersistedAgentSession,
    pub access: AssistantDataToolAccess,
    pub policy: AssistantDataPolicy,
    pub enabled: bool,
    pub deadline: u64,
    row: agent_session::Model,

    not_before: u64,
}

pub(crate) async fn prepare_on<'a>(
    db: &'a DatabaseTransaction,
    session: &'a PersistedAgentSession,
    now: u64,
) -> Result<Box<NativeSessionFence<'a>>, AssistantStoreError> {
    if !retained_context_eligible(session) {
        return Err(AssistantStoreError::Inaccessible);
    }

    if session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    check_device_on(db, session).await?;
    let policy = db.config_read().await.assistant_data_policy.clone();
    super::ensure_runtime_on(db, &session.actor_id).await?;
    let row = session_on(db, session).await?;
    let mut deadline = validate_session(&row, session, now)?;
    let access = access_on(db, session, &policy).await?;
    let enabled = desk_diagnose_core::ai_assistant::data_access::eligible_origin(session)
        && usable_memory_scope(session, now).is_some()
        && access.settings.automatic_memory_enabled;
    if !enabled
        && session.assistant_data.as_ref().is_some_and(|data| {
            !data.current_memories.is_empty()
                || data.read_uses.iter().any(|source| {
                    matches!(
                        source.purpose,
                        SourceReadPurpose::ModelMemory | SourceReadPurpose::MemoryEvidence
                    )
                })
        })
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if let Some(scope) = usable_memory_scope(session, now) {
        scope.validate().map_err(AssistantStoreError::Invalid)?;
        if let Some(until) = scope.expires_at_unix_ms {
            deadline = deadline.min(until);
        }
    }
    if let Some(until) = super::model_gate::task_deadline_on(db, session, now).await? {
        deadline = deadline.min(until);
    }
    live_time(now, deadline)?;
    Ok(Box::new(NativeSessionFence {
        db,
        session,
        access,
        policy,
        enabled,
        deadline,
        row,

        not_before: now,
    }))
}

impl NativeSessionFence<'_> {
    pub(crate) async fn finish_on(&self, now: u64) -> Result<u64, AssistantStoreError> {
        // A wall-clock correction cannot move this operation's validation time backwards.
        let now = now.max(self.not_before);
        check_device_on(self.db, self.session).await?;
        let row = session_on(self.db, self.session).await?;
        if row.state_json != self.row.state_json
            || row.version != self.row.version
            || row.lease_token != self.row.lease_token
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let mut deadline = self
            .deadline
            .min(validate_session(&row, self.session, now)?);
        if access_on(self.db, self.session, &self.policy).await? != self.access {
            return Err(AssistantStoreError::Inaccessible);
        }
        let enabled = desk_diagnose_core::ai_assistant::data_access::eligible_origin(self.session)
            && usable_memory_scope(self.session, now).is_some()
            && self.access.settings.automatic_memory_enabled;
        if enabled != self.enabled {
            return Err(AssistantStoreError::Inaccessible);
        }
        if let Some(scope) = usable_memory_scope(self.session, now)
            && let Some(until) = scope.expires_at_unix_ms
        {
            deadline = deadline.min(until);
        }
        if let Some(until) = super::model_gate::task_deadline_on(self.db, self.session, now).await?
        {
            deadline = deadline.min(until);
        }
        live_time(now, deadline)?;
        Ok(deadline)
    }
}

async fn check_device_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<(), AssistantStoreError> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let config = db.config_read().await;
    let binding = desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::AssistantDeviceBinding,
        &(&config.metadata.instance, &session.device_id),
    );
    if session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        || data.source_device_ref != binding
        || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

async fn session_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<agent_session::Model, AssistantStoreError> {
    agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&session.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)
}

fn validate_session(
    row: &agent_session::Model,
    expected: &PersistedAgentSession,
    now: u64,
) -> Result<u64, AssistantStoreError> {
    let persisted = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AssistantStoreError::Invalid("invalid original current-memory session"))?,
    );
    validate_session_access(&persisted, expected).map_err(AssistantStoreError::Invalid)?;
    let deadline = unsigned(
        row.lease_deadline
            .ok_or(AssistantStoreError::Inaccessible)?
            .timestamp_millis(),
    )?;
    if !retained_context_eligible(&persisted)
        || persisted.trigger_origin != expected.trigger_origin
        || persisted.focus_epoch != expected.focus_epoch
        || persisted.turn_state != TurnState::Running
        || row.actor_id != persisted.actor_id
        || row.device_id != persisted.device_id
        || row.conversation_id != persisted.conversation_id
        || row.version != persisted.version
        || unsigned(row.lease_token)? != persisted.lease_token
        || deadline <= now
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(deadline)
}

async fn access_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    policy: &AssistantDataPolicy,
) -> Result<AssistantDataToolAccess, AssistantStoreError> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let runtime = super::runtime_on(db, &session.actor_id).await?;
    if runtime.settings_instance_id != data.source_instance_id {
        return Err(AssistantStoreError::Inaccessible);
    }
    let access = AssistantDataToolAccess {
        settings: super::settings_on(db, &session.actor_id).await?,
        policy_revision: policy.revision,
        memory_state_generation: unsigned(runtime.memory_state_generation)?,
        control_generation: unsigned(runtime.control_generation)?,
    };
    access.validate().map_err(AssistantStoreError::Invalid)?;
    if access.memory_state_generation < data.memory_state_generation
        || access.control_generation < data.control_generation
        || access.memory_state_generation != data.memory_state_generation
        || access.control_generation != data.control_generation
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(access)
}

fn live_time(not_before: u64, deadline: u64) -> Result<(), AssistantStoreError> {
    let now = unsigned(chrono::Utc::now().timestamp_millis())?.max(not_before);
    if now >= deadline {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
