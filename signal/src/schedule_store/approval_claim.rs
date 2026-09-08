//! Renew both leases for one approved occurrence without resetting its context or budget.
use super::{FreshTaskClaim, ScheduleStore, ScheduleStoreError};
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{PersistedAgentSession, TurnState};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set};

impl ScheduleStore {
    pub(super) async fn claim_fresh_approval_on(
        txn: &DatabaseTransaction,
        input: FreshTaskClaim<'_>,
        request_id: &str,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        if input.owner <= 0
            || input.node_id.is_empty()
            || !(30..=300).contains(&input.lease_seconds)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let review = super::lock_fresh_approval_on(txn, input.run_id, request_id)
            .await?
            .ok_or(ScheduleStoreError::Conflict)?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(input.run_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(input.run_id))
            .one(txn)
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
        let now = super::authority::authority_now(txn).await?;
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
        let deadline =
            chrono::DateTime::from_timestamp_millis(until).ok_or(ScheduleStoreError::Invalid)?;
        if reference.starts_with("directory:") {
            super::directory_receipt::verify(txn, &session, request_id).await?;
        } else {
            let grants =
                crate::capability_grant_store::SignalCapabilityGrantStore::list_for_subject_on(
                    txn,
                    &session.conversation_id,
                    &session.actor_id,
                    &session.device_id,
                )
                .await?;
            crate::agent_session_store::permission_resume::consume_scheduled_decision_on(
                txn, &session, request_id, &grants, timestamp,
            )
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
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(session
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                version: Set(session.version),
                lease_token: Set(session.lease_token as i64),
                lease_deadline: Set(Some(deadline)),
                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        Self::lock_run_authority(
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
}
