//! One-shot admission of the reserved, fresh interactive rehearsal.
use super::*;
use crate::entity::agent_session;

impl ScheduleStore {
    /// Admission is not authorization: the interactive driver must still perform
    /// its normal current owner, model, readiness and per-action permission checks.
    pub async fn claim_rehearsal(
        &self,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let row = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.status != "pending" || row.started_at.is_some() {
            return Err(ScheduleStoreError::Conflict);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&row.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.kind != "fresh_task"
            || task.status != "rehearsing"
            || task.active_run_id.is_some()
            || task.task_revision != row.task_revision
            || task.target_device_id != row.target_device_id
            || task.prompt != row.prompt
            || digest(&row.prompt) != row.prompt_sha256
            || task.model_id != row.model_id
            || task.locale != row.locale
            || !row.client_conversation_id.starts_with("rehearsal_")
            || !desk_diagnose_core::conversation_key::is_valid_client_conversation_id(
                &row.client_conversation_id,
            )
            || row.conversation_id
                != derive_conversation_key(
                    &owner.to_string(),
                    &row.target_device_id,
                    Some(&row.client_conversation_id),
                    "",
                )
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
        if failures
            .pause_reasons
            .contains(&desk_agent_protocol::schedule::SchedulePauseReason::UnknownSideEffect)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::super::queue::database_now(&txn).await?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
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
        // Never adopt a conversation that has already existed, even if idle.
        if agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&row.conversation_id))
            .one(&txn)
            .await?
            .is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let changed = rehearsal::Entity::update_many()
            .set(rehearsal::ActiveModel {
                status: Set("running".into()),
                started_at: Set(Some(now)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(rehearsal::Column::Id.eq(row.id))
            .filter(rehearsal::Column::Status.eq("pending"))
            .filter(rehearsal::Column::StartedAt.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = rehearsal::Entity::find_by_id(row.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::Invalid)?;
        txn.commit().await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
