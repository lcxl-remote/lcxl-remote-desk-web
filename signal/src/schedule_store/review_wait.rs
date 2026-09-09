//! Wait one bounded tick for the owner decision without holding a DB transaction.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_session;
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

impl ScheduleStore {
    pub(crate) async fn poll_review_decision(
        &self,
        session: &mut PersistedAgentSession,
        schedule_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        if session.pending_schedule_review.as_deref() != Some(schedule_id)
            || !session.turn_state.is_active()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&session.actor_id))
            .filter(agent_session::Column::DeviceId.eq(&session.device_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.lease_token
            != i64::try_from(session.lease_token).map_err(|_| ScheduleStoreError::Invalid)?
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = self.database_time().await?;
        if row
            .lease_deadline
            .is_none_or(|deadline| deadline.timestamp_millis() <= now)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let current = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if current.version != row.version {
            return Err(ScheduleStoreError::Conflict);
        }
        if &current == session {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            return Ok(false);
        }
        let event = current
            .conversation
            .last()
            .ok_or(ScheduleStoreError::Conflict)?;
        let decision: serde_json::Value =
            serde_json::from_str(&event.text).map_err(|_| ScheduleStoreError::Conflict)?;
        if event.role != desk_diagnose_core::chat::ChatRole::SystemEvent
            || decision["schedule_id"].as_str() != Some(schedule_id)
            || !matches!(
                decision["event"].as_str(),
                Some("scheduled_task_activated" | "scheduled_task_rejected")
            )
        {
            return Err(ScheduleStoreError::Conflict);
        }
        // Only the exact approval receipt may advance this waiting snapshot.
        // Never adopt a new user input, changed scope, stopped turn or another lease.
        let mut expected = session.clone();
        expected.conversation.push(event.clone());
        expected.version = expected
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        expected.updated_at = current.updated_at.clone();
        if expected != current || current.version != row.version {
            return Err(ScheduleStoreError::Conflict);
        }
        *session = current;
        Ok(true)
    }
}
