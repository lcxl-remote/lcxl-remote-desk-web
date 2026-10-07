//! Global metadata-only attention routes child work through its trusted root.
use super::*;
use crate::entity::agent_subagent_inbox as inbox;
use desk_signal_facade::controller::ai_assistant_session::{
    AiAssistantAttentionItemDto, AiAssistantAttentionReason,
};
use std::collections::{HashMap, HashSet};

pub(crate) async fn augment_owner_attention_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    devices: Option<&HashSet<String>>,
    items: &mut Vec<AiAssistantAttentionItemDto>,
) -> Result<(), DbErr> {
    let mut query = session_row::Entity::find().filter(session_row::Column::ActorId.eq(actor));

    if let Some(devices) = devices {
        if devices.is_empty() {
            return Ok(());
        }
        query = query.filter(session_row::Column::DeviceId.is_in(devices.iter().cloned()));
    }
    let mut sessions = HashMap::new();
    for row in query.all(db).await? {
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.surface == AgentSessionSurface::AiAssistant
            && session.actor_id == actor
            && session.device_id == row.device_id
            && session.conversation_id == row.conversation_id
        {
            sessions.insert(row.conversation_id, session);
        }
    }
    let mut roots = HashMap::new();
    for (id, session) in &sessions {
        if session.agent_role.is_main() && !deleted_on(db, id).await? {
            roots.insert(id.clone(), session);
        }
    }
    let mut routed = Vec::with_capacity(items.len());
    for mut item in items.drain(..) {
        if let Some(binding) = sessions
            .get(&item.session_id)
            .and_then(|session| session.agent_role.binding())
        {
            let Some(parent) = roots
                .get(&binding.root_conversation_id)
                .filter(|root| root.device_id == item.device_id)
            else {
                continue;
            };
            item.session_id = parent.conversation_id.clone();
            item.client_conversation_id = parent.client_conversation_id.clone();
        }
        routed.push(item);
    }
    *items = routed;
    let mut event_query = inbox::Entity::find()
        .filter(inbox::Column::ActorId.eq(actor))
        .filter(inbox::Column::UiReadAtMs.is_null())
        .filter(inbox::Column::EventKind.is_in(["completed", "failed", "cancelled"]));
    if let Some(devices) = devices {
        event_query = event_query.filter(inbox::Column::DeviceId.is_in(devices.iter().cloned()));
    }
    let events = event_query.all(db).await?;
    if events.is_empty() {
        return Ok(());
    }
    let tasks = run_row::Entity::find()
        .filter(run_row::Column::ActorId.eq(actor))
        .filter(run_row::Column::TaskId.is_in(events.iter().map(|event| event.task_id.clone())))
        .all(db)
        .await?
        .into_iter()
        .map(|row| (row.task_id.clone(), row))
        .collect::<HashMap<_, _>>();
    for event in events {
        let Some(parent) = roots
            .get(&event.root_conversation_id)
            .filter(|root| root.device_id == event.device_id)
        else {
            continue;
        };
        let Some(row) = tasks.get(&event.task_id) else {
            continue;
        };
        let run = decode_run(row)?;
        if !run.state.is_terminal()
            || row.root_conversation_id != event.root_conversation_id
            || row.device_id != event.device_id
            || row.group_id != event.group_id
            || run.state_revision < u64::try_from(event.state_revision).map_err(|_| invalid())?
        {
            continue;
        }
        items.push(AiAssistantAttentionItemDto {
            attention_id: format!("subagent:{}", event.event_id),
            session_id: parent.conversation_id.clone(),
            client_conversation_id: parent.client_conversation_id.clone(),
            device_id: parent.device_id.clone(),
            goal_id: run.binding.source.goal_id().map(str::to_owned),
            request_id: Some(event.task_id),
            reason: AiAssistantAttentionReason::SubAgentResult,
            state_version: u64::try_from(event.state_revision).map_err(|_| invalid())?,
            updated_at_unix_ms: u64::try_from(event.created_at).map_err(|_| invalid())?,
            deadline_unix_ms: None,
        });
    }
    Ok(())
}
