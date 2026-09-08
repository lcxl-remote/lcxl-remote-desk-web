//! Bounded owner-only conversation choices; creation rechecks the original input.
use super::*;
use crate::entity::agent_session as session_row;
use desk_agent_protocol::schedule::management::ResumeConversationSource;
use desk_diagnose_core::{
    chat::ChatRole,
    conversation_key::{derive_conversation_key, is_valid_client_conversation_id},
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

pub(super) async fn list(
    db: &DatabaseConnection,
    owner: i32,
    public_device: &str,
    offset: u32,
    limit: u32,
) -> Result<Response, ScheduleStoreError> {
    if !(1..=50).contains(&limit)
        || public_device.is_empty()
        || public_device.len() > 256
        || public_device.chars().any(char::is_control)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    let device = public_device.to_owned();
    let actor = owner.to_string();
    let rows = session_row::Entity::find()
        .filter(session_row::Column::ActorId.eq(&actor))
        .filter(session_row::Column::DeviceId.eq(&device))
        .order_by_desc(session_row::Column::Id)
        .offset(u64::from(offset))
        .limit(u64::from(limit))
        .all(db)
        .await?;
    let next_offset = if rows.len() == limit as usize {
        Some(
            offset
                .checked_add(limit)
                .ok_or(ScheduleStoreError::Invalid)?,
        )
    } else {
        None
    };
    let mut sources = Vec::new();
    for row in rows {
        let Ok(session) = PersistedAgentSession::decode_json(&row.state_json) else {
            continue;
        };
        let Some(client) = session.client_conversation_id.as_deref() else {
            continue;
        };
        if !is_valid_client_conversation_id(client)
            || session.version != row.version
            || session.conversation_id != row.conversation_id
            || derive_conversation_key(&actor, &device, Some(client), "") != row.conversation_id
            || session.check_subject(&actor, &device).is_err()
            || session
                .check_surface(AgentSessionSurface::DeviceAssistant)
                .is_err()
            || session.input_revision == 0
            || session.input_revision > i64::MAX as u64
            || session.turn_state.is_active()
            || session.trigger_origin == TriggerOrigin::ScheduledTask
            || session.execution_state.has_unresolved_outcome()
        {
            continue;
        }
        let title = session
            .conversation
            .iter()
            .find(|message| message.role == ChatRole::User)
            .map(|message| message.text.chars().take(240).collect::<String>())
            .unwrap_or_else(|| client.into());
        sources.push(ResumeConversationSource {
            client_conversation_id: client.into(),
            target_device_id: public_device.into(),
            title,
            requirement_revision: session.input_revision,
        });
    }
    Ok(Response::ResumeSources {
        sources,
        next_offset,
    })
}
