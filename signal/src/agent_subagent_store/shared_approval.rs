//! Validate the original task before using its root owner approval switch.
use super::*;

pub(crate) async fn shared_approval_current_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    session: &PersistedAgentSession,
    now_ms: i64,
) -> Result<bool, DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(true);
    };
    if !binding.source.allows_delegated_review() || !session.allows_delegated_review() {
        return Ok(false);
    }
    if !check_child_permission_on(db, session, now_ms).await? {
        return Ok(false);
    }
    task_context_on(db, session).await?.ok_or_else(invalid)?;
    let parent = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&binding.root_conversation_id))
        .filter(session_row::Column::ActorId.eq(&session.actor_id))
        .filter(session_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let parent = PersistedAgentSession::decode_json(&parent.state_json).map_err(|_| invalid())?;
    if !parent.agent_role.is_main()
        || parent.surface != AgentSessionSurface::AiAssistant
        || parent.conversation_id != binding.root_conversation_id
        || parent.actor_id != session.actor_id
        || parent.device_id != session.device_id
    {
        return Err(invalid());
    }
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    Ok(
        decode_group(&group)?.source_admission == SourceAdmission::Open
            && !deleted_on(db, &binding.root_conversation_id).await?,
    )
}
