//! Classify original native facts for schedule settlement; never dispatch or replay.
use super::*;
use sea_orm::DatabaseTransaction;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScheduledNativeDisposition {
    Settled,
    Pending,
    Unknown,
}

pub(crate) async fn scheduled_native_on(
    txn: &DatabaseTransaction,
    group_row: &group_row::Model,
) -> Result<ScheduledNativeDisposition, DbErr> {
    let group = decode_group(group_row)?;
    let children = super::group_children_on(txn, &group).await?;
    let mut sessions = vec![group.root_conversation_id.clone()];
    let mut expected = std::collections::BTreeMap::new();
    for child in children {
        let run = decode_run(&child)?;
        if run.binding.source != group.source {
            return Err(invalid());
        }
        sessions.push(run.child_conversation_id.clone());
        expected.insert(run.child_conversation_id.clone(), run);
    }
    sessions.sort();
    sessions.dedup();
    let mut pending = false;
    let mut unknown = false;
    for id in sessions {
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&id))
            .filter(session_row::Column::ActorId.eq(&group.actor_id))
            .filter(session_row::Column::DeviceId.eq(&group.device_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.conversation_id != id
            || session.actor_id != group.actor_id
            || session.device_id != group.device_id
            || session.version != row.version
            || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
            || (id != group.root_conversation_id
                && session.agent_role.binding().is_none_or(|binding| {
                    binding.root_conversation_id != group.root_conversation_id
                        || binding.group_id != group.group_id
                }))
        {
            return Err(invalid());
        }
        if let Some(run) = expected.get(&id) {
            run.validate_session(&session).map_err(|_| invalid())?;
        } else if !session.agent_role.is_main() {
            return Err(invalid());
        }
        let mut generations = std::collections::BTreeSet::new();
        let mut resolved_generations = std::collections::BTreeSet::new();
        let limit = desk_diagnose_core::subagent::facts::MAX_ACTION_FACTS;
        use crate::capability_grant_store::*;
        use crate::entity::{
            agent_action_item as action, agent_capability_dispatch_outbox as outbox,
            agent_exec_task as exec,
        };
        // Confirmed commands can outlive their capability work projection.
        // Their original generation is independently durable and must also pin
        // the occurrence while a real worker still runs it.
        let commands = exec::Entity::find()
            .filter(exec::Column::ConversationId.eq(&id))
            .order_by_asc(exec::Column::Id)
            .limit(limit as u64 + 1)
            .all(txn)
            .await?;
        if commands.len() > limit {
            return Err(invalid());
        }
        for task in commands {
            generations.insert(task.execution_generation.clone());
            match task.status.as_str() {
                crate::agent_exec_store::STATUS_RUNNING
                | crate::agent_exec_store::STATUS_DISPATCHING => {
                    pending = true;
                }
                crate::agent_exec_store::STATUS_UNKNOWN => {
                    unknown = true;
                }
                crate::agent_exec_store::STATUS_DONE => {
                    if crate::agent_exec_store::SignalAgentExecStore::command_result_on(txn, &task)
                        .await
                        .map_err(|_| invalid())?
                        .is_some()
                    {
                        resolved_generations.insert(task.execution_generation.clone());
                    } else {
                        unknown = true;
                    }
                }
                _ => {
                    unknown = true;
                }
            }
        }
        let rows = action::Entity::find()
            .filter(action::Column::ConversationId.eq(&id))
            .filter(action::Column::ActorId.eq(&group.actor_id))
            .filter(action::Column::TargetDeviceId.eq(&group.device_id))
            .order_by_asc(action::Column::Id)
            .limit(limit as u64 + 1)
            .all(txn)
            .await?;
        if rows.len() > limit {
            return Err(invalid());
        }
        for action in rows {
            if action.dispatch_intent_at.is_none() {
                match action.status.as_str() {
                    CAPABILITY_WORK_SUPERSEDED
                    | CAPABILITY_WORK_REVOKED
                    | CAPABILITY_WORK_REVIEW_CLOSED => {}
                    CAPABILITY_WORK_PREPARED => {
                        pending = true;
                    }
                    _ => {
                        unknown = true;
                    }
                }
                continue;
            }
            let dispatch = outbox::Entity::find()
                .filter(outbox::Column::WorkId.eq(action.id))
                .one(txn)
                .await?
                .ok_or_else(invalid)?;
            let payload: CapabilityDispatchPayload =
                serde_json::from_str(&dispatch.payload_json).map_err(|_| invalid())?;
            if payload.work_id != action.id
                || payload.dispatch_id != dispatch.dispatch_id
                || payload.call_id != action.tool_call_id
                || payload.call_id != action.action_request_id
                || dispatch.payload_schema_version != 1
                || action.payload_schema_version != 1
            {
                return Err(invalid());
            }
            generations.insert(dispatch.dispatch_id.clone());
            if payload.tool_name == desk_diagnose_core::command_confirmation::COMMAND_TOOL {
                let task = exec::Entity::find()
                    .filter(exec::Column::ExecutionGeneration.eq(&dispatch.dispatch_id))
                    .filter(exec::Column::ConversationId.eq(&id))
                    .one(txn)
                    .await?;
                let Some(task) = task else {
                    if action.status == CAPABILITY_WORK_OUTCOME_UNKNOWN {
                        unknown = true;
                    } else {
                        pending = true;
                    }
                    continue;
                };
                let origin = payload.command_origin.as_ref().ok_or_else(invalid)?;
                origin.validate().map_err(|_| invalid())?;
                if task.exec_request_id != payload.call_id
                    || task.tool_call_id != origin.tool_call_id
                    || origin.tool_name != payload.tool_name
                    || origin.turn_fence.conversation_id != id
                    || origin.turn_fence.actor_id != group.actor_id
                    || origin.turn_fence.device_id != group.device_id
                {
                    return Err(invalid());
                }
                match task.status.as_str() {
                    crate::agent_exec_store::STATUS_DONE => {
                        if crate::agent_exec_store::SignalAgentExecStore::command_result_on(
                            txn, &task,
                        )
                        .await
                        .map_err(|_| invalid())?
                        .is_some()
                        {
                            resolved_generations.insert(task.execution_generation.clone());
                        } else {
                            unknown = true;
                        }
                    }
                    crate::agent_exec_store::STATUS_UNKNOWN => {
                        unknown = true;
                    }
                    crate::agent_exec_store::STATUS_RUNNING
                    | crate::agent_exec_store::STATUS_DISPATCHING => {
                        pending = true;
                    }
                    _ => {
                        unknown = true;
                    }
                }
            } else {
                if dispatch.computer_binding_json.is_none() {
                    if action.status == CAPABILITY_WORK_OUTCOME_UNKNOWN {
                        unknown = true;
                    } else {
                        pending = true;
                    }
                    continue;
                }
                let (current, _origin, result) =
                    SignalCapabilityGrantStore::computer_facts_on(txn, &dispatch.dispatch_id)
                        .await?;
                if current != action {
                    return Err(invalid());
                }
                if result.is_some() {
                    resolved_generations.insert(dispatch.dispatch_id.clone());
                } else if action.status == CAPABILITY_WORK_OUTCOME_UNKNOWN {
                    unknown = true;
                } else {
                    pending = true;
                }
            }
        }
        if session.execution_state.states().iter().any(|state| {
            matches!(state,
            desk_diagnose_core::session::ExecutionState::OutcomeUnknown { action, .. }
                if !resolved_generations.contains(&action.execution_id))
        }) {
            unknown = true;
        }
        if session
            .execution_state
            .tasks()
            .iter()
            .any(|task| !generations.contains(&task.execution_id))
            || session.execution_state.states().iter().any(|state| {
                matches!(
                    state,
                    desk_diagnose_core::session::ExecutionState::Interrupted { .. }
                )
            })
        {
            unknown = true;
        }
    }
    Ok(if pending {
        ScheduledNativeDisposition::Pending
    } else if unknown {
        ScheduledNativeDisposition::Unknown
    } else {
        ScheduledNativeDisposition::Settled
    })
}
