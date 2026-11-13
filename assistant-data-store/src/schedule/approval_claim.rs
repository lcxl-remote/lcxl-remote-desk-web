//! Reclaim a fresh occurrence after its owner approved the waiting request.
use super::{
    ScheduleStoreError,
    approval_admission::lock_fresh_approval_on,
    authority::{authority_now, lock_run_authority},
    directory_receipt::verify_directory_receipt_on,
    resume_claim::FreshTaskClaim,
    session::{OrgMode, ScheduleHostStores, SessionGuard, SessionKey, SessionPatch},
};
use crate::entity::agent_schedule_run as run;
use desk_diagnose_core::session::{PersistedAgentSession, TurnState};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

/// Reclaims a fresh occurrence after the owner approved its waiting request.
/// The caller commits.
pub async fn claim_fresh_approval_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    input: FreshTaskClaim<'_>,
    request_id: &str,
) -> Result<PersistedAgentSession, ScheduleStoreError> {
    if input.owner <= 0 || input.node_id.is_empty() || !(30..=300).contains(&input.lease_seconds) {
        return Err(ScheduleStoreError::Invalid);
    }
    let review = lock_fresh_approval_on(host, txn, input.run_id, request_id)
        .await?
        .ok_or(ScheduleStoreError::Conflict)?;
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(input.run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: input.run_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if work.owner_user_id != input.owner
        || session.actor_id != input.owner.to_string()
        || session.policy_revision != input.policy_revision
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let now = authority_now(txn).await?;
    let reference = work
        .result_ref
        .as_deref()
        .ok_or(ScheduleStoreError::Conflict)?;
    if !desk_diagnose_core::schedule::permission_wait::approved(
        &session,
        reference,
        u64::try_from(now).map_err(|_| ScheduleStoreError::Invalid)?,
    ) {
        return Err(ScheduleStoreError::Conflict);
    }
    let until = now
        .checked_add(i64::from(input.lease_seconds) * 1000)
        .ok_or(ScheduleStoreError::Invalid)?
        .min(i64::try_from(review.valid_until).map_err(|_| ScheduleStoreError::Invalid)?);
    if until <= now {
        return Err(ScheduleStoreError::Conflict);
    }
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if reference.starts_with("directory:") {
        verify_directory_receipt_on(host, txn, &session, request_id).await?;
    } else {
        let grants = host.list_task_grants(txn, &session).await?;
        host.consume_scheduled_decision(txn, &session, request_id, &grants, now)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    // Preserve original turn counters, input, history, step evidence and
    // started_at. Approval is a continuation, not a new budget allocation.
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.lease_token = session
        .lease_token
        .checked_add(1)
        .filter(|token| *token <= i64::MAX as u64)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.turn_state = TurnState::Running;
    session.terminal_error = None;
    session.terminal_permission_request_id = None;
    session.scope_snapshot =
        desk_diagnose_core::session::narrow_scope(&session.turn_start_scope, &input.scope);
    session.updated_at = timestamp.to_rfc3339();
    let epoch = work
        .lease_epoch
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("running".into()),
            lease_owner: Set(Some(input.node_id.into())),
            lease_epoch: Set(epoch),
            lease_deadline: Set(Some(until)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::Status.eq("awaiting_permission"))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .filter(run::Column::CancelRequestedAt.is_null())
        .filter(run::Column::FailureAccounted.eq(false))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
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
                lease_token: Some(session.lease_token as i64),
                lease_deadline_ms: Some(Some(until)),
                updated_at_ms: Some(now),
                turn_state: Some(TurnState::Running.as_str()),
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    lock_run_authority(
        txn,
        input.owner,
        &session.device_id,
        input.run_id,
        input.node_id,
        epoch,
    )
    .await?;
    Ok(session)
}
