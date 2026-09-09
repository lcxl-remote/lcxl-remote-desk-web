//! Expire quiescent approval waits without reclaiming or dispatching work.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_action_item as work_item, agent_schedule_run as run, agent_session};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

impl ScheduleStore {
    pub(super) async fn approval_wait_candidates(
        &self,
        after: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after))
            .filter(run::Column::Status.eq("awaiting_permission"))
            .filter(run::Column::FailureAccounted.eq(false))
            .filter(run::Column::FinishedAt.is_null())
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    pub async fn expire_fresh_approval_wait(
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
        if task.kind != "fresh_task" || task.active_run_id.as_deref() != Some(run_id) {
            return Ok(false);
        }
        // Same task write fence as execution; cleanup intentionally does not
        // require current device availability or a still-valid execution grant.
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
        if work.status != "awaiting_permission"
            || work.failure_accounted
            || work.finished_at.is_some()
            || work.lease_deadline.is_some()
            || work.conversation_id != work.run_id
        {
            return Ok(false);
        }
        let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if snapshot.kind != "fresh_task"
            || snapshot.schedule_id != task.schedule_id
            || snapshot.owner_user_id != work.owner_user_id
            || snapshot.target_device_id != task.target_device_id
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let (_, contract) = super::publication::load_contract(
            &txn,
            work.owner_user_id,
            &work.schedule_id,
            snapshot
                .contract_revision
                .ok_or(ScheduleStoreError::Invalid)?,
        )
        .await?;
        let deadline = work
            .started_at
            .ok_or(ScheduleStoreError::Invalid)?
            .checked_add(i64::from(contract.contract().budget.max_runtime_seconds) * 1000)
            .ok_or(ScheduleStoreError::Invalid)?;
        let now = super::authority::authority_now(&txn).await?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(run_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session =
            desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
        if row.actor_id != work.owner_user_id.to_string()
            || row.device_id != snapshot.target_device_id
            || session.actor_id != row.actor_id
            || session.device_id != row.device_id
            || session.conversation_id != work.run_id
            || session.version != row.version
            || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
            || session.input_revision != 1
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let denied = desk_diagnose_core::schedule::permission_wait::rejected(
            &session,
            work.result_ref
                .as_deref()
                .ok_or(ScheduleStoreError::Invalid)?,
        );
        let directory_expired = desk_diagnose_core::schedule::permission_wait::directory_expired(
            &session,
            work.result_ref
                .as_deref()
                .ok_or(ScheduleStoreError::Invalid)?,
            u64::try_from(now).map_err(|_| ScheduleStoreError::Invalid)?,
        );
        let cancelled = work.cancel_requested_at.is_some();
        let policy = crate::schedule_budget_policy::read(&txn).await?;
        let budget_rejected =
            !desk_diagnose_core::schedule::policy::permits(&policy, &contract.contract().budget);
        if !cancelled && !denied && !directory_expired && !budget_rejected && now < deadline {
            return Ok(false);
        }
        // Approval entry required every original action to be complete. Recheck
        // under the task fence: never classify an outstanding effect as timeout.
        if work_item::Entity::find()
            .filter(work_item::Column::ConversationId.eq(run_id))
            .filter(
                work_item::Column::Status
                    .ne(crate::capability_grant_store::CAPABILITY_WORK_SUCCEEDED),
            )
            .lock_exclusive()
            .one(&txn)
            .await?
            .is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        let reference = work
            .result_ref
            .as_deref()
            .ok_or(ScheduleStoreError::Invalid)?;
        if let Some(request_id) = reference.strip_prefix("permission:") {
            crate::agent_session_store::permission_resume::close_fresh_wait_decision_on(
                &txn, &session, request_id, timestamp,
            )
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        if cancelled {
            desk_diagnose_core::schedule::permission_wait::cancel(
                &mut session,
                reference,
                &timestamp.to_rfc3339(),
            )
        } else {
            desk_diagnose_core::schedule::permission_wait::expire(
                &mut session,
                reference,
                &timestamp.to_rfc3339(),
            )
        }
        .ok_or(ScheduleStoreError::Conflict)?;
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(session
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                version: Set(session.version),
                lease_deadline: Set(None),
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
        let result_ref = work.result_ref.clone();
        super::settlement::settle(super::settlement::Settlement {
            txn,
            work,
            now,
            outcome: if cancelled {
                desk_agent_protocol::schedule::ScheduledRunStatus::Cancelled
            } else {
                desk_agent_protocol::schedule::ScheduledRunStatus::Failed
            },
            offline_timeout: false,
            error_kind: Some(
                if cancelled {
                    "cancelled"
                } else if budget_rejected {
                    "budget_policy_exceeded"
                } else if denied {
                    "approval_denied"
                } else if directory_expired {
                    "directory_expired"
                } else {
                    "approval_timeout"
                }
                .into(),
            ),
            result_ref,
        })
        .await?;
        Ok(true)
    }
}
