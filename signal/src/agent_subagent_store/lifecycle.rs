//! Child report settlement shares the session lease and commits its inbox atomically.
use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_diagnose_core::{
    chat::ChatRole,
    session::TurnState,
    subagent::{
        creation::TaskCreationEnvelope,
        report::ReportSubmission,
        seam::ChildAdmission,
        state::{CompletionDisposition, PlanningFence},
    },
};

pub(crate) async fn child_records_on(
    txn: &DatabaseTransaction,
    held: &PersistedAgentSession,
    now_ms: i64,
) -> Result<(run_row::Model, SubAgentRun, DelegationGroup), DbErr> {
    let (row, run, group, _) = child_records_with_session_on(txn, held, now_ms).await?;
    Ok((row, run, group))
}

async fn child_records_with_session_on(
    txn: &DatabaseTransaction,
    held: &PersistedAgentSession,
    now_ms: i64,
) -> Result<
    (
        run_row::Model,
        SubAgentRun,
        DelegationGroup,
        Box<PersistedAgentSession>,
    ),
    DbErr,
> {
    let binding = held.agent_role.binding().ok_or_else(|| invalid())?;
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(run_row::Column::ChildConversationId.eq(&held.conversation_id))
        .filter(run_row::Column::ActorId.eq(&held.actor_id))
        .filter(run_row::Column::DeviceId.eq(&held.device_id))
        .one(txn)
        .await?
        .ok_or_else(|| invalid())?;
    let run = decode_run(&row)?;
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&held.actor_id))
        .filter(group_row::Column::DeviceId.eq(&held.device_id))
        .one(txn)
        .await?
        .ok_or_else(|| invalid())?;
    let group = current_group_on(txn, &source).await?;
    let creation: TaskCreationEnvelope =
        serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
    creation
        .validate_task(&run.binding)
        .map_err(|_| invalid())?;
    if creation.source != decode_creation(&source)?
        || held.delegated_owner_requirement.as_deref() != Some(&creation.source.owner_requirement)
    {
        return Err(invalid());
    }
    let stored = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&held.conversation_id))
        .filter(session_row::Column::ActorId.eq(&held.actor_id))
        .filter(session_row::Column::DeviceId.eq(&held.device_id))
        .one(txn)
        .await?
        .ok_or_else(|| invalid())?;
    let current =
        PersistedAgentSession::decode_boxed_json(&stored.state_json).map_err(|_| invalid())?;
    if run.binding != *binding
        || run.child_conversation_id != held.conversation_id
        || current.agent_role != held.agent_role
        || current.input_revision != held.input_revision
        || current.delegated_owner_requirement != held.delegated_owner_requirement
        || current.control_revision != held.control_revision
        || current.current_turn_id != held.current_turn_id
        || current.response_locale != creation.response_locale
        || held.response_locale != creation.response_locale
        || current.version != stored.version
        || current.version != held.version
        || current.lease_token != held.lease_token
        || i64::try_from(held.lease_token).ok() != Some(stored.lease_token)
        || !current.turn_state.is_active()
        || current.current_turn_id.is_none()
        || stored
            .lease_deadline
            .is_none_or(|deadline| deadline.timestamp_millis() < now_ms)
        || group.source != binding.source
        || group.source_epoch != binding.source_epoch
        || group.source_admission != SourceAdmission::Open
        || run.require_current(run.fence()).is_err()
        || held.subagent_report_corrections_used < run.report_corrections_used
        || held.subagent_report_corrections_used > 1
    {
        return Err(invalid());
    }
    Ok((row, run, *group, current))
}

async fn begin_child_control(
    db: &DatabaseConnection,
    held: &PersistedAgentSession,
) -> Result<DatabaseTransaction, DbErr> {
    let binding = held.agent_role.binding().ok_or_else(|| invalid())?;
    let txn = crate::db::begin_write(db, session_row::Entity).await?;

    parent_on(
        &txn,
        &binding.root_conversation_id,
        &held.actor_id,
        &held.device_id,
    )
    .await?;
    Ok(txn)
}

async fn previous_worker_report_on(
    txn: &DatabaseTransaction,
    row: &run_row::Model,
    run: &SubAgentRun,
    held: &PersistedAgentSession,
    destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
    now_ms: i64,
) -> Result<Option<desk_diagnose_core::subagent::report::StoredReport>, DbErr> {
    use crate::entity::agent_file_recovery_cleanup as tombstone;
    use desk_assistant_data_store::{device_roster, worker_report, worker_request};
    use sea_orm::QueryTrait;

    let retained = || {
        let deleted = tombstone::Entity::find()
            .select_only()
            .column(tombstone::Column::ConversationId)
            .into_query();
        session_row::Entity::find()
            .filter(session_row::Column::ConversationId.not_in_subquery(deleted))
            .filter(session_row::Column::ActorId.eq(&row.actor_id))
            .filter(session_row::Column::DeviceId.eq(&row.device_id))
            .select_only()
            .column(session_row::Column::ConversationId)
            .into_query()
    };
    let candidates = worker_request::previous_requests(row)
        .filter(run_row::Column::RootConversationId.in_subquery(retained()))
        .filter(run_row::Column::ChildConversationId.in_subquery(retained()))
        .all(txn)
        .await?;
    for candidate in candidates {
        let previous = decode_run(&candidate)?;
        if !worker_request::shares_history_scope(&previous.binding, &run.binding) {
            continue;
        }
        let Some(report) =
            worker_report::retained_terminal_report_on(txn, &previous, destination, now_ms).await?
        else {
            continue;
        };
        if !previous.binding.targets.is_empty() {
            let live = device_roster::current_request_devices(txn, held)
                .await
                .map_err(|_| invalid())?;
            let devices: Vec<_> = previous
                .binding
                .targets
                .into_iter()
                .map(|target| target.device_id)
                .collect();
            if !devices.iter().all(|device| live.contains(device))
                || !super::notification::retained_devices_are_eligible_on(txn, &devices).await?
            {
                continue;
            }
        }
        return Ok(Some(report));
    }
    Ok(None)
}

impl SubAgentStore {
    pub async fn wait_for_device_for_turn(
        &self,
        held: &mut PersistedAgentSession,
        call: &desk_diagnose_core::chat::ToolCall,
        target_ref: String,
        result_message_id: &str,
    ) -> Result<desk_diagnose_core::subagent::seam::ToolReceipt, DbErr> {
        use desk_diagnose_core::subagent::{
            device_wait::{self, DeviceWaitStatus},
            seam::ToolReceipt,
            state::TaskDependency,
            tools,
        };
        if tools::parse(held, call).map_err(|_| invalid())?
            != (tools::Operation::WaitDevice {
                target_ref: target_ref.clone(),
            })
        {
            return Err(invalid());
        }
        let txn = begin_child_control(&self.db, held).await?;
        let now = chrono::Utc::now();
        let (row, mut run, group, current) =
            child_records_with_session_on(&txn, held, now.timestamp_millis()).await?;
        if now.timestamp_millis() >= run.binding.deadline_ms
            || now.timestamp_millis() >= group.limits.deadline_ms
        {
            return Err(invalid());
        }
        let target = run
            .binding
            .targets
            .iter()
            .find(|target| target.reference == target_ref)
            .ok_or_else(invalid)?;
        let waits = super::resources::device_wait_authorized_on(&txn, &current, target).await?;
        if waits {
            let facts =
                super::facts::runtime_facts_on(&txn, &current, now.timestamp_millis()).await?;
            if !facts.dependencies.is_empty()
                || !facts.completion.incomplete_action_ids.is_empty()
                || !current.execution_state.states().is_empty()
                || !current.pending_auto_triggers.is_empty()
                || current
                    .permission_requests
                    .iter()
                    .any(|request| !request.state.is_terminal())
            {
                return Err(DbErr::Custom(device_wait::NATIVE_WORK_PENDING.into()));
            }
        }
        let status = if waits {
            DeviceWaitStatus::Waiting
        } else {
            DeviceWaitStatus::Unavailable
        };
        let message = device_wait::receipt(&current, call, result_message_id, &target_ref, status)
            .map_err(|_| invalid())?;
        let payload = serde_json::from_str(&message.text).map_err(|_| invalid())?;
        let mut next = held.clone_boxed();
        next.conversation.push(message);
        if waits {
            run.set_dependencies(
                vec![TaskDependency::Device {
                    target_ref,
                    result_message_id: result_message_id.into(),
                }],
                &now.to_rfc3339(),
            )
            .map_err(|_| invalid())?;
            replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
            next.handled_input_seq = next.latest_input_seq;
            next.finish_turn(TurnState::Idle, now.to_rfc3339());
            append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
        }
        next.version = write_child_session_on(&txn, &next, now).await?;
        let history = crate::assistant_data_store::native_scope_on(
            &txn,
            &held.actor_id,
            &held.device_id,
            &run.binding.root_conversation_id,
        )
        .await?;
        if let Some(history) = history {
            history.capture_on(&txn).await?;
        }
        txn.commit().await?;
        *held = *next;
        Ok(ToolReceipt {
            payload,
            result_message_id: result_message_id.into(),
        })
    }

    pub async fn report_for_turn(
        &self,
        held: &mut PersistedAgentSession,
        call: &desk_diagnose_core::chat::ToolCall,
        content: desk_diagnose_core::subagent::report::ReportContent,
        result_message_id: &str,
    ) -> Result<desk_diagnose_core::subagent::seam::ToolReceipt, DbErr> {
        use desk_diagnose_core::{
            chat::ChatMessage,
            subagent::{
                report::{ReportKind, TERMINAL_REPORT_BLOCKED, from_terminal_submission},
                seam::ToolReceipt,
                tools,
            },
        };

        if tools::parse(held, call).map_err(|_| invalid())?
            != tools::Operation::Report(content.clone())
            || !desk_diagnose_core::subagent::valid_id(result_message_id)
        {
            return Err(invalid());
        }
        let txn = begin_child_control(&self.db, held).await?;
        let now = chrono::Utc::now();
        let (row, mut run, group, current) =
            child_records_with_session_on(&txn, held, now.timestamp_millis()).await?;
        if now.timestamp_millis() >= run.binding.deadline_ms
            || now.timestamp_millis() >= group.limits.deadline_ms
        {
            return Err(invalid());
        }
        let caller = current
            .conversation
            .iter()
            .find(|message| {
                message.role == ChatRole::Assistant
                    && message.turn_id == current.current_turn_id
                    && message.tool_calls.iter().any(|reference| {
                        reference.id == call.id
                            && reference.name == call.name
                            && tools::same_arguments(
                                &current.ui_references,
                                &call.name,
                                &reference.arguments_json,
                                &call.arguments_json,
                            )
                    })
            })
            .ok_or_else(|| invalid())?;
        if let Some(receipt) = current.conversation.iter().find(|message| {
            message.role == ChatRole::Tool
                && message.tool_call_id.as_deref() == Some(call.id.as_str())
        }) {
            if receipt
                .data_envelope
                .as_ref()
                .is_none_or(|label| label.provenance.source_tool_name != call.name)
            {
                return Err(invalid());
            }
            let receipt = ToolReceipt {
                payload: serde_json::from_str(&receipt.text).map_err(|_| invalid())?,
                result_message_id: receipt.message_id.clone(),
            };
            txn.commit().await?;
            return Ok(receipt);
        }
        let creation: TaskCreationEnvelope =
            serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
        let mut sources = creation.input_envelopes;
        sources.push(caller.data_envelope.clone().ok_or_else(|| invalid())?);
        let yields = content.kind != ReportKind::Progress;
        let facts = if yields || !content.evidence_refs.is_empty() {
            Some(super::facts::runtime_facts_on(&txn, &current, now.timestamp_millis()).await?)
        } else {
            None
        };
        if let Some(facts) = &facts {
            for reference in &content.evidence_refs {
                if !facts.completion.available_evidence_ids.contains(reference) {
                    return Err(invalid());
                }
                let label = facts
                    .result_envelopes
                    .iter()
                    .chain(
                        current
                            .conversation
                            .iter()
                            .filter_map(|message| message.data_envelope.as_ref()),
                    )
                    .find(|label| &label.envelope_id == reference)
                    .ok_or_else(|| invalid())?;
                sources.push(label.clone());
            }
        }
        if yields
            && (caller.tool_calls.len() != 1
                || current.unclosed_tool_call_ids().as_slice() != [call.id.as_str()])
        {
            return Err(invalid());
        }
        if content.kind == ReportKind::NeedsInput {
            let facts = facts.as_ref().ok_or_else(invalid)?;
            run.synchronize_dependencies(facts.dependencies.clone(), true, &now.to_rfc3339())
                .map_err(|_| invalid())?;
        }
        let terminal_report = if let Some(facts) = &facts
            && content.kind.is_terminal()
        {
            let unsettled = || DbErr::Custom(TERMINAL_REPORT_BLOCKED.into());
            if !facts.dependencies.is_empty() || !current.execution_state.states().is_empty() {
                return Err(unsettled());
            }
            let terminal =
                from_terminal_submission(&content, &facts.completion).map_err(|error| {
                    if error.message == TERMINAL_REPORT_BLOCKED {
                        unsettled()
                    } else {
                        invalid()
                    }
                })?;
            run.synchronize_dependencies(Vec::new(), true, &now.to_rfc3339())
                .map_err(|_| invalid())?;
            run.settle_report(
                run.fence(),
                terminal.clone(),
                &facts.completion,
                &now.to_rfc3339(),
            )
            .map_err(|_| unsettled())?;
            sources.extend(facts.result_envelopes.iter().cloned());
            let text = serde_json::to_string(&terminal).map_err(|_| invalid())?;
            if text.len() as u64 > group.limits.max_result_bytes {
                return Err(invalid());
            }
            replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
            Some(terminal)
        } else {
            None
        };
        let report = desk_assistant_data_store::worker_report::append_report_on(
            &txn,
            &group,
            &run,
            ReportSubmission {
                generation: held.lease_token,
                origin: ("tool", &call.id),
                model_destination: &creation.source.model_destination,
                content,
                receipt_refs: terminal_report
                    .as_ref()
                    .map(|report| report.receipt_refs.clone())
                    .unwrap_or_default(),
            },
            &sources,
            now.timestamp_millis(),
        )
        .await?;
        let mut payload = serde_json::json!({"report_id": report.report_id, "seq": report.seq});
        if let Some(question_id) = report.question_id {
            payload["question_id"] = serde_json::json!(question_id);
            run.ask_question(question_id, &now.to_rfc3339())
                .map_err(|_| invalid())?;
            replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
        }
        let text = payload.to_string();
        let mut message = ChatMessage::tool_result(result_message_id, &call.id, &text);
        message.data_envelope = Some(
            desk_diagnose_core::subagent::projection::envelope(
                result_message_id,
                &text,
                &call.name,
                &sources,
            )
            .map_err(|_| invalid())?,
        );
        let mut next = held.clone();
        if next
            .conversation
            .iter()
            .any(|message| message.message_id == result_message_id)
        {
            return Err(invalid());
        }
        message.tool_ok = Some(true);
        next.conversation.push(message);
        if terminal_report.is_some() {
            desk_diagnose_core::conversation_attachment::model_read::release_consumed(
                &mut next.conversation,
            )
            .map_err(|_| invalid())?;
            next.pending_auto_triggers.clear();
        }
        if yields {
            next.handled_input_seq = next.latest_input_seq;
            next.finish_turn(TurnState::Idle, now.to_rfc3339());
        }
        let version = write_child_session_on(&txn, &next, now).await?;
        if yields {
            append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
        }
        let history = crate::assistant_data_store::native_scope_on(
            &txn,
            &held.actor_id,
            &held.device_id,
            &run.binding.root_conversation_id,
        )
        .await?;
        if let Some(history) = history {
            history.capture_on(&txn).await?;
        }
        txn.commit().await?;
        next.version = version;
        *held = next;
        Ok(ToolReceipt {
            payload,
            result_message_id: result_message_id.into(),
        })
    }

    pub async fn settle_turn_for_task(
        &self,
        held: &mut PersistedAgentSession,
        failure_reason: Option<&str>,
        allow_continue: bool,
    ) -> Result<(), DbErr> {
        if !held.turn_state.is_settled() {
            return Err(invalid());
        }
        let txn = desk_diagnose_core::future::boxed(|| async {
            begin_child_control(&self.db, held).await
        })
        .await?;
        let binding = held.agent_role.binding().ok_or_else(|| invalid())?;
        let history = desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::native_scope_on(
                &txn,
                &held.actor_id,
                &held.device_id,
                &binding.root_conversation_id,
            )
        })
        .await?;
        let now = chrono::Utc::now();
        let (row, mut run, group) = desk_diagnose_core::future::boxed(|| async {
            child_records_on(&txn, held, now.timestamp_millis()).await
        })
        .await?;
        let facts = desk_diagnose_core::future::boxed(|| async {
            super::facts::runtime_facts_on(&txn, held, now.timestamp_millis()).await
        })
        .await?;
        run.synchronize_dependencies(facts.dependencies, false, &now.to_rfc3339())
            .map_err(|_| invalid())?;
        let resource_wait = failure_reason == Some("delegated_model_unavailable")
            && run.dependencies.is_empty()
            && held.unclosed_tool_call_ids().is_empty()
            && held.execution_state.states().is_empty();
        if resource_wait {
            run.set_dependencies(
                vec![
                    desk_diagnose_core::subagent::state::TaskDependency::Resource {
                        reason: desk_diagnose_core::subagent::SubAgentWaitReason::ModelCapacity,
                    },
                ],
                &now.to_rfc3339(),
            )
            .map_err(|_| invalid())?;
        }
        let deadline_reached = now.timestamp_millis() >= run.binding.deadline_ms
            || now.timestamp_millis() >= group.limits.deadline_ms;
        let failure = if deadline_reached {
            Some("delegation_deadline_reached")
        } else if failure_reason.is_some() && !resource_wait {
            failure_reason
        } else if run.dependencies.is_empty() && !allow_continue {
            Some("delegated_turn_has_no_final_answer")
        } else {
            None
        };
        let mut next = held.clone();
        if let Some(reason) = failure {
            run.fail(reason, &now.to_rfc3339()).map_err(|_| invalid())?;
            next.finish_turn(TurnState::Failed, now.to_rfc3339());
            next.pending_auto_triggers.clear();
        }
        run.report_corrections_used = held.subagent_report_corrections_used;
        desk_diagnose_core::future::boxed(|| async {
            replace_run_on(&txn, &row, &run, now.timestamp_millis()).await
        })
        .await?;
        if resource_wait && !run.state.is_terminal() {
            run_row::Entity::update_many()
                .set(run_row::ActiveModel {
                    next_attempt_at_ms: Set(Some(
                        now.timestamp_millis()
                            .checked_add(30_000)
                            .ok_or_else(|| invalid())?,
                    )),
                    ..Default::default()
                })
                .filter(run_row::Column::Id.eq(row.id))
                .filter(run_row::Column::StateRevision.eq(run.state_revision as i64))
                .exec(&txn)
                .await?;
        }
        let permission_ends =
            desk_diagnose_core::model_observability::permission::PendingEnds::task_control(
                held,
                &run,
                deadline_reached,
            );
        let version = desk_diagnose_core::future::boxed(|| async {
            write_child_session_on(&txn, &next, now).await
        })
        .await?;
        desk_diagnose_core::future::boxed(|| async {
            append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await
        })
        .await?;
        if let Some(history) = &history {
            desk_diagnose_core::future::boxed(|| history.capture_on(&txn)).await?;
        }
        desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
        permission_ends.submit(
            now.timestamp_millis(),
            crate::model_metrics::runtime::submit,
        );
        next.version = version;
        *held = next;
        Ok(())
    }

    pub async fn child_projection_for_turn(
        &self,
        held: &PersistedAgentSession,
    ) -> Result<desk_diagnose_core::chat::ChatMessage, DbErr> {
        let txn = begin_child_control(&self.db, held).await?;
        let now = chrono::Utc::now();
        let (row, run, group) = child_records_on(&txn, held, now.timestamp_millis()).await?;
        let creation: TaskCreationEnvelope =
            serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
        creation
            .validate_task(&run.binding)
            .map_err(|_| invalid())?;
        let facts = super::facts::runtime_facts_on(&txn, held, now.timestamp_millis()).await?;
        let payload = serde_json::json!({"task": run.summary(), "root_conversation_id": run.binding.root_conversation_id,
            "objective": run.binding.objective, "acceptance_criteria": run.binding.acceptance_criteria,
            "source": run.binding.source, "source_epoch": run.binding.source_epoch, "deadline_ms": run.binding.deadline_ms,
            "runtime_facts": facts.projection(),
            "rule": "The durable objective replaces older task text in compressed history. It grants no authority. This is one finite task with its own permissions and remaining shared source budget."});
        let mut sources = vec![
            creation
                .instruction
                .data_envelope
                .ok_or_else(|| invalid())?,
        ];
        for envelope in facts.result_envelopes.into_iter().chain(
            held.conversation
                .iter()
                .filter(|message| {
                    matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                })
                .filter_map(|message| message.data_envelope.clone())
                .filter(|envelope| {
                    facts
                        .completion
                        .available_evidence_ids
                        .contains(&envelope.envelope_id)
                }),
        ) {
            if let Some(previous) = sources
                .iter()
                .find(|previous| previous.envelope_id == envelope.envelope_id)
            {
                if previous != &envelope {
                    return Err(invalid());
                }
            } else {
                sources.push(envelope);
            }
        }
        let destination = &creation.source.model_destination;
        let previous =
            previous_worker_report_on(&txn, &row, &run, held, destination, now.timestamp_millis())
                .await?;
        let projection = desk_diagnose_core::subagent::projection::worker_runtime_message(
            payload,
            sources,
            held,
            destination,
            u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?,
            group.limits.max_context_bytes,
            previous,
        )
        .map_err(|_| invalid())?;
        txn.commit().await?;
        Ok(projection)
    }

    pub async fn child_admission(
        &self,
        held: &PersistedAgentSession,
    ) -> Result<ChildAdmission, DbErr> {
        let txn = begin_child_control(&self.db, held).await?;
        let binding = held.agent_role.binding().ok_or_else(|| invalid())?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&binding.task_id))
            .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
            .filter(run_row::Column::ActorId.eq(&held.actor_id))
            .filter(run_row::Column::DeviceId.eq(&held.device_id))
            .one(&txn)
            .await?
            .ok_or_else(|| invalid())?;
        let run = decode_run(&row)?;
        let source = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&binding.group_id))
            .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
            .filter(group_row::Column::ActorId.eq(&held.actor_id))
            .filter(group_row::Column::DeviceId.eq(&held.device_id))
            .one(&txn)
            .await?
            .ok_or_else(|| invalid())?;
        let group = current_group_on(&txn, &source).await?;
        if run.binding.task_id != binding.task_id
            || run.binding.group_id != binding.group_id
            || run.binding.root_conversation_id != binding.root_conversation_id
            || run.binding.source != binding.source
            || run.child_conversation_id != held.conversation_id
            || run.binding.input_revision != binding.input_revision
            || run.binding.control_revision != binding.control_revision
            || group.source != binding.source
            || group.source_epoch != run.binding.source_epoch
            || run.state.is_terminal()
            || run.state == SubAgentState::Cancelling
        {
            return Err(invalid());
        }
        if run.source_paused && group.source_admission == SourceAdmission::Paused {
            txn.rollback().await?;
            return Ok(ChildAdmission::SourcePaused);
        }
        let now = chrono::Utc::now().timestamp_millis();
        child_records_on(&txn, held, now).await?;
        if now >= binding.deadline_ms || now >= group.limits.deadline_ms {
            return Err(invalid());
        }
        txn.rollback().await?;
        Ok(ChildAdmission::Admitted)
    }

    pub async fn evaluate_answer_for_turn(
        &self,
        held: &PersistedAgentSession,
        expected: PlanningFence,
        answer: &str,
    ) -> Result<Result<CompletionDisposition, &'static str>, DbErr> {
        let txn = begin_child_control(&self.db, held).await?;
        let now = chrono::Utc::now();
        let (_, mut run, group) = child_records_on(&txn, held, now.timestamp_millis()).await?;
        if now.timestamp_millis() >= run.binding.deadline_ms
            || now.timestamp_millis() >= group.limits.deadline_ms
        {
            return Err(invalid());
        }
        let facts = super::facts::runtime_facts_on(&txn, held, now.timestamp_millis()).await?;
        run.synchronize_dependencies(facts.dependencies, false, &now.to_rfc3339())
            .map_err(|_| invalid())?;
        let result = match desk_diagnose_core::subagent::report::from_answer(
            answer,
            !run.dependencies.is_empty(),
            &facts.completion,
        ) {
            Ok(report)
                if serde_json::to_vec(&report).map_err(|_| invalid())?.len() as u64
                    <= group.limits.max_result_bytes =>
            {
                run.evaluate_report(expected, &report, &facts.completion)
            }
            _ => Err("delegated text result exceeds bounds or has invalid runtime facts"),
        };
        txn.commit().await?;
        Ok(result)
    }

    pub async fn settle_answer_for_turn(
        &self,
        held: &mut PersistedAgentSession,
        expected: PlanningFence,
        answer_text: String,
    ) -> Result<Result<CompletionDisposition, &'static str>, DbErr> {
        if !held.turn_state.is_settled() {
            return Err(invalid());
        }
        let txn = desk_diagnose_core::future::boxed(|| async {
            begin_child_control(&self.db, held).await
        })
        .await?;
        let binding = held.agent_role.binding().ok_or_else(|| invalid())?;
        let history = desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::native_scope_on(
                &txn,
                &held.actor_id,
                &held.device_id,
                &binding.root_conversation_id,
            )
        })
        .await?;
        let now = chrono::Utc::now();
        let (row, mut run, group) = desk_diagnose_core::future::boxed(|| async {
            child_records_on(&txn, held, now.timestamp_millis()).await
        })
        .await?;
        if now.timestamp_millis() >= run.binding.deadline_ms
            || now.timestamp_millis() >= group.limits.deadline_ms
        {
            return Err(invalid());
        }
        let facts = desk_diagnose_core::future::boxed(|| async {
            super::facts::runtime_facts_on(&txn, held, now.timestamp_millis()).await
        })
        .await?;
        run.synchronize_dependencies(facts.dependencies, false, &now.to_rfc3339())
            .map_err(|_| invalid())?;
        let report = match desk_diagnose_core::subagent::report::from_answer(
            &answer_text,
            !run.dependencies.is_empty(),
            &facts.completion,
        ) {
            Ok(report) => report,
            Err(_) => {
                desk_diagnose_core::future::boxed(|| async { txn.rollback().await }).await?;
                return Ok(Err("invalid delegated text result"));
            }
        };
        let disposition = match run.settle_report(
            expected,
            report.clone(),
            &facts.completion,
            &now.to_rfc3339(),
        ) {
            Ok(disposition) => disposition,
            Err(error) => {
                desk_diagnose_core::future::boxed(|| async { txn.rollback().await }).await?;
                return Ok(Err(error));
            }
        };
        let creation: TaskCreationEnvelope =
            serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
        creation
            .validate_task(&run.binding)
            .map_err(|_| invalid())?;
        let answer = held
            .conversation
            .iter()
            .rev()
            .find(|message| {
                message.role == ChatRole::Assistant
                    && message.turn_id == held.current_turn_id
                    && message.tool_calls.is_empty()
            })
            .ok_or_else(|| invalid())?;
        if answer.text != answer_text {
            return Err(invalid());
        }
        let mut sources = creation.input_envelopes;
        sources.push(answer.data_envelope.clone().ok_or_else(|| invalid())?);
        let text = serde_json::to_string(&report).map_err(|_| invalid())?;
        if text.len() as u64 > group.limits.max_result_bytes {
            return Err(invalid());
        }
        run.report_corrections_used = held.subagent_report_corrections_used;
        desk_diagnose_core::future::boxed(|| {
            replace_run_on(&txn, &row, &run, now.timestamp_millis())
        })
        .await?;
        desk_diagnose_core::future::boxed(|| async {
            use desk_diagnose_core::subagent::report::{ReportContent, ReportKind};
            let kind = match disposition {
                CompletionDisposition::Complete => ReportKind::Completed,
                CompletionDisposition::Unable => ReportKind::Failed,
                CompletionDisposition::Waiting => ReportKind::Progress,
            };
            desk_assistant_data_store::worker_report::append_report_on(
                &txn,
                &group,
                &run,
                ReportSubmission {
                    generation: held.lease_token,
                    origin: ("answer", &answer.message_id),
                    model_destination: &creation.source.model_destination,
                    content: ReportContent {
                        kind,
                        summary: report.summary,
                        question: None,
                        evidence_refs: report.evidence_refs,
                    },
                    receipt_refs: report.receipt_refs,
                },
                &sources,
                now.timestamp_millis(),
            )
            .await
        })
        .await?;
        let mut next = held.clone();
        if run.state.is_terminal() {
            next.pending_auto_triggers.clear();
        }
        next.turn_state = TurnState::Idle;
        let version = desk_diagnose_core::future::boxed(|| async {
            write_child_session_on(&txn, &next, now).await
        })
        .await?;
        desk_diagnose_core::future::boxed(|| async {
            append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await
        })
        .await?;
        if let Some(history) = &history {
            desk_diagnose_core::future::boxed(|| history.capture_on(&txn)).await?;
        }
        desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
        next.version = version;
        *held = next;
        Ok(Ok(disposition))
    }
}
