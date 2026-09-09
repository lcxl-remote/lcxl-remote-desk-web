//! Settle failed or cancelled rehearsals only when no tool or durable action exists.
use super::*;
use crate::entity::{agent_action_item as work_item, agent_exec_task, agent_session};
use desk_diagnose_core::{
    schedule::rehearsal::{RehearsalSource, terminal_without_tools},
    session::PersistedAgentSession,
};
use sea_orm::QuerySelect;

impl ScheduleStore {
    /// Session and action rows are authoritative; an error return alone is insufficient.
    pub async fn finish_cancelled_rehearsal(
        &self,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        self.finish_unsuccessful_rehearsal(owner, rehearsal_id, Some(false))
            .await
    }

    pub async fn finish_failed_rehearsal(
        &self,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        self.finish_unsuccessful_rehearsal(owner, rehearsal_id, Some(true))
            .await
    }

    /// Select the terminal status from the locked durable session, not the error kind.
    pub async fn finish_terminal_rehearsal(
        &self,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        self.finish_unsuccessful_rehearsal(owner, rehearsal_id, None)
            .await
    }

    async fn finish_unsuccessful_rehearsal(
        &self,
        owner: i32,
        rehearsal_id: &str,
        expected_failed: Option<bool>,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let original = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&original.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&original.conversation_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let original = rehearsal::Entity::find_by_id(original.id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let (terminal, status) = match session.turn_state {
            desk_diagnose_core::session::TurnState::Failed if expected_failed != Some(false) => {
                (session.turn_state, "failed")
            }
            desk_diagnose_core::session::TurnState::Cancelled if expected_failed != Some(true) => {
                (session.turn_state, "cancelled")
            }
            _ => return Err(ScheduleStoreError::Conflict),
        };
        if row.actor_id != owner.to_string()
            || row.device_id != original.target_device_id
            || row.version != session.version
            || Some(row.lease_token) != i64::try_from(session.lease_token).ok()
            || row.lease_deadline.is_some()
            || original.started_at.is_none()
            || original.prompt_sha256 != digest(&original.prompt)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let actor = owner.to_string();
        let input_id = format!("rehearsal:{}:input", original.rehearsal_id);
        let source = RehearsalSource {
            actor: &actor,
            device: &original.target_device_id,
            conversation: &original.conversation_id,
            client_conversation: &original.client_conversation_id,
            input_message_id: &input_id,
            prompt: &original.prompt,
        };
        if terminal_without_tools(&session, &source) != Some(terminal) {
            return Err(ScheduleStoreError::Conflict);
        }
        let snapshot = digest(&row.state_json);
        if original.status == status {
            return if original.completed_session_version == Some(row.version)
                && original.completed_session_sha256.as_deref() == Some(snapshot.as_str())
                && original.answer_message_id.is_none()
                && original.finished_at.is_some()
            {
                Ok(original)
            } else {
                Err(ScheduleStoreError::Conflict)
            };
        }
        if original.status != "running"
            || original.finished_at.is_some()
            || task.status != "rehearsing"
            || task.active_run_id.is_some()
            || task.task_revision != original.task_revision
            || task.target_device_id != original.target_device_id
            || task.prompt != original.prompt
            || task.model_id != original.model_id
            || task.locale != original.locale
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let actions = work_item::Entity::find()
            .filter(work_item::Column::ConversationId.eq(&original.conversation_id))
            .lock_exclusive()
            .all(&txn)
            .await?;
        if !actions.is_empty() {
            return Err(ScheduleStoreError::Conflict);
        }
        if agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ConversationId.eq(&original.conversation_id))
            .one(&txn)
            .await?
            .is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::super::authority::authority_now(&txn).await?;
        if original.started_at.is_some_and(|started| started > now) {
            return Err(ScheduleStoreError::Invalid);
        }
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                status: Set("draft".into()),
                next_run_at: Set(None),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        rehearsal::Entity::update_many()
            .set(rehearsal::ActiveModel {
                status: Set(status.into()),
                finished_at: Set(Some(now)),
                completed_session_version: Set(Some(row.version)),
                completed_session_sha256: Set(Some(snapshot)),
                answer_message_id: Set(None),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(rehearsal::Column::Id.eq(original.id))
            .exec(&txn)
            .await?;
        let result = rehearsal::Entity::find_by_id(original.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::Invalid)?;
        txn.commit().await?;
        Ok(result)
    }
}
