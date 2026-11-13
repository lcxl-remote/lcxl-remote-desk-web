//! Enable tasks: owner creation, AI proposal decisions and resumes. Enabling
//! schedules the calendar only and never grants a tool or device permission.
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

/// Who decided a pending AI proposal. Both decisions use the same path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalDecider {
    Owner,
    /// The delegated approval reviewer, with its reason shown to the model.
    Ai {
        reason: String,
    },
}

impl ScheduleStore {
    /// Owner creation enables the task at once; there is no draft state.
    pub async fn create_task(
        &self,
        owner: i32,
        request: &desk_agent_protocol::schedule::ScheduleDraft,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if request.creation_source != desk_agent_protocol::schedule::ScheduleCreationSource::Manual
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let now = desk_assistant_data_store::schedule::authority_now(&txn).await?;
        let task = desk_diagnose_core::future::send_phase(|| {
            Self::create_draft_on(&txn, owner, request, now, None)
        })
        .await?;
        let result = if task.status == "pending_review" {
            let enabled = Self::activate_on(&txn, owner, &task, false).await?;
            if enabled.kind == "conversation_resume" {
                record_decision_on(&txn, &enabled, None).await?;
            }
            enabled
        } else {
            task
        };
        txn.commit().await?;
        Ok(result)
    }

    /// Resume a paused task while its device is ready. A continuation also
    /// needs its original requirement and a future time.
    pub async fn resume_task(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        verifier: &dyn super::TaskSubjectVerifier,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let task = find_task(&txn, owner, schedule_id, expected_revision).await?;
        if task.status != "paused" {
            return Err(ScheduleStoreError::Conflict);
        }
        // Recheck the current account/device gate before acquiring the task
        // lock. This enables the calendar only, never restores consumed grants.
        verifier.lock_subject(&txn, &task).await?;
        let result = Self::activate_on(&txn, owner, &task, true).await?;
        if result.kind == "conversation_resume" {
            record_decision_on(&txn, &result, None).await?;
        }
        txn.commit().await?;
        Ok(result)
    }

    /// Decide a pending AI fresh-task proposal. Approval enables the task as
    /// proposed; rejection deletes it. The decision is written back to the
    /// waiting proposing turn. Neither grants a tool or device permission.
    pub async fn decide_proposal(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        approve: bool,
        decider: ProposalDecider,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = Self::decide_proposal_on(
            &txn,
            owner,
            schedule_id,
            expected_revision,
            approve,
            &decider,
        )
        .await?;
        txn.commit().await?;
        Ok(result)
    }

    /// `decide_proposal` inside the caller's transaction, for a decision that
    /// commits together with its review record.
    pub(crate) async fn decide_proposal_on(
        txn: &crate::config::connection::DatabaseTransaction,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        approve: bool,
        decider: &ProposalDecider,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let task = find_task(txn, owner, schedule_id, expected_revision).await?;
        if task.status != "pending_review" || task.proposal_conversation_id.is_none() {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = if approve {
            Self::activate_on(txn, owner, &task, false).await?
        } else {
            let now = desk_assistant_data_store::schedule::authority_now(txn).await?;
            let changed = entity::Entity::update_many()
                .set(entity::ActiveModel {
                    status: Set("deleted".into()),
                    revision: Set(task
                        .revision
                        .checked_add(1)
                        .ok_or(ScheduleStoreError::Invalid)?),
                    updated_at: Set(now),
                    ..Default::default()
                })
                .filter(entity::Column::Id.eq(task.id))
                .filter(entity::Column::Revision.eq(task.revision))
                .exec(txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
            entity::Entity::find_by_id(task.id)
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?
        };
        record_decision_on(txn, &result, Some(decider)).await?;
        Ok(result)
    }

    /// Enable a pending or paused task inside the caller's transaction. A
    /// continuation keeps its source requirement locked and must still fit
    /// the session retention window. No receipt is written here.
    pub(super) async fn activate_on(
        txn: &crate::config::connection::DatabaseTransaction,
        owner: i32,
        task: &entity::Model,
        resume: bool,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let continuation = task.kind == "conversation_resume";
        if owner <= 0
            || task.owner_user_id != owner
            || task.status != if resume { "paused" } else { "pending_review" }
            || task.active_run_id.is_some()
            || task.calc_version != SCHEDULE_CALC_VERSION
        {
            return Err(ScheduleStoreError::Conflict);
        }
        desk_assistant_data_store::schedule::retrieval_consent(task)?;
        let mut failures: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if resume {
            failures
                .resume()
                .map_err(|_| ScheduleStoreError::Conflict)?;
        } else if !failures.pause_reasons.is_empty() {
            return Err(ScheduleStoreError::Conflict);
        }
        // Match dispatch lock order: task first, then session.
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
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        if continuation {
            lock_original_requirement(txn, owner, task).await?;
        }
        // PostgreSQL CURRENT_TIMESTAMP is the transaction start time. Use the
        // actual database clock after the locks, so an elapsed deadline rejects.
        let now = desk_assistant_data_store::schedule::authority_now(txn).await?;
        let spec = parse_json(&task.spec_json).map_err(|_| ScheduleStoreError::Invalid)?;
        let spec = validate_publication(&spec, now, continuation).map_err(|_| {
            ScheduleStoreError::ToolInput(
                desk_diagnose_core::schedule::proposal::NO_FUTURE_OCCURRENCE,
            )
        })?;
        if continuation {
            super::check_resume_retention(txn, &spec, now).await?;
        }
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
        entity::Entity::find_by_id(task.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)
    }
}

impl ScheduleStore {
    /// Test helper: enable a pending task as owner creation does, with the
    /// continuation's activation receipt.
    #[cfg(test)]
    pub(crate) async fn activate_pending_for_test(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let task = find_task(&txn, owner, schedule_id, expected_revision).await?;
        let result = Self::activate_on(&txn, owner, &task, false).await?;
        if result.kind == "conversation_resume" {
            record_decision_on(&txn, &result, None).await?;
        }
        txn.commit().await?;
        Ok(result)
    }
}

async fn find_task(
    txn: &crate::config::connection::DatabaseTransaction,
    owner: i32,
    schedule_id: &str,
    expected_revision: i64,
) -> Result<entity::Model, ScheduleStoreError> {
    let task = entity::Entity::find()
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if task.revision != expected_revision {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(task)
}

#[cfg(test)]
mod tests;

/// Hold the unchanged source input while editing or enabling its continuation.
pub(super) async fn lock_original_requirement(
    txn: &crate::config::connection::DatabaseTransaction,
    owner: i32,
    task: &entity::Model,
) -> Result<(), ScheduleStoreError> {
    let subject = desk_assistant_data_store::schedule::continuation_subject(task)?;
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
        .filter(session_row::Column::DeviceId.eq(&subject))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.conversation_id != source
        || session.check_subject(&actor, &subject).is_err()
        || session
            .check_surface(AgentSessionSurface::AiAssistant)
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
        .filter(session_row::Column::DeviceId.eq(&subject))
        .filter(session_row::Column::Version.eq(row.version))
        .filter(session_row::Column::StateJson.eq(&row.state_json))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}

/// Persist the authoritative decision or activation receipt without creating a
/// new user input. A proposal decision goes to its waiting proposing turn; a
/// continuation's activation goes to its source conversation.
pub(super) async fn record_decision_on(
    txn: &crate::config::connection::DatabaseTransaction,
    task: &entity::Model,
    decider: Option<&ProposalDecider>,
) -> Result<(), ScheduleStoreError> {
    let now = desk_assistant_data_store::schedule::authority_now(txn).await?;
    let rejected = task.status == "deleted";
    let conversation = match decider {
        Some(_) => task.proposal_conversation_id.as_deref(),
        None => task.source_conversation_id.as_deref(),
    }
    .ok_or(ScheduleStoreError::Invalid)?;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(conversation))
        .one(txn)
        .await?;
    let Some(row) = row else {
        // Deleting an orphan proposal must remain possible. The deleted task
        // row is the durable decision even when its transcript no longer exists.
        return if rejected {
            Ok(())
        } else {
            Err(ScheduleStoreError::NotFound)
        };
    };
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    // Only the matching waiting proposal may write a receipt into a live turn.
    let waiting = session.pending_schedule_review.as_deref() == Some(task.schedule_id.as_str())
        && session.owner_direct_planning()
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
    let id = format!("schedule-decision:{}:{}", task.schedule_id, task.revision);
    let text = serde_json::json!({
        "event": if rejected { "scheduled_task_rejected" } else { "scheduled_task_activated" },
        "schedule_id": task.schedule_id, "revision": task.revision, "state": task.status,
        "next_run_at_utc_ms": task.next_run_at,
        "authority": "calendar_only_no_tool_permissions",
        "decided_by": match decider {
            Some(ProposalDecider::Ai { .. }) => "approval_ai",
            Some(ProposalDecider::Owner) | None => "owner",
        },
        "reason": match decider { Some(ProposalDecider::Ai { reason }) => Some(reason), _ => None },
        "instruction": "This server receipt is the final decision. Do not request another chat confirmation. Ordinary chat does not cancel an enabled task. Every run approves its device actions separately."})
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
