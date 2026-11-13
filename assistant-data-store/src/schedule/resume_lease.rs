//! Renew a continuation's occurrence and session leases as one transaction.
use super::{
    ScheduleStoreError,
    authority::{authority_now, lock_run_authority},
    continuation_subject,
    session::{OrgMode, SessionGuard, SessionKey, SessionPatch, SessionRows, TouchColumn},
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_agent_protocol::schedule::SchedulePauseReason;
use desk_diagnose_core::{
    schedule::lifecycle::FailureState,
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, sea_query::Expr};

/// Server-held identity returned by the atomic continuation claim.
pub struct ContinuationLease<'a> {
    pub owner: i32,
    pub run_id: &'a str,
    pub node_id: &'a str,
    pub run_epoch: i64,
    pub session_token: u64,
}

/// Server-held identity for a fresh occurrence; never a conversation continuation.
pub struct FreshTaskLease<'a> {
    pub owner: i32,
    pub run_id: &'a str,
    pub node_id: &'a str,
    pub run_epoch: i64,
    pub session_token: u64,
}

/// Renews both leases in the caller's transaction. The caller commits only on
/// `Ok(true)`; any other result leaves the leases as they were.
pub async fn renew_paired_lease_on<C: ConnectionTrait, H: SessionRows<C>>(
    host: &H,
    txn: &C,
    lease: ContinuationLease<'_>,
    lease_seconds: u32,
    fresh: bool,
) -> Result<bool, ScheduleStoreError> {
    if lease.owner <= 0
        || lease.run_epoch <= 0
        || lease.session_token == 0
        || lease.session_token > i64::MAX as u64
        || lease.node_id.is_empty()
        || lease.node_id.len() > 256
        || !(30..=300).contains(&lease_seconds)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    let Some(work) = run::Entity::find()
        .filter(run::Column::RunId.eq(lease.run_id))
        .filter(run::Column::OwnerUserId.eq(lease.owner))
        .one(txn)
        .await?
    else {
        return Ok(false);
    };
    if work.status != "running"
        || work.lease_owner.as_deref() != Some(lease.node_id)
        || work.lease_epoch != lease.run_epoch
        || work.cancel_requested_at.is_some()
        || work.failure_accounted
    {
        return Ok(false);
    }
    let snapshot: entity::Model =
        serde_json::from_str(&work.task_snapshot_json).map_err(|_| ScheduleStoreError::Invalid)?;
    let kind = if fresh {
        "fresh_task"
    } else {
        "conversation_resume"
    };
    let input_revision = if fresh {
        Some(1)
    } else {
        snapshot.requirement_revision
    };
    if snapshot.kind != kind
        || snapshot.schedule_id != work.schedule_id
        || snapshot.owner_user_id != lease.owner
        || if fresh {
            work.conversation_id != work.run_id
                || work.turn_id != format!("{}-turn", work.run_id)
                || snapshot.source_conversation_id.is_some()
                || snapshot.requirement_revision.is_some()
        } else {
            snapshot.source_conversation_id.as_deref() != Some(work.conversation_id.as_str())
                || input_revision.is_none_or(|v| v <= 0)
        }
    {
        return Ok(false);
    }
    let Some(task) = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(lease.owner))
        .one(txn)
        .await?
    else {
        return Ok(false);
    };
    // Same task -> session -> occurrence order as claim and dispatch.
    let locked = entity::Entity::update_many()
        .col_expr(
            entity::Column::Revision,
            Expr::col(entity::Column::Revision),
        )
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::OwnerUserId.eq(lease.owner))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Ok(false);
    }
    let task = entity::Entity::find_by_id(task.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let failure: FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if !matches!(task.status.as_str(), "active" | "triggered" | "paused")
        || task.kind != kind
        || task.active_run_id.as_deref() != Some(lease.run_id)
        || task.target_device_id != snapshot.target_device_id
        || failure
            .pause_reasons
            .iter()
            .any(|reason| *reason != SchedulePauseReason::User)
        || i64::try_from(failure.recovery_epoch).ok() != Some(work.recovery_epoch)
    {
        return Ok(false);
    }
    if fresh {
        lock_run_authority(
            txn,
            lease.owner,
            &snapshot.target_device_id,
            lease.run_id,
            lease.node_id,
            lease.run_epoch,
        )
        .await?;
    }
    let subject = if fresh {
        snapshot.target_device_id.clone()
    } else {
        continuation_subject(&snapshot)?
    };
    let actor = lease.owner.to_string();
    let Some(row) = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &work.conversation_id,
                actor: Some(&actor),
                device: Some(&subject),
            },
            OrgMode::PersonalOnly,
            false,
        )
        .await?
    else {
        return Ok(false);
    };
    let locked = host
        .touch_session(
            txn,
            TouchColumn::LeaseToken,
            SessionGuard {
                id: Some(row.id),
                conversation: Some(&work.conversation_id),
                actor: Some(&actor),
                device: Some(&subject),
                version: None,
                lease_token: Some(lease.session_token as i64),
                state_json: None,
                lease_deadline_null: false,
                org: OrgMode::PersonalOnly,
            },
        )
        .await?;
    if locked != 1 {
        return Ok(false);
    }
    let row = host
        .find_session(txn, SessionKey::Id(row.id), OrgMode::Any, false)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if row.lease_token != lease.session_token as i64
        || session.lease_token != lease.session_token
        || session.version != row.version
        || session.conversation_id != work.conversation_id
        || session.check_subject(&actor, &subject).is_err()
        || session.surface != AgentSessionSurface::AiAssistant
        || session.trigger_origin
            != if fresh {
                TriggerOrigin::ScheduledTask
            } else {
                TriggerOrigin::ScheduledContinuation
            }
        || !session.turn_state.is_active()
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.current_request_id.as_deref() != Some(lease.run_id)
        || session.active_control_connection_id.is_some()
        || session.input_revision != input_revision.unwrap() as u64
    {
        return Ok(false);
    }
    let locked = run::Entity::update_many()
        .set(run::ActiveModel {
            lease_epoch: Set(work.lease_epoch),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::Status.eq("running"))
        .filter(run::Column::LeaseOwner.eq(lease.node_id))
        .filter(run::Column::LeaseEpoch.eq(lease.run_epoch))
        .filter(run::Column::CancelRequestedAt.is_null())
        .filter(run::Column::FailureAccounted.eq(false))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Ok(false);
    }
    let work = run::Entity::find_by_id(work.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let now = authority_now(txn).await?;
    let Some(session_deadline) = row.lease_deadline_ms else {
        return Ok(false);
    };
    let Some(run_deadline) = work.lease_deadline else {
        return Ok(false);
    };
    if session_deadline <= now || run_deadline <= now {
        return Ok(false);
    }
    let deadline = now
        .checked_add(i64::from(lease_seconds) * 1000)
        .ok_or(ScheduleStoreError::Invalid)?
        .max(session_deadline)
        .max(run_deadline);
    let changed = host
        .write_session(
            txn,
            SessionGuard {
                id: Some(row.id),
                version: Some(row.version),
                lease_token: Some(lease.session_token as i64),
                ..SessionGuard::any()
            },
            SessionPatch {
                lease_deadline_ms: Some(Some(deadline)),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            lease_deadline: Set(Some(deadline)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::LeaseEpoch.eq(lease.run_epoch))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    // The session/run locks may have waited. Recheck wall-clock parent expiry
    // after both updates; any error rolls back both deadlines.
    if fresh {
        lock_run_authority(
            txn,
            lease.owner,
            &snapshot.target_device_id,
            lease.run_id,
            lease.node_id,
            lease.run_epoch,
        )
        .await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::test_host::{Call, Recorder, continuation_rows, continuation_session};
    use desk_diagnose_core::session::TurnState;
    use sea_orm::Database;

    #[tokio::test]
    async fn renewal_extends_only_the_unchanged_session_version_and_token() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let far = chrono::Utc::now().timestamp_millis() + 600_000;
        continuation_rows(&db, "running", Some(far), false, None).await;
        let mut session = continuation_session();
        session.turn_state = TurnState::Running;
        let mut host = Recorder::new(session);
        host.lease_token = 7;
        host.lease_deadline_ms = Some(far);
        let lease = ContinuationLease {
            owner: 1,
            run_id: "run",
            node_id: "node",
            run_epoch: 1,
            session_token: 7,
        };
        assert!(
            renew_paired_lease_on(&host, &db, lease, 60, false)
                .await
                .unwrap()
        );
        let Some(Call::Write(guard, patch)) = host.calls().pop() else {
            panic!("renewal writes the session deadline last")
        };
        // A concurrent writer that changed the transcript or the token makes
        // this CAS miss instead of extending a lease it no longer owns.
        assert_eq!(
            (guard.id, guard.version, guard.lease_token),
            (Some(11), Some(4), Some(7))
        );
        assert_eq!(patch.lease_deadline_ms, Some(Some(far)));
    }
}
