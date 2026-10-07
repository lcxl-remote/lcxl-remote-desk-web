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
    reconcile_inner_on(txn, session, outbox, work, payload, now_ms, None).await
}

/// Observe an original published command after its planner fence has advanced.
/// The historical grant authorizes receipt recovery only, never another send.
pub(in crate::capability_grant_store) async fn reconcile_history_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
    now_ms: u64,
    context: &crate::schedule_store::TaskReceiptContext,
) -> Result<(String, ActionIdentity, bool), DbErr> {
    reconcile_inner_on(txn, session, outbox, work, payload, now_ms, Some(context)).await
}

async fn reconcile_inner_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
    now_ms: u64,
    history: Option<&crate::schedule_store::TaskReceiptContext>,
) -> Result<(String, ActionIdentity, bool), DbErr> {
    let origin = payload.command_origin.as_ref().ok_or_else(invalid)?;
    origin.validate().map_err(|_| invalid())?;
    if outbox.computer_binding_json.is_some()
        || payload.tool_name != desk_diagnose_core::command_confirmation::COMMAND_TOOL
        || origin.tool_name != payload.tool_name
        || origin.provider_id != payload.provider_id
        || (history.is_none()
            && session.agent_role.is_main()
            && AssistantTurnFence::from_session(session)
                .map_err(|_| invalid())?
                .as_ref()
                != Some(&origin.turn_fence))
    {
        return Err(invalid());
    }
    if let Some(context) = history {
        let fence = &origin.turn_fence;
        if !session.agent_role.is_main()
            || session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.conversation_id != context.run_id()
            || fence.delegation.is_some()
            || fence.conversation_id != session.conversation_id
            || fence.actor_id != session.actor_id
            || fence.device_id != session.device_id
            || fence.input_revision != session.input_revision
            || fence.input_revision != 1
            || session.current_turn_id.as_deref() != Some(fence.turn_id.as_str())
            || fence.lease_token > session.lease_token
            || fence.control_revision > session.control_revision
            || work.actor_id != session.actor_id
            || work.target_device_id != session.device_id
            || work.conversation_id != session.conversation_id
            || work.turn_id != fence.turn_id
            || work.manual_resolved_at.is_some()
        {
            return Err(invalid());
        }
        SignalCapabilityGrantStore::validate_task_dispatch_history_on(
            txn,
            &payload.dispatch_id,
            context.provenance(),
        )
        .await?;
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
    if call.name != payload.tool_name || canonical != payload.canonical_input_json {
        return Err(invalid());
    }
    if history.is_some()
        && !session.conversation.iter().any(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_deref() == Some(work.turn_id.as_str())
                && message
                    .tool_calls
                    .iter()
                    .any(|proposal| proposal.id == call.id)
                && message.data_envelope.as_ref().is_some_and(|envelope| {
                    envelope.validate().is_ok()
                        && origin.source_envelope_ids.contains(&envelope.envelope_id)
                })
        })
    {
        return Err(invalid());
    }
    if session.agent_role.binding().is_some() {
        desk_diagnose_core::subagent::facts::validate_origin(session, origin)
            .map_err(|_| invalid())?;
    } else if history.is_none() {
        let mut expected = ActionResultOrigin::capture(
            &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
            session,
            &call,
        )
        .map_err(|_| invalid())?;
        let context = origin.command_completion.as_ref().ok_or_else(invalid)?;
        if context.context_sha256
            != desk_diagnose_core::command_completion::context_digest(session)
                .map_err(|_| invalid())?
        {
            return Err(invalid());
        }
        expected.retention.expires_at_unix_ms = Some(context.expires_at_unix_ms);
        expected.command_completion = Some(context.clone());
        if &expected != origin {
            return Err(invalid());
        }
    }
    let task = agent_exec_task::Entity::find()
        .filter(agent_exec_task::Column::ExecutionGeneration.eq(&payload.dispatch_id))
        .one(txn)
        .await?;
    let Some(task) = task else {
        if session.agent_role.binding().is_some() {
            let (call, action) =
                super::unbound::reconcile_on(txn, session, outbox, work, payload, now_ms).await?;
            return Ok((call, action, true));
        }
        return Err(invalid());
    };
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
    if session
        .execution_state
        .tasks()
        .into_iter()
        .any(|old| old.kind == WorkKind::AgentExec && old.work_id == task.id && old != &action)
    {
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
                    || !desk_diagnose_core::conversation_attachment::delivery::matches_original(
                        m,
                        &output.content,
                        Some(&receipt.envelope),
                    )
                    || m.image_data_url != output.image_data_url
                    || m.tool_call_id.as_deref() != Some(call.id.as_str())
                    || (m.raw_result.is_none()
                        && m.data_envelope.as_ref() != Some(&receipt.envelope))
            })
        {
            return Err(invalid());
        }
        let newly_delivered = existing.is_empty();
        if newly_delivered {
            let execution = session
                .execution_state
                .execution(&action.execution_id)
                .unwrap_or_default();
            if let ExecutionState::OutcomeUnknown {
                action: current,
                placeholder_message_id,
                ..
            } = &execution
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
                output.format,
                timestamp(now_ms)?.to_rfc3339(),
            );
        }
        session.execution_state.remove(&action);
        if newly_delivered
            && session.agent_role.binding().is_some()
            && origin.turn_fence.input_revision == session.input_revision
        {
            session.add_pending_auto_trigger(desk_diagnose_core::session::PendingAutoTrigger {
                work_id: action.work_id,
                kind: WorkKind::AgentExec,
                execution_id: action.execution_id.clone(),
                tool_call_id: call.id.clone(),
                event_id: task.event_id.clone(),
                chain_id: session.chain_id.clone(),
                resolution_org_id: None,
                since: timestamp(now_ms)?.to_rfc3339(),
            });
        }
        // A child still needs the original tool-free interpreter. Keep its
        // native delivery pending until that interpreter drains the trigger.
        // Scheduled/foreground recovery otherwise owns delivery itself.
        let pending_child_interpretation = session.agent_role.binding().is_some()
            && session.pending_auto_triggers.iter().any(|pending| {
                pending.kind == WorkKind::AgentExec
                    && pending.chain_id == session.chain_id
                    && pending.event_id == task.event_id
                    && pending.execution_id == task.execution_generation
                    && pending.tool_call_id == task.tool_call_id
            });
        if !pending_child_interpretation {
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
        }
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
            session.execution_state.execution(&action.execution_id),
            Some(ExecutionState::OutcomeUnknown { .. })
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
        if !session.execution_state.contains(&action) {
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
    session.execution_state.insert(if unknown {
        ExecutionState::OutcomeUnknown {
            action: action.clone(),
            placeholder_message_id: anchor,
            since: task.created_at.to_rfc3339(),
        }
    } else {
        ExecutionState::Executing {
            action: action.clone(),
        }
    });
    Ok((call.id, action, true))
}
