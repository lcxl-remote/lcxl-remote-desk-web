//! Propagate a persisted occurrence cancellation to original device actions.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_action_item as action, agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{PersistedAgentSession, TriggerOrigin};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

impl ScheduleStore {
    pub(super) async fn action_cancel_candidates(
        &self,
        after: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after))
            .filter(run::Column::CancelRequestedAt.is_not_null())
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    /// Stop requests are monotonic and bound to original action identities. A
    /// completed action remains completed; no current execution grant is minted.
    pub async fn propagate_task_cancellation(
        &self,
        run_id: &str,
    ) -> Result<usize, ScheduleStoreError> {
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.cancel_requested_at.is_none()
            || work.started_at.is_none()
            || work.conversation_id != work.run_id
        {
            return Ok(0);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(work.owner_user_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.kind != "fresh_task" {
            return Ok(0);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.actor_id != work.owner_user_id.to_string()
            || row.actor_id != session.actor_id
            || row.device_id != session.device_id
            || session.device_id != task.target_device_id
            || session.conversation_id != work.run_id
            || session.current_request_id.as_deref() != Some(run_id)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let actions = action::Entity::find()
            .filter(action::Column::ConversationId.eq(run_id))
            .filter(action::Column::Kind.eq(crate::capability_grant_store::CAPABILITY_WORK_KIND))
            .filter(action::Column::CancelRequestedAt.is_null())
            .all(&self.db)
            .await?;
        let mut recorded = 0;
        for action in actions {
            if !matches!(
                action.status.as_str(),
                crate::capability_grant_store::CAPABILITY_WORK_DISPATCHING
                    | crate::capability_grant_store::CAPABILITY_WORK_OUTCOME_UNKNOWN
            ) || action.actor_id != session.actor_id
                || action.target_device_id.clone() != session.device_id
                || action.turn_id != work.turn_id
            {
                continue;
            }
            let key = format!("task-cancel:{}:{}", work.run_id, action.id);
            let result =
                crate::capability_grant_store::SignalCapabilityGrantStore::new(self.db.clone())
                    .request_computer_execution_cancel(
                        &action.action_request_id,
                        run_id,
                        &session.actor_id,
                        &session.device_id,
                        &key,
                        "Owner cancelled this scheduled occurrence",
                    )
                    .await;
            if matches!(result, Ok(true)) {
                recorded += 1;
            }
        }
        Ok(recorded)
    }
}
