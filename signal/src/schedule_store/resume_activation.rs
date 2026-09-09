//! Confirm a one-shot continuation against the current persisted input.
use super::{ScheduleStore, ScheduleStoreError, entity, json};
use crate::entity::agent_session as session_row;
use desk_diagnose_core::{
    schedule::{
        SCHEDULE_CALC_VERSION, lifecycle::FailureState, next_after, parse_json,
        validate_publication,
    },
    session::{AgentSessionSurface, ExecutionState, PersistedAgentSession},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

impl ScheduleStore {
    /// Manual creation is one atomic operation: no reviewable draft is exposed.
    pub async fn create_conversation_task(
        &self,
        owner: i32,
        request: &desk_agent_protocol::schedule::ScheduleDraft,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if request.kind != desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume
            || request.creation_source
                != desk_agent_protocol::schedule::ScheduleCreationSource::Manual
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let now = super::authority::authority_now(&txn).await?;
        let task = Self::create_draft_on(&txn, owner, request, now).await?;
        let result = if task.status == "pending_review" {
            self.enable_conversation_resume_on(
                &txn,
                owner,
                &task.schedule_id,
                task.revision,
                false,
                None,
            )
            .await?
        } else {
            task
        };
        txn.commit().await?;
        Ok(result)
    }

    /// Called for an explicit owner confirmation, never merely an AI proposal.
    /// This enables the calendar only; it grants no tool or device permissions.
    /// The executor must recheck this input binding under its session claim.
    pub async fn activate_conversation_resume(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        self.enable_conversation_resume(owner, schedule_id, expected_revision, false, None)
            .await
    }

    /// Resume a paused one-shot only while its original requirement and future time remain valid.
    pub async fn resume_conversation_task(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        verifier: &dyn super::TaskPublicationVerifier,
    ) -> Result<entity::Model, ScheduleStoreError> {
        self.enable_conversation_resume(owner, schedule_id, expected_revision, true, Some(verifier))
            .await
    }

    async fn enable_conversation_resume(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        resume: bool,
        verifier: Option<&dyn super::TaskPublicationVerifier>,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = self
            .enable_conversation_resume_on(
                &txn,
                owner,
                schedule_id,
                expected_revision,
                resume,
                verifier,
            )
            .await?;
        txn.commit().await?;
        Ok(result)
    }

    async fn enable_conversation_resume_on(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        resume: bool,
        verifier: Option<&dyn super::TaskPublicationVerifier>,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if owner <= 0
            || task.revision != expected_revision
            || task.kind != "conversation_resume"
            || task.status != if resume { "paused" } else { "pending_review" }
            || task.active_run_id.is_some()
            || task.contract_revision.is_some()
            || task.authorization_revision.is_some()
            || task.calc_version != SCHEDULE_CALC_VERSION
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let mut failures: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if resume {
            // Recheck the current account/device gate before acquiring the task
            // lock. This enables the calendar only, never restores consumed grants.
            verifier
                .ok_or(ScheduleStoreError::Conflict)?
                .lock_subject(txn, &task)
                .await?;
            failures
                .pause_reasons
                .remove(&desk_agent_protocol::schedule::SchedulePauseReason::AuthorizationInvalid);
            failures
                .resume()
                .map_err(|_| ScheduleStoreError::Conflict)?;
        } else if !failures.pause_reasons.is_empty() {
            return Err(ScheduleStoreError::Conflict);
        }
        // Match dispatch/revocation lock order: task first, then session.
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
            .filter(entity::Column::Revision.eq(expected_revision))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        lock_original_requirement(txn, owner, &task).await?;
        // PostgreSQL CURRENT_TIMESTAMP is the transaction start time. Use the
        // actual database clock after both locks, so an elapsed deadline rejects.
        let now = super::authority::authority_now(txn).await?;
        let spec = parse_json(&task.spec_json).map_err(|_| ScheduleStoreError::Invalid)?;
        let spec =
            validate_publication(&spec, now, true).map_err(|_| ScheduleStoreError::Invalid)?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                status: Set("active".into()),
                failure_state_json: Set(json(&failures)?),
                spec_json: Set(json(&spec)?),
                next_run_at: Set(next_after(&spec, now).map_err(|_| ScheduleStoreError::Invalid)?),
                recurrence_cursor_at: Set(Some(now)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(revision))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = entity::Entity::find_by_id(task.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        record_decision_on(txn, &result, now).await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;

/// Hold the unchanged source input while editing or enabling its continuation.
pub(super) async fn lock_original_requirement(
    txn: &sea_orm::DatabaseTransaction,
    owner: i32,
    task: &entity::Model,
) -> Result<(), ScheduleStoreError> {
    let source = task
        .source_conversation_id
        .as_deref()
        .ok_or(ScheduleStoreError::Invalid)?;
    let required = task
        .requirement_revision
        .filter(|revision| *revision > 0)
        .ok_or(ScheduleStoreError::Invalid)?;
    let actor = owner.to_string();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(source))
        .filter(session_row::Column::ActorId.eq(&actor))
        .filter(session_row::Column::DeviceId.eq(&task.target_device_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
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
    if session.input_revision != required as u64
        || matches!(
            session.execution_state,
            ExecutionState::OutcomeUnknown { .. } | ExecutionState::Interrupted { .. }
        )
    {
        return Err(ScheduleStoreError::Conflict);
    }
    // Lock the exact source envelope without advancing its input or version.
    // All input writers update this row. A racing write either wins this CAS
    // or waits until activation commits. Later chat does not cancel approval.
    let locked = session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            version: Set(row.version),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .filter(session_row::Column::ConversationId.eq(source))
        .filter(session_row::Column::ActorId.eq(&actor))
        .filter(session_row::Column::DeviceId.eq(&task.target_device_id))
        .filter(session_row::Column::Version.eq(row.version))
        .filter(session_row::Column::StateJson.eq(&row.state_json))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}

/// Persist the authoritative activation receipt without creating a new user input.
pub(super) async fn record_decision_on(
    txn: &sea_orm::DatabaseTransaction,
    task: &entity::Model,
    now: i64,
) -> Result<(), ScheduleStoreError> {
    let rejected = task.status == "deleted";
    let source = task
        .source_conversation_id
        .as_deref()
        .ok_or(ScheduleStoreError::Invalid)?;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(source))
        .one(txn)
        .await?;
    let Some(row) = row else {
        // Deleting an orphan draft must remain possible. The deleted task row
        // is the durable decision even when its transcript no longer exists.
        return if rejected {
            Ok(())
        } else {
            Err(ScheduleStoreError::NotFound)
        };
    };
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    // Only a matching schedule review may write a receipt into a live turn.
    let waiting = session.pending_schedule_review.as_deref() == Some(task.schedule_id.as_str())
        && session.trigger_origin == desk_diagnose_core::session::TriggerOrigin::User
        && row
            .lease_deadline
            .is_some_and(|deadline| deadline.timestamp_millis() > now);
    if session.turn_state.is_active() && !waiting {
        // Do not steal an interactive turn just to backfill a deletion notice.
        return if rejected {
            Ok(())
        } else {
            Err(ScheduleStoreError::Conflict)
        };
    }
    let id = format!("schedule-activation:{}:{}", task.schedule_id, task.revision);
    let text =
        serde_json::json!({"event":if rejected { "scheduled_task_rejected" } else { "scheduled_task_activated" },"schedule_id":task.schedule_id,
        "revision":task.revision,"state":task.status,"next_run_at_utc_ms":task.next_run_at,
        "authority":"calendar_only_no_tool_permissions",
        "owner_decision":if rejected { "rejected" } else { "approved" },
        "instruction":"This server receipt supersedes the earlier draft tool result. Do not request another chat confirmation. Ordinary chat does not cancel an approved task."})
        .to_string();
    let parent = session
        .conversation
        .iter()
        .rev()
        .find_map(|message| message.data_envelope.as_ref());
    let mut message = desk_diagnose_core::chat::ChatMessage::system_event(&id, &text);
    message.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            parent,
            &id,
            &text,
            "schedule_activation",
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
    session.conversation.push(message);
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    session.updated_at = timestamp.to_rfc3339();
    let changed = session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(session
                .encode_json_for_storage()
                .map_err(|_| ScheduleStoreError::Invalid)?),
            version: Set(session.version),
            updated_at: Set(timestamp),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .filter(session_row::Column::Version.eq(row.version))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}
