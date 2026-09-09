//! Renew a continuation's occurrence and session leases as one transaction.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_schedule_run as run, agent_session as session_row};
use desk_agent_protocol::schedule::SchedulePauseReason;
use desk_diagnose_core::{
    schedule::lifecycle::FailureState,
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::Expr};

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

impl ScheduleStore {
    /// Renew both leases only while the published parent authority remains current.
    /// This does not authorize model or tool calls; dispatch must check authority again.
    pub async fn renew_fresh_task(
        &self,
        lease: FreshTaskLease<'_>,
        lease_seconds: u32,
    ) -> Result<bool, ScheduleStoreError> {
        self.renew_paired_lease(
            ContinuationLease {
                owner: lease.owner,
                run_id: lease.run_id,
                node_id: lease.node_id,
                run_epoch: lease.run_epoch,
                session_token: lease.session_token,
            },
            lease_seconds,
            true,
        )
        .await
    }
    /// `false` stops the execution heartbeat. Never revive either expired lease.
    /// No session JSON or version is changed, so normal step saves can keep using
    /// their held CAS version. A user pause affects future runs, not this live run.
    pub async fn renew_conversation_resume(
        &self,
        lease: ContinuationLease<'_>,
        lease_seconds: u32,
    ) -> Result<bool, ScheduleStoreError> {
        self.renew_paired_lease(lease, lease_seconds, false).await
    }

    pub(super) async fn renew_paired_lease(
        &self,
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
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        // Acquire SQLite's write reservation before establishing a read snapshot.
        // Concurrent timer and pre-call renewals must wait for each other rather
        // than fail when a deferred read transaction upgrades to a writer in WAL.
        let reserved = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::OwnerUserId.eq(lease.owner))
            .filter(entity::Column::ActiveRunId.eq(lease.run_id))
            .exec(&txn)
            .await?;
        if reserved.rows_affected != 1 {
            return Ok(false);
        }
        let Some(work) = run::Entity::find()
            .filter(run::Column::RunId.eq(lease.run_id))
            .filter(run::Column::OwnerUserId.eq(lease.owner))
            .one(&txn)
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
        let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
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
            .one(&txn)
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
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Ok(false);
        }
        let task = entity::Entity::find_by_id(task.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let failure: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
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
            Self::lock_run_authority(
                &txn,
                lease.owner,
                &snapshot.target_device_id,
                lease.run_id,
                lease.node_id,
                lease.run_epoch,
            )
            .await?;
        }
        let actor = lease.owner.to_string();
        let Some(row) = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&work.conversation_id))
            .filter(session_row::Column::ActorId.eq(&actor))
            .filter(session_row::Column::DeviceId.eq(&snapshot.target_device_id))
            .one(&txn)
            .await?
        else {
            return Ok(false);
        };
        let locked = session_row::Entity::update_many()
            .set(session_row::ActiveModel {
                lease_token: Set(lease.session_token as i64),
                ..Default::default()
            })
            .filter(session_row::Column::Id.eq(row.id))
            .filter(session_row::Column::LeaseToken.eq(lease.session_token as i64))
            .filter(session_row::Column::ActorId.eq(&actor))
            .filter(session_row::Column::DeviceId.eq(&snapshot.target_device_id))
            .filter(session_row::Column::ConversationId.eq(&work.conversation_id))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Ok(false);
        }
        let row = session_row::Entity::find_by_id(row.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if row.lease_token != lease.session_token as i64
            || session.lease_token != lease.session_token
            || session.version != row.version
            || session.conversation_id != work.conversation_id
            || session
                .check_subject(&actor, &snapshot.target_device_id)
                .is_err()
            || session.surface != AgentSessionSurface::DeviceAssistant
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
            || session.execution_state.has_unresolved_outcome()
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
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Ok(false);
        }
        let work = run::Entity::find_by_id(work.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let now = super::authority::authority_now(&txn).await?;
        let Some(session_deadline) = row.lease_deadline.map(|d| d.timestamp_millis()) else {
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
        let deadline_dt =
            chrono::DateTime::from_timestamp_millis(deadline).ok_or(ScheduleStoreError::Invalid)?;
        let changed = session_row::Entity::update_many()
            .set(session_row::ActiveModel {
                lease_deadline: Set(Some(deadline_dt)),
                ..Default::default()
            })
            .filter(session_row::Column::Id.eq(row.id))
            .filter(session_row::Column::Version.eq(row.version))
            .filter(session_row::Column::LeaseToken.eq(lease.session_token as i64))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
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
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        // The session/run locks may have waited. Recheck wall-clock parent expiry
        // after both updates; any error rolls back both deadlines.
        if fresh {
            Self::lock_run_authority(
                &txn,
                lease.owner,
                &snapshot.target_device_id,
                lease.run_id,
                lease.node_id,
                lease.run_epoch,
            )
            .await?;
        }
        txn.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
