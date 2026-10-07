//! Main tool effects and labelled transcript receipts share one root transaction.
use super::*;
use desk_agent_protocol::data_lineage::DataEnvelope;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ToolCall},
    subagent::{
        creation::TaskCreationEnvelope,
        seam::{ObservedResult, ToolReceipt},
        tools::{self, Operation},
        wait::{ParentWait, TaskFence, WaitEvaluation},
    },
};
use sea_orm::DatabaseTransaction;
use sha2::{Digest, Sha256};

fn task_query(parent: &PersistedAgentSession) -> sea_orm::Select<run_row::Entity> {
    run_row::Entity::find()
        .filter(run_row::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(run_row::Column::ActorId.eq(&parent.actor_id))
        .filter(run_row::Column::DeviceId.eq(&parent.device_id))
}

pub(crate) async fn task_for_parent_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    task_id: &str,
) -> Result<run_row::Model, DbErr> {
    task_query(parent)
        .filter(run_row::Column::TaskId.eq(task_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)
}

fn task_labels(row: &run_row::Model) -> Result<Vec<DataEnvelope>, DbErr> {
    let run = decode_run(row)?;
    let creation: TaskCreationEnvelope =
        serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
    creation
        .validate_task(&run.binding)
        .map_err(|_| invalid())?;
    Ok(vec![
        creation.instruction.data_envelope.ok_or_else(invalid)?,
    ])
}

pub(crate) async fn current_group_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
) -> Result<DelegationGroup, DbErr> {
    let row = group_row::Entity::find()
        .filter(
            group_row::Column::GroupId
                .eq(parent.delegation_group_id.as_deref().ok_or_else(invalid)?),
        )
        .filter(group_row::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(group_row::Column::ActorId.eq(&parent.actor_id))
        .filter(group_row::Column::DeviceId.eq(&parent.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&row)?;
    if !group.can_interpret(parent.input_revision, parent.control_revision) {
        return Err(invalid());
    }
    super::scheduled_source::current_scheduled_source_on(txn, &row).await?;
    Ok(group)
}

async fn begin_main_tool(
    store: &SubAgentStore,
    held: &PersistedAgentSession,
    call: &ToolCall,
    operation: &Operation,
) -> Result<DatabaseTransaction, DbErr> {
    let txn = crate::db::begin_write(&store.db, session_row::Entity).await?;
    let child_id = match operation {
        Operation::Spawn(_) => {
            let group_id = held.delegation_group_id.as_deref().ok_or_else(invalid)?;
            let key = format!(
                "{:x}",
                Sha256::digest(
                    serde_json::to_vec(&(
                        "subagent_creation",
                        &held.conversation_id,
                        group_id,
                        &held.actor_id,
                        &held.device_id,
                        &call.id,
                    ))
                    .map_err(|_| invalid())?
                )
            );
            Some(format!("sa-{key}"))
        }
        Operation::Cancel { task_id, .. } | Operation::Message { task_id, .. } => Some(
            task_for_parent_on(&txn, held, task_id)
                .await?
                .child_conversation_id,
        ),
        _ => None,
    };
    let _ = child_id;
    Ok(txn)
}

/// Required children are checked from durable task rows in goal settlement;
/// a model's completion claim cannot clear a still-running dependency.
pub(crate) async fn required_children_complete_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
) -> Result<bool, DbErr> {
    required_children_reviewed_on(txn, parent, true).await
}

async fn required_children_reviewed_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    interpreted: bool,
) -> Result<bool, DbErr> {
    let group = current_group_on(txn, parent).await?;
    if group.source.goal_id()
        != parent
            .focus_epoch
            .goal_segment
            .as_ref()
            .map(|segment| segment.goal_id.as_str())
    {
        return Err(invalid());
    }
    let mut states = Vec::new();
    for id in &group.required_task_ids {
        let run = decode_run(&task_for_parent_on(txn, parent, id).await?)?;
        if run.binding.group_id != group.group_id || run.binding.source != group.source {
            return Err(invalid());
        }
        let observations = if interpreted {
            &parent.interpreted_subagent_results
        } else {
            &parent.accepted_subagent_observations
        };
        if !observations.iter().any(|observation| {
            observation.result.task_id == *id
                && observation.result.state_revision == run.state_revision
                && observation.result.parent_input_revision == parent.input_revision
                && observation.result.parent_control_revision == parent.control_revision
        }) {
            return Ok(false);
        }
        states.push((id.clone(), run.state));
    }
    Ok(group.required_dependencies_complete(&states))
}

impl SubAgentStore {
    pub async fn required_children_complete_for_turn(
        &self,
        held: &PersistedAgentSession,
    ) -> Result<bool, DbErr> {
        let txn = self.db.begin().await?;
        let parent = parent_planning_on(&txn, held, chrono::Utc::now().timestamp_millis()).await?;
        let ready = required_children_reviewed_on(&txn, &parent, false).await?;
        txn.commit().await?;
        Ok(ready)
    }
    /// Old groups contribute only reviewed opaque identifiers and server states.
    /// Their names and reports are available through explicitly labelled reads.
    pub async fn main_projection_for_turn(
        &self,
        held: &PersistedAgentSession,
    ) -> Result<ChatMessage, DbErr> {
        let txn = self.db.begin().await?;
        let parent = parent_planning_on(&txn, held, chrono::Utc::now().timestamp_millis()).await?;
        let group = current_group_on(&txn, &parent).await?;
        let policy = crate::subagent_policy::read(&txn).await?;
        let source = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&group.group_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut labels = decode_creation(&source)?
            .input_envelopes()
            .map_err(|_| invalid())?;
        let query = task_query(&parent);
        let total = query.clone().count(&txn).await?;
        let unfinished_query = query.clone().filter(run_row::Column::State.is_not_in([
            "completed",
            "failed",
            "cancelled",
        ]));
        let unfinished = unfinished_query.clone().count(&txn).await?;
        let mut rows = unfinished_query
            .order_by_asc(run_row::Column::Id)
            .limit(desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY as u64 + 1)
            .all(&txn)
            .await?;
        if rows.len() > desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY {
            return Err(invalid());
        }
        for row in query
            .order_by_desc(run_row::Column::Id)
            .limit(10)
            .all(&txn)
            .await?
        {
            if !rows.iter().any(|existing| existing.id == row.id) {
                rows.push(row);
            }
        }
        let mut tasks = Vec::new();
        for row in rows {
            let run = decode_run(&row)?;
            if run.binding.group_id == group.group_id {
                labels.extend(task_labels(&row)?);
                tasks.push(serde_json::json!({"task": run.summary(), "required_for_completion": group.required_task_ids.contains(&run.binding.task_id)}));
            } else {
                tasks.push(serde_json::json!({"task_id": run.binding.task_id, "group_id": run.binding.group_id,
                    "state": run.state, "input_revision": run.binding.input_revision,
                    "control_revision": run.binding.control_revision, "state_revision": run.state_revision,
                    "details_require_explicit_read": true}));
            }
        }
        let payload = serde_json::json!({"group_id": group.group_id, "source_epoch": group.source_epoch,
            "deadline_ms": group.limits.deadline_ms, "total": total, "unfinished": unfinished,
            "creation_limits": policy.limits, "policy_revision": policy.revision,
            "created_in_group": group.tasks_created,
            "creation_capacity_rule": "Only unfinished children count toward the configured limit, including queued, approval, background work and cancelling tasks. Reject new creation at the limit; terminal tasks release slots. There is no cumulative creation limit.",
            "remaining_unfinished_slots": (policy.limits.max_unfinished_per_root as u64).saturating_sub(unfinished),
            "tasks": tasks, "omitted": total.saturating_sub(tasks.len() as u64),
            "rule": "Current task state is durable across compression. Use list/status/result tools for omitted details. Task completion is not main task completion; verify reports against receipts. Approval attention does not finish a wait."});
        let id = format!(
            "main-delegation-{:x}",
            Sha256::digest(payload.to_string().as_bytes())
        );
        let message =
            desk_diagnose_core::subagent::projection::runtime_message(&id, &payload, &labels)
                .map_err(|_| invalid())?;
        if message.text.len() as u64 > group.limits.max_context_bytes {
            return Err(invalid());
        }
        txn.commit().await?;
        Ok(message)
    }

    pub async fn execute_main_tool(
        &self,
        held: &mut PersistedAgentSession,
        call: &ToolCall,
        operation: Operation,
        result_message_id: &str,
    ) -> Result<ToolReceipt, DbErr> {
        if tools::parse(held, call).map_err(|_| invalid())? != operation
            || !desk_diagnose_core::subagent::valid_id(result_message_id)
        {
            return Err(invalid());
        }
        let txn = begin_main_tool(self, held, call, &operation).await?;
        let now = chrono::Utc::now();
        let parent = parent_planning_on(&txn, held, now.timestamp_millis()).await?;
        let caller = parent
            .conversation
            .iter()
            .find(|message| {
                message.role == ChatRole::Assistant
                    && message.turn_id == parent.current_turn_id
                    && message.tool_calls.iter().any(|reference| {
                        reference.id == call.id
                            && reference.name == call.name
                            && tools::same_arguments(
                                &reference.arguments_json,
                                &call.arguments_json,
                            )
                    })
            })
            .and_then(|message| message.data_envelope.clone())
            .ok_or_else(invalid)?;
        if let Some(result) = parent.conversation.iter().find(|message| {
            message.role == ChatRole::Tool
                && message.tool_call_id.as_deref() == Some(call.id.as_str())
        }) {
            if result
                .data_envelope
                .as_ref()
                .is_none_or(|label| label.provenance.source_tool_name != call.name)
            {
                return Err(invalid());
            }
            let receipt = ToolReceipt {
                payload: serde_json::from_str(&result.text).map_err(|_| invalid())?,
                result_message_id: result.message_id.clone(),
            };
            txn.commit().await?;
            return Ok(receipt);
        }
        let group = current_group_on(&txn, &parent).await?;
        let mut labels = vec![caller];
        let mut next = held.clone();
        let mut observation = None;
        let payload = match operation {
            Operation::Spawn(request) => {
                let summary = super::creation::spawn_on(&txn, &parent, call, &request).await?;
                labels.extend(task_labels(
                    &task_for_parent_on(&txn, &parent, &summary.task_id).await?,
                )?);
                serde_json::to_value(summary).map_err(|_| invalid())?
            }
            Operation::Cancel { .. } | Operation::Message { .. } => {
                let controlled = super::task_control::apply_task_control_from_turn_on(
                    &txn,
                    &parent,
                    call,
                    now.timestamp_millis(),
                )
                .await?;
                labels.extend(task_labels(
                    &task_for_parent_on(&txn, &parent, &controlled.task.task_id).await?,
                )?);
                serde_json::to_value(controlled.task).map_err(|_| invalid())?
            }
            Operation::List { cursor, limit } => {
                let before = cursor
                    .as_deref()
                    .map(str::parse::<i64>)
                    .transpose()
                    .map_err(|_| invalid())?
                    .unwrap_or(i64::MAX);
                if before <= 0 {
                    return Err(invalid());
                }
                let query = task_query(&parent);
                let total = query.clone().count(&txn).await?;
                let unfinished = query
                    .clone()
                    .filter(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
                    .count(&txn)
                    .await?;
                let rows = query
                    .filter(run_row::Column::Id.lt(before))
                    .order_by_desc(run_row::Column::Id)
                    .limit(u64::from(limit) + 1)
                    .all(&txn)
                    .await?;
                let mut items = Vec::new();
                for row in rows.iter().take(limit as usize) {
                    labels.extend(task_labels(row)?);
                    items.push(decode_run(row)?.summary());
                }
                let has_more = rows.len() > limit as usize;
                let next_cursor = has_more.then(|| rows[limit as usize - 1].id.to_string());
                serde_json::to_value(AiAssistantSubAgentPage {
                    items,
                    total,
                    unfinished,
                    has_more,
                    next_cursor,
                })
                .map_err(|_| invalid())?
            }
            Operation::Status { task_id } => {
                let row = task_for_parent_on(&txn, &parent, &task_id).await?;
                labels.extend(task_labels(&row)?);
                serde_json::to_value(decode_run(&row)?.summary()).map_err(|_| invalid())?
            }
            Operation::ReadResult {
                task_id,
                include_task,
            } => {
                let row = task_for_parent_on(&txn, &parent, &task_id).await?;
                let run = decode_run(&row)?;
                let result = run.result();
                labels.extend(task_labels(&row)?);
                if result.report.is_some() {
                    let label: DataEnvelope = serde_json::from_str(
                        row.result_envelope_json.as_deref().ok_or_else(invalid)?,
                    )
                    .map_err(|_| invalid())?;
                    label.validate().map_err(|_| invalid())?;
                    labels.push(label);
                    observation = Some(ObservedResult {
                        task_id,
                        state_revision: run.state_revision,
                        tool_call_id: call.id.clone(),
                        result_message_id: result_message_id.into(),
                        parent_input_revision: parent.input_revision,
                        parent_control_revision: parent.control_revision,
                    });
                }
                serde_json::to_value(
                    desk_diagnose_core::subagent::result::SubAgentModelResult::from_result(
                        &result,
                        include_task,
                    ),
                )
                .map_err(|_| invalid())?
            }
            Operation::Wait { task_ids, mode } => {
                if parent.subagent_wait.is_some()
                    || (parent.ready_subagent_wait.is_some()
                        && parent.trigger_origin
                            != desk_diagnose_core::session::TriggerOrigin::SubAgentCompletion)
                    || now.timestamp_millis() >= group.limits.deadline_ms
                {
                    return Err(invalid());
                }
                let mut current = Vec::new();
                let mut summaries = Vec::new();
                for task_id in task_ids {
                    let row = task_for_parent_on(&txn, &parent, &task_id).await?;
                    let run = decode_run(&row)?;
                    labels.extend(task_labels(&row)?);
                    summaries.push(run.summary());
                    current.push((
                        TaskFence {
                            task_id,
                            input_revision: run.binding.input_revision,
                            control_revision: run.binding.control_revision,
                        },
                        run.state,
                    ));
                }
                let id = format!(
                    "subagent-wait-{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&(
                            &parent.conversation_id,
                            &group.group_id,
                            parent.input_revision,
                            parent.control_revision,
                            &call.id,
                        ))
                        .map_err(|_| invalid())?
                    )
                );
                let wait = ParentWait {
                    wait_id: id,
                    tool_call_id: call.id.clone(),
                    result_message_id: result_message_id.into(),
                    group_id: group.group_id.clone(),
                    source_epoch: group.source_epoch,
                    retry_after_ms: None,
                    parent_input_revision: parent.input_revision,
                    parent_control_revision: parent.control_revision,
                    mode,
                    tasks: current.iter().map(|(fence, _)| fence.clone()).collect(),
                };
                wait.validate().map_err(|_| invalid())?;
                let ready = wait.evaluate(parent.input_revision, parent.control_revision, &current)
                    == WaitEvaluation::Ready;
                let payload = serde_json::json!({"wait_id": wait.wait_id, "status": if ready {"ready"} else {"waiting"},
                    "tasks": summaries, "rule": "Waiting releases model capacity; completion does not grant new device authority."});
                next.ready_subagent_wait = None;
                next.ready_subagent_notification = None;
                if !ready {
                    next.subagent_wait = Some(wait);
                }
                payload
            }
        };
        let text = payload.to_string();
        if text.len() as u64 > group.limits.max_context_bytes {
            return Err(invalid());
        }
        if next
            .conversation
            .iter()
            .any(|message| message.message_id == result_message_id)
        {
            return Err(invalid());
        }
        let mut message = ChatMessage::tool_result(result_message_id, &call.id, &text);
        message.data_envelope = Some(
            desk_diagnose_core::subagent::projection::envelope(
                result_message_id,
                &text,
                &call.name,
                &labels,
            )
            .map_err(|_| invalid())?,
        );
        next.conversation.push(message);
        if let Some(observation) = observation {
            observation.validate().map_err(|_| invalid())?;
            if next.observed_subagent_results.len() >= 64 {
                return Err(invalid());
            }
            next.observed_subagent_results.push(observation);
        }
        let version = write_child_session_on(&txn, &next, now).await?;
        txn.commit().await?;
        next.version = version;
        *held = next;
        Ok(ToolReceipt {
            payload,
            result_message_id: result_message_id.into(),
        })
    }
}
