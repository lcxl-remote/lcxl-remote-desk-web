//! Root-scoped child cancellation and adjustment with durable operation receipts.
use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_delegation_reservation as receipt_row;
use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantSubAgentControl, SubAgentControlAction,
};
use desk_diagnose_core::model_observability::{
    InputIssue, PermissionOutcome, Stage, StageOutcome, tool::ToolObservation,
};
use sea_orm::ActiveModelTrait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentControlOutcome {
    pub task: AiAssistantSubAgentSummary,
    pub cancel_request_id: Option<String>,
}

/// Only the business outcome is serialized into the immutable control receipt.
/// Permission updates remain local until the enclosing transaction commits.
pub(crate) struct PreparedControl {
    pub outcome: SubAgentControlOutcome,
    pub permission_ends: desk_diagnose_core::model_observability::permission::PendingEnds,
}

impl SubAgentStore {
    pub async fn control_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        request: &AiAssistantSubAgentControl,
    ) -> Result<SubAgentControlOutcome, DbErr> {
        validate_control(request)?;
        let txn = desk_diagnose_core::future::boxed(|| async {
            crate::db::begin_write(&self.db, session_row::Entity).await
        })
        .await?;

        let history = desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::native_scope_on(&txn, actor, device, root)
        })
        .await?;
        let parent = desk_diagnose_core::future::boxed(|| async {
            parent_on(&txn, root, actor, device).await
        })
        .await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let prepared = desk_diagnose_core::future::boxed(|| async {
            prepare_owner_control_on(&txn, &parent, request, now_ms).await
        })
        .await?;
        if let Some(history) = &history {
            desk_diagnose_core::future::boxed(|| history.capture_on(&txn)).await?;
        }
        desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
        prepared
            .permission_ends
            .submit(now_ms, crate::model_metrics::runtime::submit);
        Ok(prepared.outcome)
    }
}

fn validate_control(request: &AiAssistantSubAgentControl) -> Result<(), DbErr> {
    if !desk_diagnose_core::subagent::valid_id(&request.client_request_id)
        || !desk_diagnose_core::subagent::valid_id(&request.task_id)
        || request.expected_input_revision == 0
        || request.expected_control_revision == 0
        || request.expected_input_revision > i64::MAX as u64
        || request.expected_control_revision > i64::MAX as u64
        || match &request.action {
            SubAgentControlAction::Cancel => false,
            SubAgentControlAction::Reply {
                question_id,
                message,
            } => {
                !desk_diagnose_core::subagent::valid_id(question_id)
                    || message.trim().is_empty()
                    || message.len() > desk_diagnose_core::subagent::report::MAX_QUESTION_BYTES
            }
            SubAgentControlAction::Adjust {
                message,
                client_timezone,
            } => {
                !desk_diagnose_core::ai_assistant::source::valid_statement_timezone(client_timezone)
                    || message.trim().is_empty()
                    || message.chars().count()
                        > desk_diagnose_core::subagent::MAX_DELEGATED_TASK_CHARS
            }
        }
    {
        return Err(invalid());
    }
    Ok(())
}

/// The caller holds owner/root then child control. No model, device or network
/// call occurs in this transaction; observations and cancellation follow commit.
pub(crate) async fn prepare_owner_control_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    request: &AiAssistantSubAgentControl,
    now_ms: i64,
) -> Result<PreparedControl, DbErr> {
    apply_task_control_with_source_on(
        txn,
        parent,
        request,
        None,
        now_ms,
        &ToolObservation::default(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn apply_task_control_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    request: &AiAssistantSubAgentControl,
    now_ms: i64,
) -> Result<SubAgentControlOutcome, DbErr> {
    Ok(prepare_owner_control_on(txn, parent, request, now_ms)
        .await?
        .outcome)
}

/// Main-model controls retain the actual committed tool caller's restrictions;
/// they cannot pass through the authenticated human adjustment path.
pub(crate) async fn apply_task_control_from_turn_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    call: &desk_diagnose_core::chat::ToolCall,
    now_ms: i64,
    input_observation: &ToolObservation,
) -> Result<PreparedControl, DbErr> {
    use desk_diagnose_core::subagent::tools::{self, Operation};
    let operation =
        tools::parse_observed(parent, call, input_observation).map_err(|_| invalid())?;
    let (task_id, input, control, action) = match operation {
        Operation::Cancel {
            task_id,
            input_revision,
            control_revision,
        } => (
            task_id,
            input_revision,
            control_revision,
            SubAgentControlAction::Cancel,
        ),
        Operation::Adjust {
            task_id,
            input_revision,
            control_revision,
            message,
        } => (
            task_id,
            input_revision,
            control_revision,
            SubAgentControlAction::Adjust {
                message,
                client_timezone: String::new(),
            },
        ),
        _ => return Err(invalid()),
    };
    let caller = parent
        .conversation
        .iter()
        .find(|message| {
            message.role == desk_diagnose_core::chat::ChatRole::Assistant
                && message.turn_id == parent.current_turn_id
                && message.tool_calls.iter().any(|reference| {
                    reference.id == call.id
                        && reference.name == call.name
                        && desk_diagnose_core::subagent::tools::same_arguments(
                            &parent.ui_references,
                            &call.name,
                            &reference.arguments_json,
                            &call.arguments_json,
                        )
                })
        })
        .and_then(|message| message.data_envelope.clone())
        .ok_or_else(|| invalid())?;
    let id = format!("model-control-{:x}", Sha256::digest(call.id.as_bytes()));
    let request = AiAssistantSubAgentControl {
        client_request_id: id,
        task_id,
        expected_input_revision: input,
        expected_control_revision: control,
        action,
    };
    apply_task_control_with_source_on(
        txn,
        parent,
        &request,
        Some(caller),
        now_ms,
        input_observation,
    )
    .await
}

async fn apply_task_control_with_source_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    request: &AiAssistantSubAgentControl,
    adjustment_source: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
    now_ms: i64,
    input_observation: &ToolObservation,
) -> Result<PreparedControl, DbErr> {
    if !parent.agent_role.is_main() || parent.surface != AgentSessionSurface::AiAssistant {
        input_observation.permission(PermissionOutcome::PolicyRejected);
        return Err(invalid());
    }
    let reservation_id = desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::DelegationReservation,
        &(
            &parent.conversation_id,
            &parent.actor_id,
            &parent.device_id,
            "subagent_control",
            &request.client_request_id,
        ),
    );
    let arguments = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(request, &adjustment_source)).map_err(|_| invalid())?)
    );
    if let Some(receipt) = desk_diagnose_core::future::boxed(|| async {
        receipt_row::Entity::find()
            .filter(receipt_row::Column::ReservationId.eq(&reservation_id))
            .one(txn)
            .await
    })
    .await?
    {
        if receipt.root_conversation_id != parent.conversation_id
            || receipt.operation_kind != "subagent_control"
            || receipt.task_id.as_deref() != Some(request.task_id.as_str())
            || receipt.arguments_sha256 != arguments
            || receipt.state != "control_committed"
        {
            return Err(invalid());
        }
        return Ok(PreparedControl {
            outcome: serde_json::from_str(&receipt.reservation_json).map_err(|_| invalid())?,
            permission_ends: Default::default(),
        });
    }
    let row = desk_diagnose_core::future::boxed(|| async {
        run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&request.task_id))
            .filter(run_row::Column::RootConversationId.eq(&parent.conversation_id))
            .filter(run_row::Column::ActorId.eq(&parent.actor_id))
            .filter(run_row::Column::DeviceId.eq(&parent.device_id))
            .one(txn)
            .await
    })
    .await?
    .ok_or_else(|| {
        input_observation.reject(Stage::Reference, InputIssue::UnknownReference);
        invalid()
    })?;
    let mut run = decode_run(&row)?;
    if run.binding.input_revision != request.expected_input_revision
        || run.binding.control_revision != request.expected_control_revision
    {
        input_observation.reject(Stage::Reference, InputIssue::ReferenceExpired);
        return Err(if adjustment_source.is_some() {
            DbErr::Custom(desk_diagnose_core::subagent::CONTROL_OBSERVATION_CHANGED.into())
        } else {
            invalid()
        });
    }
    input_observation.stage(Stage::Reference, StageOutcome::Passed);
    let prepared = desk_diagnose_core::future::send_phase(|| async {
        if let SubAgentControlAction::Reply {
            question_id,
            message,
        } = &request.action
        {
            let now = chrono::DateTime::from_timestamp_millis(now_ms).ok_or_else(invalid)?;
            let task =
                reply_to_question_on(txn, &row, question_id, message, None, None, now).await?;
            return Ok(PreparedControl {
                outcome: SubAgentControlOutcome {
                    task,
                    cancel_request_id: None,
                },
                permission_ends: Default::default(),
            });
        }

        let group_row = desk_diagnose_core::future::boxed(|| async {
            group_row::Entity::find()
                .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
                .filter(group_row::Column::RootConversationId.eq(&parent.conversation_id))
                .filter(group_row::Column::ActorId.eq(&parent.actor_id))
                .filter(group_row::Column::DeviceId.eq(&parent.device_id))
                .one(txn)
                .await
        })
        .await?
        .ok_or_else(|| invalid())?;
        let group = current_group_on(txn, &group_row).await?;
        if group.source != run.binding.source {
            return Err(invalid());
        }
        let child_row = desk_diagnose_core::future::boxed(|| async {
            session_row::Entity::find()
                .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
                .filter(session_row::Column::ActorId.eq(&parent.actor_id))
                .filter(session_row::Column::DeviceId.eq(&parent.device_id))
                .one(txn)
                .await
        })
        .await?
        .ok_or_else(|| invalid())?;
        let child =
            PersistedAgentSession::decode_json(&child_row.state_json).map_err(|_| invalid())?;
        run.validate_session(&child).map_err(|_| invalid())?;
        let now = chrono::DateTime::from_timestamp_millis(now_ms)
            .ok_or_else(|| invalid())?
            .to_rfc3339();
        let mut cancellation = None;
        let changed = match &request.action {
            SubAgentControlAction::Reply { .. } => return Err(invalid()),
            SubAgentControlAction::Cancel => {
                let changed = run
                    .request_cancel(run.fence(), &now)
                    .map_err(|_| invalid())?;
                if changed {
                    desk_diagnose_core::future::boxed(|| async {
                        super::native_cancel::cancel_native_actions_on(
                            txn,
                            &child,
                            &reservation_id,
                            now_ms,
                        )
                        .await
                    })
                    .await?;
                    cancellation = child.current_request_id.clone();
                    run.settle_cancel(&now).map_err(|_| invalid())?;
                }
                changed
            }
            SubAgentControlAction::Adjust {
                message,
                client_timezone,
            } => {
                if adjustment_source.is_some()
                    && (run.state.is_terminal() || run.state == SubAgentState::Cancelling)
                {
                    input_observation.reject(Stage::Preflight, InputIssue::Precondition);
                    return Err(DbErr::Custom(
                        desk_diagnose_core::subagent::CONTROL_OBSERVATION_CHANGED.into(),
                    ));
                }
                if group.source_admission == SourceAdmission::Closed
                    || now_ms >= run.binding.deadline_ms
                    || group.source_epoch != run.binding.source_epoch
                    || group.source != run.binding.source
                {
                    input_observation.permission(PermissionOutcome::PolicyRejected);
                    return Err(invalid());
                }
                let criteria = run.binding.acceptance_criteria.clone();
                run.adjust(run.fence(), message.clone(), criteria, &now)
                    .map_err(|_| {
                        input_observation.reject(Stage::Preflight, InputIssue::Precondition);
                        invalid()
                    })?;
                let creation: desk_diagnose_core::subagent::creation::TaskCreationEnvelope =
                    serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
                let label = match &adjustment_source {
                    Some(label) => {
                        label.validate().map_err(|_| invalid())?;
                        if !label
                            .allowed_destinations
                            .contains(&creation.source.model_destination)
                        {
                            return Err(invalid());
                        }
                        label.clone()
                    }
                    None => desk_diagnose_core::model_message_labels::model_bound_user_message(
                        format!("owner-adjustment-{reservation_id}"),
                        message.clone(),
                        creation.source.model_destination.clone(),
                    )
                    .map_err(|_| invalid())?
                    .data_envelope
                    .ok_or_else(|| invalid())?,
                };
                let adjusted = creation
                    .adjusted(
                        &run.binding,
                        label,
                        &run.child_conversation_id,
                        if adjustment_source.is_none() {
                            Some((now_ms as u64, client_timezone.as_str()))
                        } else {
                            parent
                                .assistant_data
                                .as_ref()
                                .filter(|data| data.current_actor_input_seq > 0)
                                .map(|data| {
                                    (
                                        data.current_input_recorded_at_unix_ms,
                                        data.current_input_timezone.as_str(),
                                    )
                                })
                        },
                    )
                    .map_err(|_| invalid())?;
                let changed = run_row::Entity::update_many()
                    .set(run_row::ActiveModel {
                        creation_envelope_json: Set(
                            serde_json::to_string(&adjusted).map_err(|_| invalid())?
                        ),
                        ..Default::default()
                    })
                    .filter(run_row::Column::Id.eq(row.id))
                    .filter(run_row::Column::StateRevision.eq(row.state_revision))
                    .exec(txn)
                    .await?;
                if changed.rows_affected != 1 {
                    return Err(invalid());
                }
                cancellation = child.current_request_id.clone();
                true
            }
        };
        let permission_ends = if changed {
            desk_diagnose_core::future::boxed(|| async {
                replace_run_on(txn, &row, &run, now_ms).await
            })
            .await?;
            let prepared = desk_diagnose_core::future::boxed(|| async {
                synchronize_control_on(txn, &run, now_ms, false).await
            })
            .await?;
            desk_diagnose_core::future::boxed(|| async {
                append_state_event_on(txn, &group, &run, now_ms).await
            })
            .await?;
            prepared
        } else {
            Default::default()
        };
        let result = SubAgentControlOutcome {
            task: run.summary(),
            cancel_request_id: cancellation,
        };
        Ok(PreparedControl {
            outcome: result,
            permission_ends,
        })
    })
    .await?;
    receipt_row::ActiveModel {
        reservation_id: Set(reservation_id.clone()),
        root_conversation_id: Set(parent.conversation_id.clone()),
        group_id: Set(row.group_id.clone()),
        conversation_id: Set(parent.conversation_id.clone()),
        task_id: Set(Some(row.task_id.clone())),
        operation_kind: Set("subagent_control".into()),
        arguments_sha256: Set(arguments),
        source_epoch: Set(row.source_epoch),
        input_revision: Set(
            i64::try_from(prepared.outcome.task.input_revision).map_err(|_| invalid())?
        ),
        control_revision: Set(
            i64::try_from(prepared.outcome.task.control_revision).map_err(|_| invalid())?
        ),
        reservation_json: Set(serde_json::to_string(&prepared.outcome).map_err(|_| invalid())?),
        actual_json: Set(None),
        state: Set("control_committed".into()),
        version: Set(1),
        created_at: Set(now_ms),
        settled_at: Set(Some(now_ms)),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    Ok(prepared)
}

/// Both reply entry points hold the original root and Worker session controls.
pub(super) async fn reply_to_question_on(
    txn: &DatabaseTransaction,
    row: &run_row::Model,
    question_id: &str,
    message: &str,
    caller: Option<&desk_agent_protocol::data_lineage::DataEnvelope>,
    after_seq: Option<u64>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AiAssistantSubAgentSummary, DbErr> {
    use desk_diagnose_core::subagent::{creation::TaskCreationEnvelope, report::QUESTION_NOT_OPEN};

    let mut run = decode_run(row)?;
    if run.state != SubAgentState::WaitingInput
        || run.pending_question_id.as_deref() != Some(question_id)
    {
        return Err(DbErr::Custom(QUESTION_NOT_OPEN.into()));
    }
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&run.binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&run.actor_id))
        .filter(group_row::Column::DeviceId.eq(&run.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = super::current_group_on(txn, &source).await?;
    let now_ms = now.timestamp_millis();
    if group.source_admission != SourceAdmission::Open
        || group.source != run.binding.source
        || group.source_epoch != run.binding.source_epoch
        || now_ms >= run.binding.deadline_ms
        || now_ms >= group.limits.deadline_ms
    {
        return Err(invalid());
    }
    super::scheduled_source::current_scheduled_source_on(txn, &source).await?;
    let creation: TaskCreationEnvelope =
        serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
    creation
        .validate_task(&run.binding)
        .map_err(|_| invalid())?;
    if creation.source != decode_creation(&source)? {
        return Err(invalid());
    }
    let stored = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .filter(session_row::Column::ActorId.eq(&run.actor_id))
        .filter(session_row::Column::DeviceId.eq(&run.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut child =
        PersistedAgentSession::decode_boxed_json(&stored.state_json).map_err(|_| invalid())?;
    run.validate_session(&child).map_err(|_| invalid())?;
    if child.turn_state.is_active()
        || child.version != stored.version
        || i64::try_from(child.lease_token).ok() != Some(stored.lease_token)
        || child.delegated_owner_requirement.as_deref() != Some(&creation.source.owner_requirement)
    {
        return Err(invalid());
    }
    let question = desk_assistant_data_store::worker_report::read_open_question_on(
        txn,
        &run,
        question_id,
        now_ms,
    )
    .await?;
    if after_seq.is_some_and(|cursor| question.report.seq <= cursor) {
        return Err(DbErr::Custom(QUESTION_NOT_OPEN.into()));
    }
    let caller = match caller {
        Some(label) => label.clone(),
        None => desk_diagnose_core::model_message_labels::model_bound_user_message(
            format!("owner-reply-{question_id}"),
            message.into(),
            creation.source.model_destination.clone(),
        )
        .map_err(|_| invalid())?
        .data_envelope
        .ok_or_else(invalid)?,
    };
    let reply = desk_diagnose_core::subagent::projection::runtime_message(
        &format!("reply-{question_id}"),
        &serde_json::json!({
            "request_id": run.binding.task_id,
            "question_id": question_id,
            "question": question.report.content.question,
            "reply": message,
            "rule": "This reply supplies data for the original delegated request. Its original objective and permissions still apply.",
        }),
        &[question.envelope, caller],
    )
    .map_err(|_| invalid())?;
    let label = reply.data_envelope.as_ref().ok_or_else(invalid)?;
    if !label
        .allowed_destinations
        .contains(&creation.source.model_destination)
        || serde_json::to_vec(&reply).map_err(|_| invalid())?.len() as u64
            > group.limits.max_context_bytes
    {
        return Err(invalid());
    }
    child.conversation.push(reply);
    run.answer_question(question_id, &now.to_rfc3339())
        .map_err(|_| invalid())?;
    replace_run_on(txn, row, &run, now_ms).await?;
    write_child_session_on(txn, &child, now).await?;
    append_state_event_on(txn, &group, &run, now_ms).await?;
    Ok(run.summary())
}

#[cfg(test)]
mod client_timezone_tests {
    use super::*;

    #[test]
    fn owner_adjustments_require_a_valid_client_timezone() {
        let mut request = AiAssistantSubAgentControl {
            client_request_id: "input".into(),
            task_id: "task".into(),
            expected_input_revision: 1,
            expected_control_revision: 1,
            action: SubAgentControlAction::Adjust {
                message: "Check yesterday".into(),
                client_timezone: "Asia/Shanghai".into(),
            },
        };
        assert!(validate_control(&request).is_ok());
        for timezone in ["", "Bad/Timezone"] {
            if let SubAgentControlAction::Adjust {
                client_timezone, ..
            } = &mut request.action
            {
                *client_timezone = timezone.into();
            }
            assert!(validate_control(&request).is_err());
        }
    }
}
