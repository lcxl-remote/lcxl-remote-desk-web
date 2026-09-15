//! Fence owner decisions and cancellation of the original conversation pause.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_action_item, agent_exec_task, agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Set};

#[cfg(test)]
mod tests;

async fn lock_wait(
    txn: &DatabaseTransaction,
    run_id: &str,
) -> Result<(run::Model, agent_session::Model, PersistedAgentSession), ScheduleStoreError> {
    let initial = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let locked = entity::Entity::update_many()
        .col_expr(
            entity::Column::Revision,
            sea_orm::sea_query::Expr::col(entity::Column::Revision),
        )
        .filter(entity::Column::ScheduleId.eq(&initial.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(initial.owner_user_id))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&initial.schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let work = run::Entity::find_by_id(initial.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&work.conversation_id))
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if task.kind != "conversation_resume"
        || task.calc_version != desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION
        || task.contract_revision.is_some()
        || task.authorization_revision.is_some()
        || task.active_run_id.as_deref() != Some(run_id)
        || task.source_conversation_id.as_deref() != Some(work.conversation_id.as_str())
        || task.requirement_revision != i64::try_from(session.input_revision).ok()
        || i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch)
        || work.status != "awaiting_permission"
        || work.failure_accounted
        || work.finished_at.is_some()
        || work.started_at.is_none()
        || work.lease_deadline.is_some()
        || work.lease_owner.is_none()
        || work.lease_epoch <= 0
        || work.attempt != 1
        || session.actor_id != work.owner_user_id.to_string()
        || row.actor_id != session.actor_id
        || session.device_id != task.target_device_id
        || row.device_id != session.device_id
        || session.conversation_id != work.conversation_id
        || session.surface != AgentSessionSurface::DeviceAssistant
        || session.trigger_origin != TriggerOrigin::ScheduledContinuation
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
        || row.lease_deadline.is_some()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    // Reuse the pure close validator on a copy; this grants no authority and
    // does not alter the persisted request while checking the quiescent pause.
    let mut checked = session.clone();
    desk_diagnose_core::schedule::permission_wait::cancel_continuation(
        &mut checked,
        work.result_ref
            .as_deref()
            .ok_or(ScheduleStoreError::Conflict)?,
        &session.updated_at,
    )
    .ok_or(ScheduleStoreError::Conflict)?;
    Ok((work, row, session))
}

pub(super) async fn lock_approval_on(
    txn: &DatabaseTransaction,
    initial: &PersistedAgentSession,
    request_id: &str,
) -> Result<(), ScheduleStoreError> {
    let (work, _, current) = lock_wait(
        txn,
        initial
            .current_request_id
            .as_deref()
            .ok_or(ScheduleStoreError::Conflict)?,
    )
    .await?;
    if current != *initial
        || work.cancel_requested_at.is_some()
        || desk_diagnose_core::schedule::permission_wait::reference(&current, request_id).as_deref()
            != work.result_ref.as_deref()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}

impl ScheduleStore {
    pub(super) async fn cancel_continuation_wait(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, entity::Entity).await?;
        let (work, row, mut session) = lock_wait(&txn, run_id).await?;
        if work.cancel_requested_at.is_none() {
            return Ok(false);
        }
        // Match continuation settlement's known terminal states. Historical
        // successful work is allowed; unresolved effects must not be hidden.
        let actions = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ConversationId.eq(&work.conversation_id))
            .lock_exclusive()
            .all(&txn)
            .await?;
        if actions.iter().any(|action| {
            !matches!(
                action.status.as_str(),
                "done"
                    | "rejected"
                    | "expired"
                    | "cancelled"
                    | "capability_succeeded"
                    | "capability_failed"
                    | "capability_superseded_before_intent"
                    | "capability_revoked_before_intent"
            )
        }) || agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ConversationId.eq(&work.conversation_id))
            .filter(agent_exec_task::Column::Status.ne("done"))
            .lock_exclusive()
            .one(&txn)
            .await?
            .is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(&txn).await?;
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        let reference = work
            .result_ref
            .as_deref()
            .ok_or(ScheduleStoreError::Conflict)?;
        if let Some(request_id) = reference.strip_prefix("permission:") {
            crate::agent_session_store::permission_resume::close_wait_decision_on(
                &txn, &session, request_id, timestamp,
            )
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        desk_diagnose_core::schedule::permission_wait::cancel_continuation(
            &mut session,
            reference,
            &timestamp.to_rfc3339(),
        )
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
            outcome: desk_agent_protocol::schedule::ScheduledRunStatus::Cancelled,
            offline_timeout: false,
            error_kind: Some("cancelled".into()),
            result_ref,
        })
        .await?;
        Ok(true)
    }
}
