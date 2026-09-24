//! Owner attention projected from durable goal and opening-request rows.
//! The projection carries no goal text or tool arguments into global UI.

use desk_signal_facade::controller::ai_assistant_session::AiAssistantAttentionItemDto;
use std::collections::{HashMap, HashSet};

use desk_diagnose_core::dynamic_run::PermissionRequestState;
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};

use crate::entity::{
    agent_approval_delegation, agent_goal_open_request, agent_goal_run, agent_session,
};

pub async fn list_goal_attention(
    db: &DatabaseConnection,
    actor_id: &str,
    now_unix_ms: u64,
) -> Result<Vec<AiAssistantAttentionItemDto>, DbErr> {
    let goal_rows = agent_goal_run::Entity::find()
        .filter(agent_goal_run::Column::ActorId.eq(actor_id))
        .filter(agent_goal_run::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .all(db)
        .await?;
    let mut items = Vec::new();
    for row in goal_rows {
        let goal = crate::agent_goal_store::decode(&row)?;
        items.extend(AiAssistantAttentionItemDto::for_goal(&goal, now_unix_ms));
    }

    let now = i64::try_from(now_unix_ms)
        .map_err(|_| DbErr::Custom("invalid owner attention clock".into()))?;
    let open_rows = agent_goal_open_request::Entity::find()
        .filter(agent_goal_open_request::Column::ActorId.eq(actor_id))
        .filter(agent_goal_open_request::Column::Status.eq("pending"))
        .filter(agent_goal_open_request::Column::ExpiresAt.gt(now))
        .all(db)
        .await?;
    for row in open_rows {
        let request = crate::agent_goal_open_store::decode(&row)?;
        items.push(AiAssistantAttentionItemDto::from_goal_open(&request));
    }

    let delegated: HashSet<String> = agent_approval_delegation::Entity::find()
        .filter(agent_approval_delegation::Column::ActorId.eq(actor_id))
        .filter(agent_approval_delegation::Column::Status.eq("active"))
        .all(db)
        .await?
        .into_iter()
        .map(|row| row.conversation_id)
        .collect();
    let sessions = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(actor_id))
        .all(db)
        .await?;
    let mut client_conversations = HashMap::new();
    for row in sessions {
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        client_conversations.insert(
            row.conversation_id.clone(),
            session.client_conversation_id.clone(),
        );
        if session.surface != AgentSessionSurface::AiAssistant || session.turn_state.is_active() {
            continue;
        }
        if delegated.contains(&row.conversation_id)
            && session.trigger_origin.allows_delegated_review()
        {
            continue;
        }
        for request in &session.permission_requests {
            if request.state == PermissionRequestState::Pending
                && request.input_revision == session.input_revision
            {
                items.push(AiAssistantAttentionItemDto::from_permission(
                    &row.conversation_id,
                    &row.device_id,
                    request,
                    u64::try_from(row.updated_at.timestamp_millis()).unwrap_or_default(),
                ));
            }
        }
    }
    for item in &mut items {
        item.client_conversation_id = client_conversations
            .get(&item.session_id)
            .cloned()
            .flatten();
    }
    items.sort_by(|left, right| {
        right
            .updated_at_unix_ms
            .cmp(&left.updated_at_unix_ms)
            .then_with(|| left.attention_id.cmp(&right.attention_id))
    });
    Ok(items)
}
