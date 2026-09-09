//! Atomic occurrence/session claim for an explicitly confirmed continuation.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_schedule_run as run, agent_session as session_row};
use desk_agent_protocol::AgentScope;
use desk_diagnose_core::{
    schedule::{SCHEDULE_CALC_VERSION, lifecycle::FailureState},
    session::{AgentSessionSurface, ExecutionState, PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

/// Server-resolved personal subject policy. Never deserialize this from a client
/// or infer it from the stored task. Device/session policy is rechecked per call.
pub struct ContinuationClaim<'a> {
    pub owner: i32,
    pub run_id: &'a str,
    pub node_id: &'a str,
    pub lease_seconds: u32,
    pub policy_revision: i64,
    pub scope: AgentScope,
}

/// A coherent server-side approval projection; never client-supplied authority.
pub struct ContinuationPermissionClaim<'a> {
    pub continuation: ContinuationClaim<'a>,
    pub request_id: &'a str,
    pub expected_session_version: i64,
    pub expected_run_epoch: i64,
    pub grants: &'a [desk_agent_protocol::capability_grant::CapabilityGrant],
}

#[derive(Clone, Copy)]
struct PermissionReceiptClaim<'a> {
    request_id: &'a str,
    version: i64,
    epoch: i64,
    grants: &'a [desk_agent_protocol::capability_grant::CapabilityGrant],
}

pub struct ClaimedContinuation {
    pub run: run::Model,
    pub session: PersistedAgentSession,
}

impl ScheduleStore {
    pub async fn claim_conversation_resume(
        &self,
        input: ContinuationClaim<'_>,
    ) -> Result<ClaimedContinuation, ScheduleStoreError> {
        self.claim_continuation_on(input, None).await
    }

    pub async fn claim_continuation_permission(
        &self,
        input: ContinuationPermissionClaim<'_>,
    ) -> Result<ClaimedContinuation, ScheduleStoreError> {
        let ContinuationPermissionClaim {
            continuation,
            request_id,
            expected_session_version,
            expected_run_epoch,
            grants,
        } = input;
        self.claim_continuation_on(
            continuation,
            Some(PermissionReceiptClaim {
                request_id,
                version: expected_session_version,
                epoch: expected_run_epoch,
                grants,
            }),
        )
        .await
    }

    async fn claim_continuation_on(
        &self,
        input: ContinuationClaim<'_>,
        permission: Option<PermissionReceiptClaim<'_>>,
    ) -> Result<ClaimedContinuation, ScheduleStoreError> {
        if input.owner <= 0
            || input.node_id.is_empty()
            || input.node_id.len() > 256
            || !(30..=300).contains(&input.lease_seconds)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(input.run_id))
            .filter(run::Column::OwnerUserId.eq(input.owner))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.cancel_requested_at.is_some() || work.failure_accounted {
            return Err(ScheduleStoreError::Conflict);
        }
        if let Some(PermissionReceiptClaim {
            request_id,
            version,
            epoch,
            ..
        }) = permission
        {
            if version < 1
                || epoch < 1
                || epoch != work.lease_epoch
                || work.status != "awaiting_permission"
                || work.lease_deadline.is_some()
                || work.started_at.is_none()
                || work.finished_at.is_some()
                || work.attempt != 1
                || work.result_ref.as_deref() != Some(format!("permission:{request_id}").as_str())
            {
                return Err(ScheduleStoreError::Conflict);
            }
        } else if !matches!(work.status.as_str(), "queued" | "waiting_device")
            || work.lease_epoch != 0
            || work.attempt != 0
            || work.started_at.is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(input.owner))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let failure: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let source = task
            .source_conversation_id
            .as_deref()
            .ok_or(ScheduleStoreError::Invalid)?;
        if task.kind != "conversation_resume"
            || !(matches!(task.status.as_str(), "active" | "triggered")
                || (permission.is_some() && task.status == "paused"))
            || task.active_run_id.as_deref() != Some(input.run_id)
            || failure.pause_reasons.iter().any(|reason| {
                permission.is_none()
                    || *reason != desk_agent_protocol::schedule::SchedulePauseReason::User
            })
            || i64::try_from(failure.recovery_epoch).ok() != Some(work.recovery_epoch)
            || task.calc_version != SCHEDULE_CALC_VERSION
            || task.requirement_revision.is_none_or(|v| v <= 0)
            || task.contract_revision.is_some()
            || task.authorization_revision.is_some()
            || work.conversation_id != source
            || work.turn_id.is_empty()
            || snapshot.schedule_id != task.schedule_id
            || snapshot.owner_user_id != input.owner
            || snapshot.kind != task.kind
            || snapshot.target_device_id != task.target_device_id
            || snapshot.task_revision != task.task_revision
            || snapshot.prompt != task.prompt
            || snapshot.spec_json != task.spec_json
            || snapshot.requirement_revision != task.requirement_revision
            || snapshot.source_conversation_id != task.source_conversation_id
        {
            return Err(ScheduleStoreError::Conflict);
        }
        // All task operations lock task before session and occurrence rows.
        let revision = task
            .revision
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let locked = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(revision),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let actor = input.owner.to_string();
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(source))
            .filter(session_row::Column::ActorId.eq(&actor))
            .filter(session_row::Column::DeviceId.eq(&task.target_device_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if session.conversation_id != source
            || session
                .check_subject(&actor, &task.target_device_id)
                .is_err()
            || session
                .check_surface(AgentSessionSurface::DeviceAssistant)
                .is_err()
        {
            return Err(ScheduleStoreError::NotFound);
        }
        if session.input_revision != task.requirement_revision.unwrap() as u64
            || !session.turn_state.can_claim()
            || session.latest_input_seq != session.handled_input_seq
            || session.execution_state != ExecutionState::None
            || session.version != row.version
            || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
            || row.lease_token < 0
            || row.lease_token == i64::MAX
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if let Some(PermissionReceiptClaim {
            request_id,
            version,
            ..
        }) = permission
            && (desk_diagnose_core::assistant_policy::require_current_policy(input.policy_revision)
                .is_err()
                || session.version != version
                || session.trigger_origin != TriggerOrigin::ScheduledContinuation
                || session.current_request_id.as_deref() != Some(work.run_id.as_str())
                || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
                || session.active_control_connection_id.is_some()
                || session.turn_state != desk_diagnose_core::session::TurnState::Idle
                || session.terminal_error.is_some()
                || !session.pending_auto_triggers.is_empty()
                || desk_diagnose_core::schedule::permission_wait::reference(&session, request_id)
                    != work.result_ref)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let locked = session_row::Entity::update_many()
            .set(session_row::ActiveModel {
                version: Set(row.version),
                ..Default::default()
            })
            .filter(session_row::Column::Id.eq(row.id))
            .filter(session_row::Column::Version.eq(row.version))
            .filter(session_row::Column::StateJson.eq(&row.state_json))
            .filter(session_row::Column::LeaseToken.eq(row.lease_token))
            .filter(session_row::Column::ActorId.eq(&actor))
            .filter(session_row::Column::DeviceId.eq(&task.target_device_id))
            .filter(session_row::Column::ConversationId.eq(source))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(&txn).await?;
        if permission.is_none() && now >= work.start_deadline {
            return Err(ScheduleStoreError::Conflict);
        }
        let deadline = now
            .checked_add(i64::from(input.lease_seconds) * 1000)
            .ok_or(ScheduleStoreError::Invalid)?;
        let now_dt =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        let deadline_dt =
            chrono::DateTime::from_timestamp_millis(deadline).ok_or(ScheduleStoreError::Invalid)?;
        let turn_id = if let Some(PermissionReceiptClaim {
            request_id, grants, ..
        }) = permission
        {
            crate::agent_session_store::permission_resume::consume_scheduled_decision_on(
                &txn, &session, request_id, grants, now_dt,
            )
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
            crate::agent_session_store::permission_resume::turn_id(source, request_id)
        } else {
            work.turn_id.clone()
        };
        session
            .begin_turn(
                &turn_id,
                Some(work.run_id.clone()),
                None,
                input.policy_revision,
                input.scope,
                now_dt.to_rfc3339(),
            )
            .map_err(|_| ScheduleStoreError::Conflict)?;
        session.adopt_trigger(TriggerOrigin::ScheduledContinuation, &turn_id);
        session.version = row
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let changed = session_row::Entity::update_many()
            .set(session_row::ActiveModel {
                state_json: Set(session
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                version: Set(session.version),
                lease_token: Set(session.lease_token as i64),
                lease_deadline: Set(Some(deadline_dt)),
                updated_at: Set(now_dt),
                ..Default::default()
            })
            .filter(session_row::Column::Id.eq(row.id))
            .filter(session_row::Column::Version.eq(row.version))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let epoch = work
            .lease_epoch
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("running".into()),
                lease_owner: Set(Some(input.node_id.into())),
                lease_epoch: Set(epoch),
                lease_deadline: Set(Some(deadline)),
                attempt: Set(if permission.is_some() {
                    work.attempt
                } else {
                    work.attempt
                        .checked_add(1)
                        .ok_or(ScheduleStoreError::Invalid)?
                }),
                turn_id: Set(turn_id),
                result_ref: Set(None),
                started_at: Set(work.started_at.or(Some(now))),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::Status.eq(&work.status))
            .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
            .filter(run::Column::CancelRequestedAt.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        entity::Entity::update_many()
            .set(entity::ActiveModel {
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .exec(&txn)
            .await?;
        let run = run::Entity::find_by_id(work.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(ClaimedContinuation { run, session })
    }
}

#[cfg(test)]
pub(super) mod tests;
