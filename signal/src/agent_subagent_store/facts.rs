//! Read original OSS action bindings and results within the caller's writer.
use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::{
    agent_action_item as work, agent_capability_dispatch_outbox as outbox, agent_exec_task,
};
use desk_diagnose_core::{
    dynamic_run::PermissionRequestState,
    session::ExecutionState,
    subagent::{
        facts::{MAX_ACTION_FACTS, RuntimeFacts, command_succeeded, validate_origin},
        state::TaskDependency,
    },
};

pub(crate) async fn runtime_facts_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    now_ms: i64,
) -> Result<RuntimeFacts, DbErr> {
    let mut facts = RuntimeFacts::default();
    let rows = work::Entity::find()
        .filter(work::Column::ConversationId.eq(&session.conversation_id))
        .filter(work::Column::ActorId.eq(&session.actor_id))
        .filter(work::Column::TargetDeviceId.eq(&session.device_id))
        .order_by_asc(work::Column::Id)
        .limit(MAX_ACTION_FACTS as u64 + 1)
        .all(txn)
        .await?;
    if rows.len() > MAX_ACTION_FACTS {
        return Err(invalid());
    }
    let mut tracked_generations = Vec::new();
    for row in rows {
        let id = format!("work:{}", row.id);
        use crate::capability_grant_store::*;
        if row.dispatch_intent_at.is_none() {
            match row.status.as_str() {
                CAPABILITY_WORK_PREPARED => {
                    facts
                        .dependency(TaskDependency::Work { work_id: id })
                        .map_err(|_| invalid())?;
                }
                CAPABILITY_WORK_SUPERSEDED
                | CAPABILITY_WORK_REVOKED
                | CAPABILITY_WORK_REVIEW_CLOSED => {}
                _ => {
                    facts.incomplete(id).map_err(|_| invalid())?;
                }
            }
            continue;
        }
        let original = outbox::Entity::find()
            .filter(outbox::Column::WorkId.eq(row.id))
            .order_by_asc(outbox::Column::Id)
            .limit(2)
            .all(txn)
            .await?;
        let [dispatch] = original.as_slice() else {
            return Err(invalid());
        };
        let payload: CapabilityDispatchPayload =
            serde_json::from_str(&dispatch.payload_json).map_err(|_| invalid())?;
        if payload.work_id != row.id
            || payload.dispatch_id != dispatch.dispatch_id
            || payload.call_id != row.tool_call_id
            || payload.call_id != row.action_request_id
            || dispatch.payload_schema_version != 1
            || row.payload_schema_version != 1
        {
            return Err(invalid());
        }
        if payload.tool_name == desk_diagnose_core::command_confirmation::COMMAND_TOOL {
            let origin = payload.command_origin.as_ref().ok_or_else(invalid)?;
            validate_origin(session, origin).map_err(|_| invalid())?;
            let task = agent_exec_task::Entity::find()
                .filter(agent_exec_task::Column::ExecutionGeneration.eq(&dispatch.dispatch_id))
                .filter(agent_exec_task::Column::ConversationId.eq(&session.conversation_id))
                .one(txn)
                .await?;
            let Some(task) = task else {
                facts
                    .dependency(TaskDependency::Work { work_id: id })
                    .map_err(|_| invalid())?;
                continue;
            };
            if task.tool_call_id != origin.tool_call_id {
                return Err(invalid());
            }
            tracked_generations.push(task.execution_generation.clone());
            if task.status == crate::agent_exec_store::STATUS_DONE {
                let Some((_output, receipt)) =
                    crate::agent_exec_store::SignalAgentExecStore::command_result_on(txn, &task)
                        .await
                        .map_err(|_| invalid())?
                else {
                    return Err(invalid());
                };
                let disposition: desk_agent_protocol::edge_exec::EdgeExecDisposition =
                    serde_json::from_str(task.disposition_json.as_deref().ok_or_else(invalid)?)
                        .map_err(|_| invalid())?;
                let (success, required) = match disposition {
                    desk_agent_protocol::edge_exec::EdgeExecDisposition::Executed { outcome } => {
                        let required = !matches!(&outcome, desk_agent_protocol::AgentOutcome::Ok(desk_agent_protocol::OperationOutput::Exec(output)) if !output.started);
                        (command_succeeded(&outcome), required)
                    }
                    desk_agent_protocol::edge_exec::EdgeExecDisposition::RejectedBeforeDispatch { .. }
                    | desk_agent_protocol::edge_exec::EdgeExecDisposition::DispatchFailedBeforeWorker { .. }
                    | desk_agent_protocol::edge_exec::EdgeExecDisposition::HostAtCapacity { .. } => (false, false),
                    desk_agent_protocol::edge_exec::EdgeExecDisposition::ExecutionStateUnknown { .. } => { return Err(invalid()); }
                };
                facts
                    .receipt_for_call(
                        &origin.tool_call_id,
                        &receipt,
                        row.is_side_effecting && required,
                        success,
                        success,
                    )
                    .map_err(|_| invalid())?;
            } else if row.manual_resolved_at.is_some() {
                facts.incomplete(id).map_err(|_| invalid())?;
            } else if matches!(
                task.status.as_str(),
                crate::agent_exec_store::STATUS_DISPATCHING
                    | crate::agent_exec_store::STATUS_RUNNING
                    | crate::agent_exec_store::STATUS_UNKNOWN
            ) {
                facts
                    .dependency(TaskDependency::Work {
                        work_id: format!("exec:{}", task.id),
                    })
                    .map_err(|_| invalid())?;
            } else {
                return Err(invalid());
            }
        } else {
            if dispatch.computer_binding_json.is_none() {
                if row.manual_resolved_at.is_some() {
                    facts.incomplete(id).map_err(|_| invalid())?;
                } else {
                    facts
                        .dependency(TaskDependency::Work { work_id: id })
                        .map_err(|_| invalid())?;
                }
                continue;
            }
            let (current, origin, result) =
                SignalCapabilityGrantStore::computer_facts_on(txn, &dispatch.dispatch_id).await?;
            if current != row {
                return Err(invalid());
            }
            validate_origin(session, &origin).map_err(|_| invalid())?;
            tracked_generations.push(dispatch.dispatch_id.clone());
            if let Some(result) = result {
                let verified = result.native_verified;
                let required = row.is_side_effecting && result.native_result != desk_agent_protocol::computer_use::ComputerActionResultClass::DefinitelyNotStarted;
                facts
                    .receipt_for_call(
                        &origin.tool_call_id,
                        &result.receipt,
                        required,
                        result.outcome == CapabilityDispatchOutcome::Succeeded,
                        verified,
                    )
                    .map_err(|_| invalid())?;
            } else if row.manual_resolved_at.is_some() {
                facts.incomplete(id).map_err(|_| invalid())?;
            } else {
                facts
                    .dependency(TaskDependency::Work { work_id: id })
                    .map_err(|_| invalid())?;
            }
        }
    }
    for request in &session.permission_requests {
        if request.input_revision == session.input_revision
            && request.state == PermissionRequestState::Pending
        {
            facts
                .dependency(TaskDependency::Approval {
                    permission_request_id: request.request_id.clone(),
                })
                .map_err(|_| invalid())?;
        }
    }
    for directory in session.file_scope.records() {
        if directory.state == desk_diagnose_core::file_scope::DirectoryConsentState::Pending {
            facts
                .dependency(TaskDependency::DirectoryApproval {
                    directory_request_id: directory.proposal.request_id.clone(),
                })
                .map_err(|_| invalid())?;
        }
    }
    for action in session.execution_state.tasks() {
        if !tracked_generations.contains(&action.execution_id) {
            facts
                .incomplete(format!("untracked:{}", action.execution_id))
                .map_err(|_| invalid())?;
        }
    }
    if session
        .execution_state
        .states()
        .iter()
        .any(|state| matches!(state, ExecutionState::Interrupted { .. }))
    {
        facts
            .incomplete("interrupted-action".into())
            .map_err(|_| invalid())?;
    }
    facts
        .retained_evidence(session, u64::try_from(now_ms).map_err(|_| invalid())?)
        .map_err(|_| invalid())?;
    Ok(facts)
}
