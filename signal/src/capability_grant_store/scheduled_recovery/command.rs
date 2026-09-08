//! Reconcile the original command identity, never a Computer Action identity.
use super::*;
use crate::{agent_exec_store::SignalAgentExecStore, entity::agent_exec_task};

pub(super) async fn reconcile_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
    now_ms: u64,
) -> Result<(String, ActionIdentity, bool), DbErr> {
    let origin = payload.command_origin.as_ref().ok_or_else(invalid)?;
    origin.validate().map_err(|_| invalid())?;
    if outbox.computer_binding_json.is_some()
        || payload.tool_name != desk_diagnose_core::command_confirmation::COMMAND_TOOL
        || origin.tool_name != payload.tool_name
        || origin.provider_id != payload.provider_id
        || AssistantTurnFence::from_session(session)
            .map_err(|_| invalid())?
            .as_ref()
            != Some(&origin.turn_fence)
    {
        return Err(invalid());
    }
    let proposals: Vec<_> = session
        .conversation
        .iter()
        .filter(|m| {
            m.role == ChatRole::Assistant && m.turn_id.as_deref() == Some(work.turn_id.as_str())
        })
        .flat_map(|m| m.tool_calls.iter())
        .filter(|call| call.id == origin.tool_call_id)
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
    let canonical = desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
        &call.name,
        serde_json::from_str(&call.arguments_json).map_err(|_| invalid())?,
    )
    .map_err(|_| invalid())?;
    if canonical != payload.canonical_input_json {
        return Err(invalid());
    }
    let mut expected = ActionResultOrigin::capture(
        &desk_diagnose_core::device_assistant::device_assistant_provider_registry(),
        session,
        &call,
    )
    .map_err(|_| invalid())?;
    let context = origin.command_completion.as_ref().ok_or_else(invalid)?;
    if context.context_sha256
        != desk_diagnose_core::command_completion::context_digest(session).map_err(|_| invalid())?
    {
        return Err(invalid());
    }
    expected.retention.expires_at_unix_ms = Some(context.expires_at_unix_ms);
    expected.command_completion = Some(context.clone());
    if &expected != origin {
        return Err(invalid());
    }
    let task = agent_exec_task::Entity::find()
        .filter(agent_exec_task::Column::ExecutionGeneration.eq(&payload.dispatch_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if task.exec_request_id != payload.call_id
        || task.conversation_id != session.conversation_id
        || task.tool_call_id != call.id
        || task.target_connection_id.is_empty()
        || task.event_id != format!("signal-exec:{}:done", payload.call_id)
    {
        return Err(invalid());
    }
    let action =
        ActionIdentity::agent_exec(task.id, &task.exec_request_id, &task.execution_generation);
    if session.execution_state.waitable_task().is_some_and(|old| {
        old.kind == WorkKind::AgentExec && old.work_id == task.id && old != &action
    }) {
        return Err(invalid());
    }
    if task.status == crate::agent_exec_store::STATUS_DONE {
        let (output, receipt) = SignalAgentExecStore::command_result_on(txn, &task)
            .await
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
        if outbox.state == DISPATCH_OUTBOX_COMPLETED {
            let completed: CapabilityDispatchCompletion =
                serde_json::from_str(work.result_json.as_deref().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?;
            validate_completion(&completed)?;
            if completed.dispatch_id != payload.dispatch_id
                || completed.call_id != payload.call_id
                || completed.generation != payload.generation
                || work.result_schema_version != Some(1)
                || work.status
                    != match completed.outcome {
                        CapabilityDispatchOutcome::Succeeded => CAPABILITY_WORK_SUCCEEDED,
                        CapabilityDispatchOutcome::Failed => CAPABILITY_WORK_FAILED,
                    }
                || (completed.outcome == CapabilityDispatchOutcome::Succeeded
                    && completed.result_digest_sha256 != receipt.envelope.digest_sha256)
            {
                return Err(invalid());
            }
        } else {
            SignalCapabilityGrantStore::record_dispatch_completion_on(
                txn,
                &CapabilityDispatchCompletion {
                    dispatch_id: payload.dispatch_id.clone(),
                    call_id: payload.call_id.clone(),
                    generation: payload.generation,
                    outcome: CapabilityDispatchOutcome::Succeeded,
                    result_digest_sha256: receipt.envelope.digest_sha256.clone(),
                },
                now_ms,
            )
            .await?;
        }
        let existing: Vec<_> = session
            .conversation
            .iter()
            .filter(|m| {
                m.message_id == task.event_id
                    || (m.tool_call_id.as_deref() == Some(call.id.as_str())
                        && m.data_envelope.as_ref() == Some(&receipt.envelope))
            })
            .collect();
        if existing.len() > 1
            || existing.first().is_some_and(|m| {
                !matches!(m.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                    || m.text != output.content
                    || m.image_data_url != output.image_data_url
                    || m.tool_call_id.as_deref() != Some(call.id.as_str())
                    || m.data_envelope.as_ref() != Some(&receipt.envelope)
            })
        {
            return Err(invalid());
        }
        if existing.is_empty() {
            if let ExecutionState::OutcomeUnknown {
                action: current,
                placeholder_message_id,
                ..
            } = &session.execution_state
                && current == &action
                && !session.conversation.iter().any(|m| {
                    &m.message_id == placeholder_message_id
                        && m.tool_call_id.as_deref() == Some(call.id.as_str())
                })
            {
                return Err(invalid());
            }
            session.apply_completion_with_envelope(
                &task.event_id,
                &task.execution_generation,
                &call.id,
                &task.exec_request_id,
                output.content,
                Some(receipt.envelope),
                timestamp(now_ms)?.to_rfc3339(),
            );
        }
        if session.execution_state.waitable_task() == Some(&action) {
            session.execution_state = ExecutionState::None;
        }
        // The recovered turn owns delivery, including a foreground result with
        // a different message ID. A publisher must not create another follow-up.
        agent_exec_task::Entity::update_many()
            .set(agent_exec_task::ActiveModel {
                delivery_state: Set(crate::agent_exec_store::DELIVERY_CONSUMED.into()),
                ..Default::default()
            })
            .filter(agent_exec_task::Column::Id.eq(task.id))
            .filter(agent_exec_task::Column::ExecutionGeneration.eq(&task.execution_generation))
            .filter(agent_exec_task::Column::Status.eq(crate::agent_exec_store::STATUS_DONE))
            .filter(
                agent_exec_task::Column::DeliveryState
                    .eq(crate::agent_exec_store::DELIVERY_PENDING),
            )
            .exec(txn)
            .await?;
        return Ok((call.id, action, false));
    }
    if !matches!(task.status.as_str(), "dispatching" | "running" | "unknown")
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
    let unknown = task.status != crate::agent_exec_store::STATUS_RUNNING
        || task.deadline <= timestamp(now_ms)?
        || outbox.state == DISPATCH_OUTBOX_OUTCOME_UNKNOWN
        || matches!(
            session.execution_state,
            ExecutionState::OutcomeUnknown { .. }
        );
    let mut message = ChatMessage::tool_result(
        format!("scheduled-command-status:{}", payload.dispatch_id),
        &call.id,
        if unknown {
            "The original command outcome is unknown. Do not retry automatically."
        } else {
            "Awaiting the original command result. Do not dispatch the command again."
        },
    );
    message.turn_id = Some(work.turn_id.clone());
    message.background_task_id = Some(task.exec_request_id.clone());
    let parent = session
        .conversation
        .iter()
        .find(|m| m.role == ChatRole::Assistant && m.tool_calls.iter().any(|c| c.id == call.id))
        .and_then(|m| m.data_envelope.as_ref())
        .ok_or_else(invalid)?;
    message.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            Some(parent),
            &call.id,
            &message.text,
            "scheduled_command_status",
        )
        .map_err(|_| invalid())?;
    let indexes: Vec<_> = session
        .conversation
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.tool_call_id.as_deref() == Some(call.id.as_str())
                && matches!(m.role, ChatRole::Tool | ChatRole::UntrustedOutput)
        })
        .map(|(i, _)| i)
        .collect();
    if indexes.len() > 1 {
        return Err(invalid());
    }
    if let Some(&index) = indexes.first() {
        if session.execution_state.waitable_task() != Some(&action) {
            return Err(invalid());
        }
        message.message_id = session.conversation[index].message_id.clone();
        message.role = session.conversation[index].role;
    }
    let anchor = message.message_id.clone();
    if let Some(&index) = indexes.first() {
        session.conversation[index] = message;
    } else {
        session.conversation.push(message);
    }
    session.execution_state = if unknown {
        ExecutionState::OutcomeUnknown {
            action: action.clone(),
            placeholder_message_id: anchor,
            since: task.created_at.to_rfc3339(),
        }
    } else {
        ExecutionState::Executing {
            action: action.clone(),
        }
    };
    Ok((call.id, action, true))
}
