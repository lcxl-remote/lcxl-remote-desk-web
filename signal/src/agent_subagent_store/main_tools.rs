//! Main tool effects and labelled transcript receipts share one root transaction.
use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_agent_protocol::data_lineage::DataEnvelope;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ToolCall},
    model_observability::{
        InputIssue, PermissionOutcome, Stage, StageOutcome, tool::ToolObservation,
    },
    subagent::{
        creation::TaskCreationEnvelope,
        seam::{ObservedResult, ToolReceipt},
        tools::{self, Operation},
        wait::{ParentWait, TaskFence, WaitEvaluation},
    },
};

use sha2::{Digest, Sha256};

fn worker_scope(parent: &PersistedAgentSession) -> sea_orm::Select<run_row::Entity> {
    desk_assistant_data_store::worker_request::in_scope(
        &parent.actor_id,
        &parent.device_id,
        &parent.conversation_id,
        parent.agent_role.workbench_id(),
    )
}

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
    task_for_parent_observed_on(txn, parent, task_id, &ToolObservation::default()).await
}

async fn task_for_parent_observed_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    task_id: &str,
    observation: &ToolObservation,
) -> Result<run_row::Model, DbErr> {
    observation.stage(Stage::Reference, StageOutcome::Attempted);
    task_query(parent)
        .filter(run_row::Column::TaskId.eq(task_id))
        .one(txn)
        .await?
        .ok_or_else(|| {
            observation.reject(Stage::Reference, InputIssue::UnknownReference);
            invalid()
        })
}

fn task_labels(row: &run_row::Model) -> Result<Vec<DataEnvelope>, DbErr> {
    let run = decode_run(row)?;
    let creation: TaskCreationEnvelope =
        serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
    creation
        .validate_task(&run.binding)
        .map_err(|_| invalid())?;
    Ok(vec![
        creation
            .instruction
            .data_envelope
            .ok_or_else(|| invalid())?,
    ])
}

pub(crate) async fn current_group_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
) -> Result<DelegationGroup, DbErr> {
    let (group, _) = current_group_record_on(txn, parent).await?;
    Ok(group)
}

async fn current_group_record_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
) -> Result<(DelegationGroup, String), DbErr> {
    let row = group_row::Entity::find()
        .filter(
            group_row::Column::GroupId.eq(parent
                .delegation_group_id
                .as_deref()
                .ok_or_else(|| invalid())?),
        )
        .filter(group_row::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(group_row::Column::ActorId.eq(&parent.actor_id))
        .filter(group_row::Column::DeviceId.eq(&parent.device_id))
        .one(txn)
        .await?
        .ok_or_else(|| invalid())?;
    let group = decode_group(&row)?;
    if !group.can_interpret(parent.input_revision, parent.control_revision)
        || group.source.workbench_id() != parent.agent_role.workbench_id()
    {
        return Err(invalid());
    }
    super::scheduled_source::current_scheduled_source_on(txn, &row).await?;
    Ok((group, row.model_binding_json))
}

async fn begin_main_tool(
    store: &SubAgentStore,
    held: &PersistedAgentSession,
    call: &ToolCall,
    operation: &Operation,
    input_observation: &ToolObservation,
) -> Result<DatabaseTransaction, DbErr> {
    let txn = crate::db::begin_write(&store.db, session_row::Entity).await?;
    let child_id = match operation {
        Operation::Spawn(_) => {
            let group_id = held
                .delegation_group_id
                .as_deref()
                .ok_or_else(|| invalid())?;
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
            Some(desk_diagnose_core::native_ids::stable(
                desk_diagnose_core::native_ids::Kind::ChildConversation,
                &key,
            ))
        }
        Operation::Cancel { task_id, .. }
        | Operation::Adjust { task_id, .. }
        | Operation::Reply {
            request_id: task_id,
            ..
        } => Some(
            task_for_parent_observed_on(&txn, held, task_id, input_observation)
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
        let txn = desk_diagnose_core::future::boxed(|| async { self.db.begin().await }).await?;
        let parent = desk_diagnose_core::future::boxed(|| async {
            parent_planning_on(&txn, held, chrono::Utc::now().timestamp_millis()).await
        })
        .await?;
        let group =
            desk_diagnose_core::future::boxed(|| async { current_group_on(&txn, &parent).await })
                .await?;
        let policy = desk_diagnose_core::future::boxed(|| async {
            crate::subagent_policy::read(&txn).await
        })
        .await?;
        let source = desk_diagnose_core::future::boxed(|| async {
            group_row::Entity::find()
                .filter(group_row::Column::GroupId.eq(&group.group_id))
                .one(&txn)
                .await
        })
        .await?
        .ok_or_else(|| invalid())?;
        let creation = decode_creation(&source)?;
        let mut labels = creation.input_envelopes().map_err(|_| invalid())?;
        let query = task_query(&parent);
        let total =
            desk_diagnose_core::future::boxed(|| async { query.clone().count(&txn).await }).await?;
        let unfinished_query = query.clone().filter(run_row::Column::State.is_not_in([
            "completed",
            "failed",
            "cancelled",
        ]));
        let unfinished = desk_diagnose_core::future::boxed(|| async {
            unfinished_query.clone().count(&txn).await
        })
        .await?;
        let mut rows = desk_diagnose_core::future::boxed(|| async {
            unfinished_query
                .order_by_asc(run_row::Column::Id)
                .limit(desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY as u64 + 1)
                .all(&txn)
                .await
        })
        .await?;
        if rows.len() > desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY {
            return Err(invalid());
        }
        for row in desk_diagnose_core::future::boxed(|| async {
            query
                .order_by_desc(run_row::Column::Id)
                .limit(10)
                .all(&txn)
                .await
        })
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
        let mut payload = serde_json::json!({"group_id": group.group_id, "source_epoch": group.source_epoch,
            "deadline_ms": group.limits.deadline_ms, "total": total, "unfinished": unfinished,
            "creation_limits": policy.limits, "policy_revision": policy.revision,
            "created_in_group": group.tasks_created,
            "creation_capacity_rule": "Only unfinished children count toward the configured limit, including queued, approval, background work and cancelling tasks. Reject new creation at the limit; terminal tasks release slots. There is no cumulative creation limit.",
            "remaining_unfinished_slots": (policy.limits.max_unfinished_per_root as u64).saturating_sub(unfinished),
            "tasks": tasks, "omitted": total.saturating_sub(tasks.len() as u64),
            "pending_reports": [],
            "rule": "Current task state is durable across compression. Use list, status and report tools for omitted details. Pending reports are independent; acknowledge only the ones you have handled. Verify reports against receipts. Approval attention does not finish a wait."});
        if let Some(workbench) = group.source.workbench_id() {
            let workers = desk_diagnose_core::future::boxed(|| {
                desk_assistant_data_store::worker_request::recent_workbench_workers_on(
                    &txn,
                    &parent.actor_id,
                    workbench,
                )
            })
            .await?;
            payload["recent_workbench_worker_ids"] = serde_json::json!(workers);
            let payload = payload.as_object_mut().ok_or_else(invalid)?;
            payload.remove("remaining_unfinished_slots");
            payload.insert("creation_capacity_rule".into(), serde_json::json!(
                "A dispatched request can queue when Worker slots are occupied. The configured Worker limit applies to requests that have started and are still unfinished, including waits."
            ));
        }
        let max_bytes = usize::try_from(group.limits.max_context_bytes).map_err(|_| invalid())?;
        let base_bytes = desk_diagnose_core::subagent::projection::runtime_text(&payload).len();
        let reports = desk_diagnose_core::future::boxed(|| {
            desk_assistant_data_store::worker_report::project_pending_on(
                &txn,
                &group,
                &creation.model_destination,
                chrono::Utc::now().timestamp_millis(),
                max_bytes.saturating_sub(base_bytes),
                &mut labels,
            )
        })
        .await?;
        payload["pending_reports"] = serde_json::json!(reports);
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
        desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
        Ok(message)
    }

    #[cfg(test)]
    pub async fn execute_main_tool(
        &self,
        held: &mut PersistedAgentSession,
        call: &ToolCall,
        operation: Operation,
        result_message_id: &str,
    ) -> Result<ToolReceipt, DbErr> {
        self.execute_main_tool_observed(
            held,
            call,
            operation,
            result_message_id,
            &ToolObservation::default(),
        )
        .await
    }

    pub async fn execute_main_tool_observed(
        &self,
        held: &mut PersistedAgentSession,
        call: &ToolCall,
        operation: Operation,
        result_message_id: &str,
        input_observation: &ToolObservation,
    ) -> Result<ToolReceipt, DbErr> {
        if tools::parse_observed(held, call, input_observation).map_err(|_| invalid())? != operation
            || !desk_diagnose_core::subagent::valid_id(result_message_id)
        {
            return Err(invalid());
        }
        let txn = desk_diagnose_core::future::boxed(|| async {
            begin_main_tool(self, held, call, &operation, input_observation).await
        })
        .await?;
        let history = desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::native_scope_on(
                &txn,
                &held.actor_id,
                &held.device_id,
                &held.conversation_id,
            )
        })
        .await?;
        let now = chrono::Utc::now();
        let parent = desk_diagnose_core::future::boxed(|| async {
            parent_planning_on(&txn, held, now.timestamp_millis()).await
        })
        .await?;
        if desk_diagnose_core::model_controls::is_control(&call.name) {
            desk_diagnose_core::future::boxed(|| async {
                crate::model_egress_store::SignalModelEgressStore::validate_control_on(
                    &txn, &parent, call,
                )
                .await
            })
            .await?;
        }
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
                                &call.name,
                                &reference.arguments_json,
                                &call.arguments_json,
                            )
                    })
            })
            .and_then(|message| message.data_envelope.clone())
            .ok_or_else(|| invalid())?;
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
            if let Some(history) = &history {
                desk_diagnose_core::future::boxed(|| history.capture_on(&txn)).await?;
            }
            desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
            return Ok(receipt);
        }
        let (group, model_binding) = desk_diagnose_core::future::boxed(|| async {
            current_group_record_on(&txn, &parent).await
        })
        .await?;
        let mut labels = vec![caller];
        let mut next = held.clone();
        let mut observation = None;
        let mut permission_ends =
            desk_diagnose_core::model_observability::permission::PendingEnds::default();
        let payload = match operation {
            Operation::Spawn(request) => {
                let summary = desk_diagnose_core::future::boxed(|| async {
                    super::creation::spawn_on(&txn, &parent, call, &request).await
                })
                .await?;
                labels.extend(task_labels(
                    &desk_diagnose_core::future::boxed(|| async {
                        task_for_parent_on(&txn, &parent, &summary.task_id).await
                    })
                    .await?,
                )?);
                serde_json::to_value(summary).map_err(|_| invalid())?
            }
            Operation::Cancel { .. } | Operation::Adjust { .. } => {
                let controlled = desk_diagnose_core::future::boxed(|| async {
                    super::task_control::apply_task_control_from_turn_on(
                        &txn,
                        &parent,
                        call,
                        now.timestamp_millis(),
                        input_observation,
                    )
                    .await
                })
                .await?;
                labels.extend(task_labels(
                    &desk_diagnose_core::future::boxed(|| async {
                        task_for_parent_on(&txn, &parent, &controlled.outcome.task.task_id).await
                    })
                    .await?,
                )?);
                permission_ends = controlled.permission_ends;
                serde_json::to_value(controlled.outcome.task).map_err(|_| invalid())?
            }
            Operation::List { cursor, limit } => {
                if cursor.is_some() {
                    input_observation.stage(Stage::Reference, StageOutcome::Attempted);
                }
                let page = desk_diagnose_core::future::boxed(|| {
                    desk_assistant_data_store::worker_request::worker_page_on(
                        &txn,
                        worker_scope(&parent),
                        cursor.as_deref(),
                        limit,
                    )
                })
                .await?
                .ok_or_else(|| {
                    input_observation.reject(Stage::Reference, InputIssue::UnknownReference);
                    invalid()
                })?;
                if cursor.is_some() {
                    input_observation.stage(Stage::Reference, StageOutcome::Passed);
                }
                serde_json::to_value(page).map_err(|_| invalid())?
            }
            Operation::WorkerStatus { worker_id } => {
                input_observation.stage(Stage::Reference, StageOutcome::Attempted);
                let status = desk_diagnose_core::future::boxed(|| {
                    desk_assistant_data_store::worker_request::worker_status_on(
                        &txn,
                        worker_scope(&parent),
                        &worker_id,
                    )
                })
                .await?
                .ok_or_else(|| {
                    input_observation.reject(Stage::Reference, InputIssue::UnknownReference);
                    invalid()
                })?;
                input_observation.stage(Stage::Reference, StageOutcome::Passed);
                serde_json::to_value(status).map_err(|_| invalid())?
            }
            Operation::Status { task_id } => {
                let row = desk_diagnose_core::future::boxed(|| async {
                    task_for_parent_observed_on(&txn, &parent, &task_id, input_observation).await
                })
                .await?;
                labels.extend(task_labels(&row)?);
                serde_json::to_value(decode_run(&row)?.summary()).map_err(|_| invalid())?
            }
            Operation::Reply {
                request_id,
                question_id,
                message,
            } => {
                let row = desk_diagnose_core::future::boxed(|| {
                    task_for_parent_observed_on(&txn, &parent, &request_id, input_observation)
                })
                .await?;
                let after_seq = if parent.trigger_origin
                    == desk_diagnose_core::session::TriggerOrigin::SubAgentCompletion
                {
                    if parent.has_workbench_report_notification() {
                        Some(
                            tools::report_question_sequence(&parent, &request_id, &question_id)
                                .and_then(|seq| seq.checked_sub(1))
                                .ok_or_else(invalid)?,
                        )
                    } else {
                        let wait = parent.ready_subagent_wait.as_ref().ok_or_else(invalid)?;
                        if !wait.tasks.iter().any(|task| {
                            task.task_id == request_id
                                && i64::try_from(task.input_revision).ok()
                                    == Some(row.input_revision)
                                && i64::try_from(task.control_revision).ok()
                                    == Some(row.control_revision)
                        }) {
                            return Err(invalid());
                        }
                        Some(
                            wait.report_wait
                                .reply_cursor(&request_id)
                                .ok_or_else(invalid)?,
                        )
                    }
                } else {
                    None
                };
                let reply = desk_diagnose_core::future::boxed(|| {
                    super::task_control::reply_to_question_on(
                        &txn,
                        &row,
                        &question_id,
                        &message,
                        Some(&labels[0]),
                        after_seq,
                        now,
                    )
                })
                .await?;
                labels.extend(task_labels(&row)?);
                serde_json::to_value(reply).map_err(|_| invalid())?
            }
            Operation::Report(_) | Operation::WaitDevice { .. } => return Err(invalid()),
            Operation::AcknowledgeReports(report_ids) => {
                let turn_id = parent.current_turn_id.as_deref().ok_or_else(invalid)?;
                let payload = serde_json::json!({"acknowledged_report_ids": report_ids});
                desk_diagnose_core::future::boxed(|| {
                    desk_assistant_data_store::worker_report::settle_projected_on(
                        &txn,
                        &group,
                        report_ids,
                        turn_id,
                        now.timestamp_millis(),
                    )
                })
                .await?;
                payload
            }
            Operation::ReadReports {
                request_id,
                after_seq,
                limit,
            } => {
                let row = desk_diagnose_core::future::boxed(|| {
                    task_for_parent_observed_on(&txn, &parent, &request_id, input_observation)
                })
                .await?;
                let run = decode_run(&row)?;
                let destination = serde_json::from_str(&model_binding).map_err(|_| invalid())?;
                let (page, terminal) = desk_diagnose_core::future::boxed(|| {
                    desk_assistant_data_store::worker_report::read_report_page_on(
                        &txn,
                        &run,
                        desk_assistant_data_store::worker_report::ReportPageRead {
                            after_seq,
                            limit,
                            byte_limit: group.limits.max_context_bytes.min(64 * 1024),
                            destination: &destination,
                            now_ms: now.timestamp_millis(),
                        },
                        &mut labels,
                    )
                })
                .await?;
                if !page.acceptance_criteria.is_empty() {
                    labels.extend(task_labels(&row)?);
                }
                if let Some(report_id) = terminal {
                    observation = Some(ObservedResult {
                        task_id: request_id,
                        report_id,
                        state_revision: run.state_revision,
                        tool_call_id: call.id.clone(),
                        result_message_id: result_message_id.into(),
                        parent_input_revision: parent.input_revision,
                        parent_control_revision: parent.control_revision,
                    });
                }
                serde_json::to_value(page).map_err(|_| invalid())?
            }
            Operation::WaitReports(report_wait) => {
                if now.timestamp_millis() >= group.limits.deadline_ms {
                    input_observation.permission(PermissionOutcome::PolicyRejected);
                }
                if parent.subagent_wait.is_some()
                    || (parent.ready_subagent_wait.is_some()
                        && parent.trigger_origin
                            != desk_diagnose_core::session::TriggerOrigin::SubAgentCompletion)
                    || now.timestamp_millis() >= group.limits.deadline_ms
                {
                    if now.timestamp_millis() < group.limits.deadline_ms {
                        input_observation.reject(Stage::Preflight, InputIssue::Precondition);
                    }
                    return Err(invalid());
                }
                let destination = serde_json::from_str(&model_binding).map_err(|_| invalid())?;
                let mut current = Vec::new();
                let mut summaries = Vec::new();
                let mut reports = Vec::new();
                for cursor in &report_wait.requests {
                    let row = desk_diagnose_core::future::boxed(|| {
                        task_for_parent_observed_on(
                            &txn,
                            &parent,
                            &cursor.request_id,
                            input_observation,
                        )
                    })
                    .await?;
                    let run = decode_run(&row)?;
                    labels.extend(task_labels(&row)?);
                    if let Some(report) = desk_diagnose_core::future::boxed(|| {
                        desk_assistant_data_store::worker_report::first_matching_report_on(
                            &txn,
                            &run,
                            cursor.after_seq,
                            &report_wait.kinds,
                            &destination,
                            now.timestamp_millis(),
                        )
                    })
                    .await?
                    {
                        reports.push(report);
                    }
                    summaries.push(run.summary());
                    current.push((
                        TaskFence {
                            task_id: cursor.request_id.clone(),
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
                    tasks: current.iter().map(|(fence, _)| fence.clone()).collect(),
                    report_wait,
                };
                wait.validate().map_err(|_| invalid())?;
                let ready = wait.evaluate_reports(
                    parent.input_revision,
                    parent.control_revision,
                    &current,
                    !reports.is_empty(),
                ) == WaitEvaluation::Ready;
                let references = if ready {
                    reports
                        .into_iter()
                        .map(|stored| {
                            labels.push(stored.envelope);
                            stored.report.wake_reference()
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let payload = serde_json::json!({"wait_id": wait.wait_id, "status": if ready {"ready"} else {"waiting"},
                    "tasks": summaries, "reports": references,
                    "rule": "Waiting releases model capacity; completion does not grant new device authority."});
                next.ready_subagent_wait = None;
                next.ready_subagent_notification = None;
                if !ready {
                    next.subagent_wait = Some(wait);
                }
                payload
            }
        };
        if matches!(
            call.name.as_str(),
            tools::STATUS | tools::REPORTS | tools::REPLY | tools::WAIT_REPORTS
        ) {
            input_observation.stage(Stage::Reference, StageOutcome::Passed);
        }
        input_observation.stage(Stage::Preflight, StageOutcome::Passed);
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
        let version = desk_diagnose_core::future::boxed(|| async {
            write_child_session_on(&txn, &next, now).await
        })
        .await?;
        desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
        permission_ends.submit(
            now.timestamp_millis(),
            crate::model_metrics::runtime::submit,
        );
        next.version = version;
        *held = next;
        Ok(ToolReceipt {
            payload,
            result_message_id: result_message_id.into(),
        })
    }
}
