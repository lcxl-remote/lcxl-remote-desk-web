//! Original action evidence inside the caller's schedule transaction.
use super::*;
use desk_diagnose_core::{
    action_result::ActionResultOrigin,
    action_turn_fence::AssistantTurnFence,
    chat::{ChatMessage, ChatRole, ToolCall},
    dynamic_run::BackgroundTaskState,
    session::{ActionIdentity, ExecutionState, TriggerOrigin, TurnState, WorkKind},
};
use sea_orm::{DatabaseTransaction, QueryOrder};
use std::collections::BTreeSet;

mod command;
pub(super) mod prepared;
mod reads;
mod unbound;

fn invalid() -> DbErr {
    DbErr::Custom("invalid original scheduled action recovery".into())
}

/// The caller holds a SQLite write transaction and has verified the original
/// task/session/occurrence identity and both expired executor leases.
pub(crate) async fn reconcile_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    now_ms: u64,
) -> Result<(), DbErr> {
    if session.trigger_origin != TriggerOrigin::ScheduledContinuation
        || session.turn_state != TurnState::Running
        || matches!(session.execution_state, ExecutionState::Interrupted { .. })
    {
        return Err(invalid());
    }
    let turn = session.current_turn_id.clone().ok_or_else(invalid)?;
    let rows = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ConversationId.eq(&session.conversation_id))
        .order_by_asc(agent_action_item::Column::Id)
        .all(txn)
        .await?;
    let carried = session.execution_state.waitable_task().cloned();
    let mut matched = BTreeSet::new();
    let mut matched_actions = Vec::new();
    let mut pending = false;
    for row in rows {
        if row.turn_id != turn {
            if carried.as_ref().is_some_and(|action| {
                action.kind == WorkKind::ComputerAction && action.work_id == row.id
            }) {
                return Err(invalid());
            }
            continue;
        }
        if row.kind != CAPABILITY_WORK_KIND
            || row.actor_id != session.actor_id
            || row.target_device_id != session.device_id
            || row.manual_resolved_at.is_some()
        {
            return Err(invalid());
        }
        if matches!(
            row.status.as_str(),
            CAPABILITY_WORK_PREPARED | CAPABILITY_WORK_SUPERSEDED | CAPABILITY_WORK_REVOKED
        ) {
            let call = prepared::close_on(txn, session, &row, now_ms).await?;
            if !matched.insert(call) {
                return Err(invalid());
            }
            continue;
        }
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(row.id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let (outbox, work, payload) =
            super::computer_binding::original_on(txn, &outbox.dispatch_id).await?;
        if payload.command_origin.is_some() {
            let (call, action, waiting) =
                command::reconcile_on(txn, session, &outbox, &work, &payload, now_ms).await?;
            if !matched.insert(call) || (pending && waiting) {
                return Err(invalid());
            }
            pending |= waiting;
            matched_actions.push(action);
            continue;
        }
        if outbox.computer_binding_json.is_none() {
            let (call, action) =
                unbound::reconcile_on(txn, session, &outbox, &work, &payload, now_ms).await?;
            if !matched.insert(call) || pending {
                return Err(invalid());
            }
            pending = true;
            matched_actions.push(action);
            continue;
        }
        let binding = super::computer_background::bound(&outbox, &work, &payload)?;
        if AssistantTurnFence::from_session(session)
            .map_err(|_| invalid())?
            .as_ref()
            != Some(&binding.origin.turn_fence)
        {
            return Err(invalid());
        }
        let proposals: Vec<_> = session
            .conversation
            .iter()
            .filter(|message| message.role == ChatRole::Assistant)
            .flat_map(|message| message.tool_calls.iter())
            .filter(|call| call.id == binding.origin.tool_call_id)
            .collect();
        if proposals.len() != 1 {
            return Err(invalid());
        }
        let proposal = proposals[0];
        let call = ToolCall {
            id: proposal.id.clone(),
            name: proposal.name.clone(),
            arguments_json: proposal.arguments_json.clone(),
        };
        let origin = ActionResultOrigin::capture(
            &desk_diagnose_core::device_assistant::device_assistant_provider_registry(),
            session,
            &call,
        )
        .map_err(|_| invalid())?;
        if origin != binding.origin || !matched.insert(call.id.clone()) {
            return Err(invalid());
        }
        let action = ActionIdentity::new(
            work.id,
            &payload.call_id,
            &payload.dispatch_id,
            WorkKind::ComputerAction,
        );
        matched_actions.push(action.clone());
        if carried.as_ref().is_some_and(|old| {
            old.kind == WorkKind::ComputerAction && old.work_id == work.id && old != &action
        }) {
            return Err(invalid());
        }
        if let Some(original) =
            super::computer_completion::terminal_result(&outbox, work.clone(), &payload)?
        {
            super::computer_delivery::validate_destination(session, &original)?;
            let present: Vec<_> = session
                .conversation
                .iter()
                .filter(|message| {
                    matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                        && message.tool_call_id.as_deref() == Some(call.id.as_str())
                        && message.data_envelope.as_ref() == Some(&original.receipt.envelope)
                })
                .collect();
            if present.len() > 1
                || present.first().is_some_and(|message| {
                    message.text != original.output.content
                        || message.image_data_url != original.output.image_data_url
                })
            {
                return Err(invalid());
            }
            if present.is_empty() {
                session.apply_completion_with_envelope(
                    &original.work.completion_event_id,
                    &payload.dispatch_id,
                    &call.id,
                    &action.action_request_id,
                    &original.output.content,
                    Some(original.receipt.envelope),
                    timestamp(now_ms)?.to_rfc3339(),
                );
            }
            if session.execution_state.waitable_task() == Some(&action) {
                session.execution_state = ExecutionState::None;
            }
        } else {
            if pending
                || !matches!(
                    outbox.state.as_str(),
                    DISPATCH_OUTBOX_SENDING | DISPATCH_OUTBOX_OUTCOME_UNKNOWN
                )
                || !matches!(
                    work.status.as_str(),
                    CAPABILITY_WORK_DISPATCHING | CAPABILITY_WORK_OUTCOME_UNKNOWN
                )
            {
                return Err(invalid());
            }
            pending = true;
            let running = super::computer_background::task_on(txn, &work, now_ms)
                .await?
                .is_some_and(|task| {
                    matches!(
                        task.state,
                        BackgroundTaskState::Running | BackgroundTaskState::CancelRequested
                    )
                });
            let unknown = !running
                || outbox.state == DISPATCH_OUTBOX_OUTCOME_UNKNOWN
                || matches!(
                    session.execution_state,
                    ExecutionState::OutcomeUnknown { .. }
                );
            let id = format!("scheduled-action-status:{}", payload.dispatch_id);
            let mut message = ChatMessage::tool_result(
                &id,
                &call.id,
                if unknown {
                    "The original action outcome is unknown. Do not retry automatically."
                } else {
                    "The original action is still running. Await its original result; do not dispatch again."
                },
            );
            message.turn_id = Some(turn.clone());
            message.background_task_id = Some(action.action_request_id.clone());
            let parent = session
                .conversation
                .iter()
                .find(|message| {
                    message.role == ChatRole::Assistant
                        && message
                            .tool_calls
                            .iter()
                            .any(|candidate| candidate.id == call.id)
                })
                .and_then(|message| message.data_envelope.as_ref())
                .ok_or_else(invalid)?;
            message.data_envelope =
                desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                    Some(parent),
                    &call.id,
                    &message.text,
                    "scheduled_action_status",
                )
                .map_err(|_| invalid())?;
            let mut indexes = session
                .conversation
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    item.tool_call_id.as_deref() == Some(call.id.as_str())
                        && matches!(item.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                })
                .map(|(index, _)| index);
            let index = indexes.next();
            if indexes.next().is_some() {
                return Err(invalid());
            }
            if let Some(index) = index {
                if session.execution_state.waitable_task() != Some(&action) {
                    return Err(invalid());
                }
                message.message_id = session.conversation[index].message_id.clone();
                message.role = session.conversation[index].role;
            }
            let anchor = message.message_id.clone();
            if let Some(index) = index {
                session.conversation[index] = message;
            } else {
                session.conversation.push(message);
            }
            session.execution_state = if unknown {
                ExecutionState::OutcomeUnknown {
                    action,
                    placeholder_message_id: anchor,
                    since: work.dispatch_intent_at.ok_or_else(invalid)?.to_rfc3339(),
                }
            } else {
                ExecutionState::Executing { action }
            };
        }
    }
    reads::close_untracked_on(txn, session, &mut matched).await?;
    if !session.unclosed_tool_call_ids().is_empty()
        || session
            .conversation
            .iter()
            .filter(|message| message.turn_id.as_deref() == Some(turn.as_str()))
            .flat_map(|message| message.tool_calls.iter())
            .any(|call| !matched.contains(&call.id))
        || carried
            .as_ref()
            .is_some_and(|action| !matched_actions.contains(action))
    {
        return Err(invalid());
    }
    Ok(())
}
