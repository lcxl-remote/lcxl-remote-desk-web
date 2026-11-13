//! Commit an AI draft and its original tool-result receipt under one turn fence.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    model_management::{cancel, list, lock_target},
    resume_activation::activate_on,
    session::{
        OrgMode, ScheduleHostStores, SessionGuard, SessionKey, SessionPatch, TouchColumn, timestamp,
    },
};
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ToolCall},
    model_observability::{InputIssue, Stage, StageOutcome, tool::ToolObservation},
    schedule::management_tools::Action,
    session::PersistedAgentSession,
};
use sea_orm::ConnectionTrait;

/// Applies a parsed management action in the caller's transaction after its
/// admission locks. Returns the receipt id and the session to adopt once the
/// caller commits.
pub async fn manage_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    owner: i32,
    session: &PersistedAgentSession,
    call: &ToolCall,
    action: Action,
    observation: &ToolObservation,
) -> Result<(String, PersistedAgentSession), ScheduleStoreError> {
    let target = lock_target(txn, owner, session, &action).await?;
    let locked = host
        .touch_session(
            txn,
            TouchColumn::Version,
            SessionGuard {
                conversation: Some(&session.conversation_id),
                actor: Some(&session.actor_id),
                device: Some(&session.device_id),
                version: Some(session.version),
                lease_token: Some(
                    i64::try_from(session.lease_token).map_err(|_| ScheduleStoreError::Invalid)?,
                ),
                ..SessionGuard::any()
            },
        )
        .await?;
    if locked != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &session.conversation_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let now = authority_now(txn).await?;
    if row.state_json
        != session
            .encode_json_for_storage()
            .map_err(|_| ScheduleStoreError::Invalid)?
        || row.lease_deadline_ms.is_none_or(|deadline| deadline <= now)
        || !row.personal
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let parents: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| {
            message.role == ChatRole::Assistant
                && message.tool_calls.iter().any(|tool| {
                    tool.id == call.id
                        && tool.name == call.name
                        && tool.arguments_json == call.arguments_json
                })
        })
        .collect();
    if parents.len() != 1 || !session.unclosed_tool_call_ids().contains(&call.id) {
        return Err(ScheduleStoreError::Conflict);
    }
    if desk_diagnose_core::model_controls::is_control(&call.name) {
        host.validate_model_control(txn, session, call)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    let parent = parents[0]
        .data_envelope
        .as_ref()
        .ok_or(ScheduleStoreError::Invalid)?;
    let mut awaiting_review = None;
    let (content, source) = match action {
        Action::Create(draft) => {
            desk_diagnose_core::schedule::validate_publication(
                &draft.spec,
                now,
                draft.kind == desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume,
            )
            .map_err(|_| {
                observation.reject(Stage::Preflight, InputIssue::Precondition);
                ScheduleStoreError::ToolInput(
                    desk_diagnose_core::schedule::proposal::NO_FUTURE_OCCURRENCE,
                )
            })?;
            if draft.kind == desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume {
                host.check_resume_retention(txn, &draft.spec, now)
                    .await
                    .inspect_err(|error| {
                        if matches!(error, ScheduleStoreError::ExceedsSessionRetention { .. }) {
                            observation.reject(Stage::Preflight, InputIssue::Precondition);
                        }
                    })?;
            }
            observation.stage(Stage::Preflight, StageOutcome::Passed);
            let mut task = desk_diagnose_core::future::send_phase(|| {
                host.create_draft(txn, owner, &draft, now, Some(session))
            })
            .await?;
            desk_diagnose_core::future::send_phase(|| {
                host.record_prompt_retention(txn, &mut task, session, parents[0])
            })
            .await?;
            let message = if task.kind == "conversation_resume" {
                // A continuation needs no approval: enable it in this turn's
                // transaction without writing to the running session.
                task = activate_on(host, txn, owner, &task, false).await?;
                "The conversation timer is enabled. It grants no tool permissions; its run approves device actions one at a time."
            } else {
                awaiting_review = Some(task.schedule_id.clone());
                "The AI reviewer or the owner decides this proposal in the application review dialog, and this turn waits for that decision. Do not report success before it. No extra chat confirmation is needed. No tool permission is granted; every run approves device actions one at a time."
            };
            (serde_json::json!({"schedule_id":task.schedule_id,"kind":task.kind,"state":task.status,
                "next_run_at_utc_ms":task.next_run_at,"awaiting_decision":awaiting_review.is_some(),
                "message":message}).to_string(), "schedule_proposal")
        }
        Action::List { after, limit } => {
            observation.stage(Stage::Preflight, StageOutcome::Passed);
            (
                list(txn, owner, session, after, limit, now).await?,
                "schedule_query",
            )
        }
        Action::Cancel {
            expected_revision, ..
        } => (
            cancel(txn, target, expected_revision, now, observation).await?,
            "schedule_cancellation",
        ),
    };
    let envelope = desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
        Some(parent),
        &call.id,
        &content,
        source,
    )
    .map_err(|_| ScheduleStoreError::Invalid)?;
    let mut next = session.clone();
    let mut row = row;
    if awaiting_review.is_some()
        && host
            .open_for_schedule_proposal(
                txn,
                &row.raw,
                &mut next,
                u64::try_from(now).map_err(|_| ScheduleStoreError::Invalid)?,
            )
            .await?
    {
        // The audit event advanced the row; the receipt follows it.
        row = host
            .find_session(txn, SessionKey::Id(row.id), OrgMode::Any, false)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
    }
    next.pending_schedule_review = awaiting_review;
    let mut message =
        ChatMessage::tool_result(format!("schedule-proposal:{}", call.id), &call.id, content);
    message.turn_id = next.current_turn_id.clone();
    message.data_envelope = envelope;
    next.conversation.push(message);
    next.version = next
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let timestamp = timestamp(now)?;
    next.updated_at = timestamp.to_rfc3339();
    let state_json = next
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
                version: Some(next.version),
                state_json: Some(&state_json),
                updated_at_ms: Some(now),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok((format!("schedule-proposal:{}", call.id), next))
}
