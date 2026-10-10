//! Durable control updates, independently observable inbox events and child admission.
use super::*;
use desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentEvent;
use desk_diagnose_core::subagent::control::synchronize_session;
use sea_orm::sea_query::OnConflict;
use sha2::{Digest, Sha256};

pub(crate) async fn synchronize_control_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    run: &SubAgentRun,
    now_ms: i64,
    deadline_reached: bool,
) -> Result<desk_diagnose_core::model_observability::permission::PendingEnds, DbErr> {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .filter(session_row::Column::ActorId.eq(&run.actor_id))
        .filter(session_row::Column::DeviceId.eq(&run.device_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
    {
        return Err(invalid());
    }
    let now = chrono::DateTime::from_timestamp_millis(now_ms).ok_or_else(invalid)?;
    if matches!(
        run.state,
        SubAgentState::Failed | SubAgentState::Cancelled | SubAgentState::Cancelling
    ) {
        let operation = format!(
            "terminal:{}:{}",
            run.binding.task_id, run.binding.control_revision
        );
        super::native_cancel::cancel_native_actions_on(db, &session, &operation, now_ms).await?;
    }
    let permission_ends =
        desk_diagnose_core::model_observability::permission::PendingEnds::task_control(
            &session,
            run,
            deadline_reached,
        );
    let input_changed = session.input_revision != run.binding.input_revision;
    synchronize_session(&mut session, run, &now.to_rfc3339()).map_err(|_| invalid())?;
    if input_changed {
        let task = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&run.binding.task_id))
            .filter(run_row::Column::ChildConversationId.eq(&run.child_conversation_id))
            .one(db)
            .await?
            .ok_or_else(invalid)?;
        let context: desk_diagnose_core::subagent::creation::TaskCreationEnvelope =
            serde_json::from_str(&task.creation_envelope_json).map_err(|_| invalid())?;
        context.validate_task(&run.binding).map_err(|_| invalid())?;
        if session
            .conversation
            .iter()
            .any(|message| message.message_id == context.instruction.message_id)
        {
            return Err(invalid());
        }
        session.conversation.push(context.instruction);
    }
    session.version = row.version.checked_add(1).ok_or_else(invalid)?;
    let changed = session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
            version: Set(session.version),
            lease_token: Set(i64::try_from(session.lease_token).map_err(|_| invalid())?),
            lease_deadline: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .filter(session_row::Column::Version.eq(row.version))
        .filter(session_row::Column::LeaseToken.eq(row.lease_token))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(permission_ends)
}

/// One immutable notification per task-state revision. UI reads and model
/// result consumption have separate fields and are never inferred from progress.
pub(crate) async fn append_state_event_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    group: &DelegationGroup,
    run: &SubAgentRun,
    now_ms: i64,
) -> Result<(), DbErr> {
    use crate::entity::agent_subagent_inbox as inbox;
    if group.group_id != run.binding.group_id
        || group.root_conversation_id != run.binding.root_conversation_id
        || group.actor_id != run.actor_id
        || group.device_id != run.device_id
    {
        return Err(invalid());
    }
    let key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &group.root_conversation_id,
                &run.binding.task_id,
                run.state_revision,
            ))
            .map_err(|_| invalid())?
        )
    );
    let event_id = format!("subagent-event-{key}");
    let event = AiAssistantSubAgentEvent {
        event_id: event_id.clone(),
        task: run.summary(),
        parent_input_revision: group.parent_input_revision,
        parent_control_revision: group.parent_control_revision,
        created_at: chrono::DateTime::from_timestamp_millis(now_ms)
            .ok_or_else(invalid)?
            .to_rfc3339(),
    };
    let event_json = serde_json::to_string(&event).map_err(|_| invalid())?;
    inbox::Entity::insert(inbox::ActiveModel {
        event_id: Set(event_id),
        dedup_key_sha256: Set(key.clone()),
        root_conversation_id: Set(group.root_conversation_id.clone()),
        group_id: Set(group.group_id.clone()),
        task_id: Set(run.binding.task_id.clone()),
        actor_id: Set(run.actor_id.clone()),
        device_id: Set(run.device_id.clone()),
        state_revision: Set(i64::try_from(run.state_revision).map_err(|_| invalid())?),
        event_kind: Set(run.state.as_str().into()),
        event_json: Set(event_json.clone()),
        parent_input_revision: Set(
            i64::try_from(group.parent_input_revision).map_err(|_| invalid())?
        ),
        parent_control_revision: Set(
            i64::try_from(group.parent_control_revision).map_err(|_| invalid())?
        ),
        ui_read_at_ms: Set(None),
        model_observed_at_ms: Set(None),
        observed_tool_call_id: Set(None),
        observed_message_id: Set(None),
        interpreted_at_ms: Set(None),
        interpreted_turn_id: Set(None),
        created_at: Set(now_ms),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(inbox::Column::DedupKeySha256)
            .do_nothing()
            .to_owned(),
    )
    .try_insert()
    .exec(db)
    .await?;
    let persisted = inbox::Entity::find()
        .filter(inbox::Column::DedupKeySha256.eq(&key))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    // Idempotency fixes the original event timestamp rather than replacing it.
    let original: AiAssistantSubAgentEvent =
        serde_json::from_str(&persisted.event_json).map_err(|_| invalid())?;
    if original.task != event.task
        || original.parent_input_revision != event.parent_input_revision
        || original.parent_control_revision != event.parent_control_revision
        || original.event_id != event.event_id
    {
        return Err(invalid());
    }
    Ok(())
}

/// Permission recording may proceed while its source is paused. It cannot
/// revive a cancelled task, adopt a newer child input or spend an expired source.
pub(crate) async fn check_child_permission_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    session: &PersistedAgentSession,
    now_ms: i64,
) -> Result<bool, DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(true);
    };
    let Some(row) = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(run_row::Column::ActorId.eq(&session.actor_id))
        .filter(run_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
    else {
        return Ok(false);
    };
    let run = decode_run(&row)?;
    let Some(row) = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
    else {
        return Ok(false);
    };
    let group = decode_group(&row)?;
    Ok(run.validate_session(session).is_ok()
        && !run.state.is_terminal()
        && run.state != SubAgentState::Cancelling
        && group.source_admission != SourceAdmission::Closed
        && run.binding.source == group.source
        && run.binding.source_epoch == group.source_epoch
        && now_ms < run.binding.deadline_ms
        && now_ms < group.limits.deadline_ms)
}
