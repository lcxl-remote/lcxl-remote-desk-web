//! Fence owner decisions and cancellation of the original conversation pause.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    continuation_subject,
    session::{OrgMode, ScheduleHostStores, SessionGuard, SessionKey, SessionPatch, SessionRow},
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

async fn lock_wait<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    run_id: &str,
) -> Result<(run::Model, SessionRow<H::Row>, PersistedAgentSession), ScheduleStoreError> {
    let initial = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    host.lock_conversation(txn, &initial.conversation_id)
        .await?;
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
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &work.conversation_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            true,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if task.kind != "conversation_resume"
        || task.calc_version != desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION
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
        || session.device_id
            != continuation_subject(&task).map_err(|_| ScheduleStoreError::Invalid)?
        || row.device_id != session.device_id
        || session.conversation_id != work.conversation_id
        || session.surface != AgentSessionSurface::AiAssistant
        || session.trigger_origin != TriggerOrigin::ScheduledContinuation
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
        || row.lease_deadline_ms.is_some()
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

pub async fn lock_approval_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    initial: &PersistedAgentSession,
    request_id: &str,
) -> Result<(), ScheduleStoreError> {
    let (work, _, current) = lock_wait(
        host,
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

/// Closes a cancelled continuation's quiescent permission wait in the caller's
/// transaction. Returns the occurrence and the database time for the host to
/// settle as cancelled, or `None` when no cancellation was requested.
pub async fn cancel_continuation_wait_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    run_id: &str,
) -> Result<Option<(run::Model, i64)>, ScheduleStoreError> {
    let (work, row, mut session) = lock_wait(host, txn, run_id).await?;
    if work.cancel_requested_at.is_none() {
        return Ok(None);
    }
    // Match continuation settlement's known terminal states. Historical
    // successful work is allowed; unresolved effects must not be hidden.
    if host.has_open_actions(txn, &work.conversation_id).await? {
        return Err(ScheduleStoreError::Conflict);
    }
    let now = authority_now(txn).await?;
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    let reference = work
        .result_ref
        .as_deref()
        .ok_or(ScheduleStoreError::Conflict)?;
    if let Some(request_id) = reference.strip_prefix("permission:") {
        host.close_wait_decision(txn, &session, request_id, now)
            .await?;
    }
    desk_diagnose_core::schedule::permission_wait::cancel_continuation(
        &mut session,
        reference,
        &timestamp.to_rfc3339(),
    )
    .ok_or(ScheduleStoreError::Conflict)?;
    desk_diagnose_core::ai_assistant::scheduled_retrieval::restore_owner_retrieval(&mut session)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let state_json = session
        .encode_json_for_storage()
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let changed = host
        .write_session(
            txn,
            SessionGuard {
                id: Some(row.id),
                version: Some(row.version),
                lease_token: Some(row.lease_token),
                ..SessionGuard::any()
            },
            SessionPatch {
                state_json: Some(&state_json),
                version: Some(session.version),
                lease_deadline_ms: Some(None),
                updated_at_ms: Some(now),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(Some((work, now)))
}
