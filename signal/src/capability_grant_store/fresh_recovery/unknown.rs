//! Preserve the exact dispatched action as unknown, never as completion evidence.
use super::*;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole},
    session::{ActionIdentity, WorkKind},
};

pub(super) async fn restore(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    row: &agent_action_item::Model,
    context: &crate::schedule_store::TaskReceiptContext,
) -> Result<(), DbErr> {
    if row.status != CAPABILITY_WORK_OUTCOME_UNKNOWN
        || row.kind != CAPABILITY_WORK_KIND
        || row.actor_id != session.actor_id
        || row.target_device_id != session.device_id
        || session.current_turn_id.as_deref() != Some(row.turn_id.as_str())
        || row.result_json.is_some()
        || row.manual_resolved_at.is_some()
    {
        return Err(invalid());
    }
    let outbox = agent_capability_dispatch_outbox::Entity::find()
        .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(row.id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let (outbox, work, payload) =
        super::super::computer_binding::original_on(txn, &outbox.dispatch_id).await?;
    if outbox.state != DISPATCH_OUTBOX_OUTCOME_UNKNOWN {
        return Err(invalid());
    }
    let binding = super::super::computer_background::bound(&outbox, &work, &payload)?;
    SignalCapabilityGrantStore::validate_task_dispatch_history_on(
        txn,
        &outbox.dispatch_id,
        context.provenance(),
    )
    .await?;
    let action = ActionIdentity::new(
        work.id,
        &payload.call_id,
        &payload.dispatch_id,
        WorkKind::ComputerAction,
    );
    if session.execution_state.tasks().into_iter().any(|current| {
        (current.action_request_id == action.action_request_id
            || current.execution_id == action.execution_id)
            && current != &action
    }) {
        return Err(invalid());
    }
    let calls: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_deref() == Some(work.turn_id.as_str())
        })
        .flat_map(|message| message.tool_calls.iter().map(move |call| (message, call)))
        .filter(|(_, call)| call.id == binding.origin.tool_call_id)
        .collect();
    let [(proposal, call)] = calls.as_slice() else {
        return Err(invalid());
    };
    let original = desk_diagnose_core::chat::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments_json.clone(),
    };
    let captured = desk_diagnose_core::action_result::ActionResultOrigin::capture(
        &desk_diagnose_core::device_assistant::device_assistant_provider_registry(),
        session,
        &original,
    )
    .map_err(|_| invalid())?;
    if captured != binding.origin {
        return Err(invalid());
    }
    let mut message = ChatMessage::tool_result(
        format!("task-action-unknown:{}", payload.dispatch_id),
        &call.id,
        "The original action outcome is unknown. Do not retry automatically.",
    );
    message.turn_id = Some(work.turn_id.clone());
    message.background_task_id = Some(action.action_request_id.clone());
    message.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            proposal.data_envelope.as_ref(),
            &call.id,
            &message.text,
            "task_unknown_action",
        )
        .map_err(|_| invalid())?;
    let indexes: Vec<_> = session
        .conversation
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            matches!(item.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                && item.tool_call_id.as_deref() == Some(call.id.as_str())
        })
        .map(|(index, _)| index)
        .collect();
    match indexes.as_slice() {
        [] => {
            if session
                .conversation
                .iter()
                .any(|item| item.message_id == message.message_id)
            {
                return Err(invalid());
            }
            session.conversation.push(message.clone());
        }
        [index] => {
            if !session.execution_state.contains(&action) {
                return Err(invalid());
            }
            message.message_id = session.conversation[*index].message_id.clone();
            message.role = session.conversation[*index].role;
            session.conversation[*index] = message.clone();
        }
        _ => return Err(invalid()),
    }
    session
        .execution_state
        .insert(ExecutionState::OutcomeUnknown {
            action,
            placeholder_message_id: message.message_id,
            since: work.dispatch_intent_at.ok_or_else(invalid)?.to_rfc3339(),
        });
    Ok(())
}
