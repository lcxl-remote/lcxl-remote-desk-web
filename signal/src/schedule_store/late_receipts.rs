//! Reconcile late original receipts without changing the original outcome or failure policy.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_action_item as action, agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{
    ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

impl ScheduleStore {
    pub(super) async fn late_receipt_candidates(
        &self,
        after: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after))
            .filter(run::Column::Status.eq("outcome_unknown"))
            .filter(run::Column::FailureAccounted.eq(true))
            .filter(run::Column::FinishedAt.is_not_null())
            .filter(run::Column::ReceiptsReconciledAt.is_null())
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    pub async fn reconcile_late_task_receipts(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;

        let initial = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&initial.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(initial.owner_user_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.kind != "fresh_task" {
            return Ok(false);
        }
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
        let work = run::Entity::find_by_id(initial.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.status != "outcome_unknown"
            || !work.failure_accounted
            || work.finished_at.is_none()
            || work.receipts_reconciled_at.is_some()
            || work.lease_deadline.is_some()
            || work.conversation_id != work.run_id
        {
            return Ok(false);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(run_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.turn_state != TurnState::Failed
            || session.actor_id != work.owner_user_id.to_string()
            || session.actor_id != row.actor_id
            || session.device_id != task.target_device_id
            || session.device_id != row.device_id
            || session.conversation_id != work.run_id
            || session.version != row.version
            || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
            || row.lease_deadline.is_some()
            || session.current_request_id.as_deref() != Some(run_id)
            || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
            || session.input_revision != 1
            || !session.pending_auto_triggers.is_empty()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if !session.execution_state.unknown().is_some() {
            return Ok(false);
        }
        let actions = action::Entity::find()
            .filter(action::Column::ConversationId.eq(run_id))
            .lock_exclusive()
            .all(&txn)
            .await?;
        if actions.is_empty()
            || actions.iter().any(|action| {
                !(matches!(
                    action.status.as_str(),
                    crate::capability_grant_store::CAPABILITY_WORK_SUCCEEDED
                        | crate::capability_grant_store::CAPABILITY_WORK_FAILED
                ))
            })
        {
            return Ok(false);
        }
        let now = super::authority::authority_now(&txn).await?;
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        let context = super::TaskReceiptContext::load(&txn, &work).await?;
        if !(crate::capability_grant_store::fresh_recovery::restore_completed_calls(
            &txn,
            &mut session,
            &context,
            &timestamp.to_rfc3339(),
        )
        .await?)
            || session.execution_state != ExecutionState::None
            || !session.unclosed_tool_call_ids().is_empty()
        {
            return Ok(false);
        }
        session.version = session
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        session.updated_at = timestamp.to_rfc3339();
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(session
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                version: Set(session.version),
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
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                receipts_reconciled_at: Set(Some(now)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::Status.eq("outcome_unknown"))
            .filter(run::Column::ReceiptsReconciledAt.is_null())
            .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        txn.commit().await?;
        Ok(true)
    }
}
