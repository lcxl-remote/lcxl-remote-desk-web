//! Commit an AI draft and its original tool-result receipt under one turn fence.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_session;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ToolCall},
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
impl ScheduleStore {
    pub(crate) async fn propose_from_session(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<String, ScheduleStoreError> {
        let owner: i32 = session
            .actor_id
            .parse()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let draft = desk_diagnose_core::schedule::proposal::draft(session, call)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        if owner != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID {
            return Err(ScheduleStoreError::NotFound);
        }
        let locked =
            agent_session::Entity::update_many()
                .col_expr(
                    agent_session::Column::Version,
                    sea_orm::sea_query::Expr::col(agent_session::Column::Version),
                )
                .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
                .filter(agent_session::Column::ActorId.eq(&session.actor_id))
                .filter(agent_session::Column::DeviceId.eq(&session.device_id))
                .filter(agent_session::Column::Version.eq(session.version))
                .filter(agent_session::Column::LeaseToken.eq(
                    i64::try_from(session.lease_token).map_err(|_| ScheduleStoreError::Invalid)?,
                ))
                .exec(&txn)
                .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let now = super::authority::authority_now(&txn).await?;
        if row.state_json
            != session
                .encode_json_for_storage()
                .map_err(|_| ScheduleStoreError::Invalid)?
            || row
                .lease_deadline
                .is_none_or(|deadline| deadline.timestamp_millis() <= now)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let parents: Vec<_> = session
            .conversation
            .iter()
            .filter(|message| {
                message.role == ChatRole::Assistant
                    && message.tool_calls.iter().any(|tool| {
                        tool.id == call.id
                            && tool.name == call.name
                            && tool.arguments_json == call.arguments_json
                    })
            })
            .collect();
        if parents.len() != 1 || !session.unclosed_tool_call_ids().contains(&call.id) {
            return Err(ScheduleStoreError::Conflict);
        }
        let parent = parents[0]
            .data_envelope
            .as_ref()
            .ok_or(ScheduleStoreError::Invalid)?;
        desk_diagnose_core::schedule::validate_publication(
            &draft.spec,
            now,
            draft.kind == desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
        let task = Self::create_draft_on(&txn, owner, &draft, now).await?;
        let content = serde_json::json!({"schedule_id":task.schedule_id,"kind":task.kind,"state":"draft",
            "message":"Owner review is required. Scheduling is not enabled and no execution permission has been granted."}).to_string();
        let envelope = desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            Some(parent),
            &call.id,
            &content,
            "schedule_proposal",
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
        let mut next = session.clone();
        let mut message =
            ChatMessage::tool_result(format!("schedule-proposal:{}", call.id), &call.id, content);
        message.turn_id = next.current_turn_id.clone();
        message.data_envelope = envelope;
        next.conversation.push(message);
        next.version = next
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        next.updated_at = timestamp.to_rfc3339();
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                version: Set(next.version),
                state_json: Set(next
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        txn.commit().await?;
        *session = next;
        Ok(task.schedule_id)
    }
}
