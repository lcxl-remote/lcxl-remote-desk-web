//! One check of a waiting schedule-proposal turn for the owner's decision.
use super::{
    ScheduleStoreError,
    queue::database_now,
    session::{OrgMode, SessionKey, SessionRows},
};
use desk_diagnose_core::{
    model_observability::{PermissionOutcome, tool::ToolObservation},
    session::PersistedAgentSession,
};
use sea_orm::ConnectionTrait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPoll {
    /// The decision receipt was adopted into `session`.
    Decided,
    /// Nothing changed yet; the host waits before asking again.
    Unchanged,
}

/// Reads without a transaction. An expired lease or any other change than the
/// exact decision receipt is a conflict.
pub async fn poll_review_decision_on<C: ConnectionTrait, H: SessionRows<C>>(
    host: &H,
    db: &C,
    session: &mut PersistedAgentSession,
    schedule_id: &str,
    observation: &ToolObservation,
) -> Result<ReviewPoll, ScheduleStoreError> {
    if session.pending_schedule_review.as_deref() != Some(schedule_id)
        || !session.turn_state.is_active()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let row = host
        .find_session(
            db,
            SessionKey::Conversation {
                conversation: &session.conversation_id,
                actor: Some(&session.actor_id),
                device: Some(&session.device_id),
            },
            OrgMode::Any,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if row.lease_token
        != i64::try_from(session.lease_token).map_err(|_| ScheduleStoreError::Invalid)?
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let now = database_now(db).await?;
    if row.lease_deadline_ms.is_none_or(|deadline| deadline <= now) {
        return Err(ScheduleStoreError::Conflict);
    }
    let current = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if current.version != row.version {
        return Err(ScheduleStoreError::Conflict);
    }
    if &current == session {
        return Ok(ReviewPoll::Unchanged);
    }
    let event = current
        .conversation
        .last()
        .ok_or(ScheduleStoreError::Conflict)?;
    let decision: serde_json::Value =
        serde_json::from_str(&event.text).map_err(|_| ScheduleStoreError::Conflict)?;
    if event.role != desk_diagnose_core::chat::ChatRole::SystemEvent
        || decision["schedule_id"].as_str() != Some(schedule_id)
        || !matches!(
            decision["event"].as_str(),
            Some("scheduled_task_activated" | "scheduled_task_rejected")
        )
    {
        return Err(ScheduleStoreError::Conflict);
    }
    // Only the exact approval receipt may advance this waiting snapshot.
    // Never adopt a new user input, changed scope, stopped turn or another lease.
    let mut expected = session.clone();
    expected.conversation.push(event.clone());
    expected.version = expected
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    expected.updated_at = current.updated_at.clone();
    if expected != current || current.version != row.version {
        return Err(ScheduleStoreError::Conflict);
    }
    let approved = decision["event"].as_str() == Some("scheduled_task_activated");
    *session = current;
    observation.permission(if approved {
        PermissionOutcome::Approved
    } else {
        PermissionOutcome::Denied
    });
    Ok(ReviewPoll::Decided)
}
