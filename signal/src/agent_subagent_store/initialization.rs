//! Initialize finite source budgets before the first parent model call.

use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_diagnose_core::{
    chat::ChatMessage,
    goal::GoalRun,
    input_read_context::ReadContextSelection,
    subagent::{DelegationSource, creation::CreationEnvelope},
};
use sea_orm::ActiveModelTrait;

pub(crate) fn decode_creation(row: &group_row::Model) -> Result<CreationEnvelope, DbErr> {
    let creation: CreationEnvelope =
        serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
    creation.validate().map_err(|_| invalid())?;
    let group = decode_group(row)?;
    let model: desk_agent_protocol::data_lineage::DestinationIdentity =
        serde_json::from_str(&row.model_binding_json).map_err(|_| invalid())?;
    if creation.root_conversation_id != group.root_conversation_id
        || creation.actor_id != group.actor_id
        || creation.device_id != group.device_id
        || creation.source != group.source
        || creation.parent_input_revision != group.parent_input_revision
        || creation.parent_control_revision > group.parent_control_revision
        || creation.source_key().map_err(|_| invalid())? != row.source_key_sha256
        || creation.model_destination != model
    {
        return Err(invalid());
    }
    Ok(creation)
}

/// A goal approved after an ordinary input must establish its own source group
/// before claiming any model work. Later segments reuse the same allocation.
pub(crate) async fn initialize_goal_group_on(
    db: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    goal: &GoalRun,
    now_ms: i64,
) -> Result<(), DbErr> {
    if !session.agent_role.is_main()
        || session.surface != AgentSessionSurface::AiAssistant
        || goal.conversation_id != session.conversation_id
        || goal.owner_id != session.actor_id
        || goal.device_id != session.device_id
        || goal.input_revision != session.input_revision
    {
        return Err(invalid());
    }
    let id = session.delegation_group_id.as_deref().ok_or_else(invalid)?;
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(id))
        .filter(group_row::Column::RootConversationId.eq(&session.conversation_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&row)?;
    let creation = decode_creation(&row)?;
    let binding =
        desk_diagnose_core::goal::GoalModelBinding::from_destination(&creation.model_destination)
            .map_err(|_| invalid())?;
    if !group.can_interpret(session.input_revision, session.control_revision)
        || creation.owner_requirement.message_id != goal.source_message_id
        || binding != goal.model_binding
    {
        return Err(invalid());
    }
    match group.source {
        DelegationSource::Goal { goal_id } if goal_id == goal.goal_id => Ok(()),
        DelegationSource::UserInput { input_revision } if input_revision == goal.input_revision => {
            initialize_input_group_on(
                db,
                session,
                creation.owner_requirement,
                creation.original_read_context,
                Some(goal),
                now_ms,
            )
            .await
        }
        _ => Err(invalid()),
    }
}

/// The input transaction already holds owner and root control. No model request,
/// device dispatch or separate transaction is permitted inside this helper.
pub(crate) async fn initialize_input_group_on(
    db: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    owner_requirement: ChatMessage,
    read_context: Option<ReadContextSelection>,
    goal: Option<&GoalRun>,
    now_ms: i64,
) -> Result<(), DbErr> {
    let source = match goal {
        Some(goal) => DelegationSource::Goal {
            goal_id: goal.goal_id.clone(),
        },
        None => DelegationSource::UserInput {
            input_revision: session.input_revision,
        },
    };
    let creation = CreationEnvelope::capture(session, source, owner_requirement, read_context)
        .map_err(|error| DbErr::Custom(format!("capture delegation source: {}", error.message)))?;
    persist_source_group_on(db, session, &creation, goal, now_ms).await
}

/// Source capture and persistence share the caller's owner/root/task transaction.
pub(super) async fn persist_source_group_on(
    db: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    creation: &CreationEnvelope,
    goal: Option<&GoalRun>,
    now_ms: i64,
) -> Result<(), DbErr> {
    let group = creation.new_group(now_ms, goal).map_err(|error| {
        DbErr::Custom(format!("initialize delegation group: {}", error.message))
    })?;
    let key = creation.source_key().map_err(|_| invalid())?;
    if let Some(existing) = group_row::Entity::find()
        .filter(group_row::Column::SourceKeySha256.eq(&key))
        .one(db)
        .await?
    {
        if decode_creation(&existing)? != *creation {
            return Err(invalid());
        }
        let existing = decode_group(&existing)?;
        if !existing.can_interpret(session.input_revision, session.control_revision) {
            return Err(invalid());
        }
        session.delegation_group_id = Some(existing.group_id);
        return Ok(());
    }
    let (source_schedule_id, source_occurrence_id) = match &group.source {
        DelegationSource::ScheduledOccurrence {
            schedule_id,
            occurrence_id,
        } => (Some(schedule_id.clone()), Some(occurrence_id.clone())),
        _ => (None, None),
    };
    group_row::ActiveModel {
        group_id: Set(group.group_id.clone()),
        source_key_sha256: Set(key),
        root_conversation_id: Set(group.root_conversation_id.clone()),
        actor_id: Set(group.actor_id.clone()),
        device_id: Set(group.device_id.clone()),
        source_goal_id: Set(group.source.goal_id().map(str::to_string)),
        source_schedule_id: Set(source_schedule_id),
        source_occurrence_id: Set(source_occurrence_id),
        source_admission: Set(admission_label(group.source_admission).into()),
        source_epoch: Set(1),
        parent_input_revision: Set(
            i64::try_from(group.parent_input_revision).map_err(|_| invalid())?
        ),
        parent_control_revision: Set(
            i64::try_from(group.parent_control_revision).map_err(|_| invalid())?
        ),
        parent_active: Set(true),
        state_json: Set(serde_json::to_string(&group).map_err(|_| invalid())?),
        creation_envelope_json: Set(serde_json::to_string(&creation).map_err(|_| invalid())?),
        model_binding_json: Set(
            serde_json::to_string(&creation.model_destination).map_err(|_| invalid())?
        ),
        version: Set(1),
        deadline_ms: Set(group.limits.deadline_ms),
        created_at: Set(now_ms),
        updated_at: Set(now_ms),
        ..Default::default()
    }
    .insert(db)
    .await?;
    session.delegation_group_id = Some(group.group_id);
    Ok(())
}
