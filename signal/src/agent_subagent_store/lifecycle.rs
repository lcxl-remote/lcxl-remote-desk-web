//! Child report settlement shares the session lease and commits its inbox atomically.
use super::*;
use desk_diagnose_core::{
    chat::ChatRole,
    session::TurnState,
    subagent::{
        creation::TaskCreationEnvelope,
        seam::ChildAdmission,
        state::{CompletionDisposition, PlanningFence},
    },
};
use sea_orm::DatabaseTransaction;
use sha2::Digest;

pub(crate) async fn child_records_on(
    txn: &DatabaseTransaction,
    held: &PersistedAgentSession,
    now_ms: i64,
) -> Result<(run_row::Model, SubAgentRun, DelegationGroup), DbErr> {
    let binding = held.agent_role.binding().ok_or_else(invalid)?;
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(run_row::Column::ChildConversationId.eq(&held.conversation_id))
        .filter(run_row::Column::ActorId.eq(&held.actor_id))
        .filter(run_row::Column::DeviceId.eq(&held.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let run = decode_run(&row)?;
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&held.actor_id))
        .filter(group_row::Column::DeviceId.eq(&held.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&source)?;
    let creation: TaskCreationEnvelope =
        serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
    creation
        .validate_task(&run.binding)
        .map_err(|_| invalid())?;
    if creation.source != decode_creation(&source)?
        || held.delegated_owner_requirement.as_ref() != Some(&creation.source.owner_requirement)
    {
        return Err(invalid());
    }
    let stored = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&held.conversation_id))
        .filter(session_row::Column::ActorId.eq(&held.actor_id))
        .filter(session_row::Column::DeviceId.eq(&held.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let current = PersistedAgentSession::decode_json(&stored.state_json).map_err(|_| invalid())?;
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
    Ok((row, run, group))
}

async fn begin_child_control(
    db: &DatabaseConnection,
    held: &PersistedAgentSession,
) -> Result<DatabaseTransaction, DbErr> {
    let binding = held.agent_role.binding().ok_or_else(invalid)?;
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

impl SubAgentStore {
    pub async fn settle_turn_for_task(
        &self,
        held: &mut PersistedAgentSession,
        failure_reason: Option<&str>,
        allow_continue: bool,
    ) -> Result<(), DbErr> {
        if !held.turn_state.is_settled() {
            return Err(invalid());
        }
        let txn = begin_child_control(&self.db, held).await?;
        let now = chrono::Utc::now();
        let (row, mut run, group) = child_records_on(&txn, held, now.timestamp_millis()).await?;
        let facts = super::facts::runtime_facts_on(&txn, held, now.timestamp_millis()).await?;
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
        replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
        if resource_wait && !run.state.is_terminal() {
            run_row::Entity::update_many()
                .set(run_row::ActiveModel {
                    next_attempt_at_ms: Set(Some(
                        now.timestamp_millis()
                            .checked_add(30_000)
                            .ok_or_else(invalid)?,
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
        let version = write_child_session_on(&txn, &next, now).await?;
        append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
        txn.commit().await?;
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
        let text = serde_json::to_string(&payload).map_err(|_| invalid())?;
        let id = format!("child-state-{:x}", sha2::Sha256::digest(text.as_bytes()));
        let mut sources = vec![creation.instruction.data_envelope.ok_or_else(invalid)?];
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
        let projection = desk_diagnose_core::subagent::projection::runtime_message_for_history(
            &id,
            &payload,
            &sources,
            held,
            &creation.source.model_destination,
            u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?,
        )
        .map_err(|_| invalid())?;
        if serde_json::to_vec(&projection)
            .map_err(|_| invalid())?
            .len() as u64
            > group.limits.max_context_bytes
        {
            return Err(invalid());
        }
        txn.commit().await?;
        Ok(projection)
    }

    pub async fn child_admission(
        &self,
        held: &PersistedAgentSession,
    ) -> Result<ChildAdmission, DbErr> {
        let txn = begin_child_control(&self.db, held).await?;
        let binding = held.agent_role.binding().ok_or_else(invalid)?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&binding.task_id))
            .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
            .filter(run_row::Column::ActorId.eq(&held.actor_id))
            .filter(run_row::Column::DeviceId.eq(&held.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let run = decode_run(&row)?;
        let source = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&binding.group_id))
            .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
            .filter(group_row::Column::ActorId.eq(&held.actor_id))
            .filter(group_row::Column::DeviceId.eq(&held.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let group = decode_group(&source)?;
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
        let txn = begin_child_control(&self.db, held).await?;
        let now = chrono::Utc::now();
        let (row, mut run, group) = child_records_on(&txn, held, now.timestamp_millis()).await?;
        if now.timestamp_millis() >= run.binding.deadline_ms
            || now.timestamp_millis() >= group.limits.deadline_ms
        {
            return Err(invalid());
        }
        let facts = super::facts::runtime_facts_on(&txn, held, now.timestamp_millis()).await?;
        run.synchronize_dependencies(facts.dependencies, false, &now.to_rfc3339())
            .map_err(|_| invalid())?;
        let report = match desk_diagnose_core::subagent::report::from_answer(
            &answer_text,
            !run.dependencies.is_empty(),
            &facts.completion,
        ) {
            Ok(report) => report,
            Err(_) => {
                txn.rollback().await?;
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
                txn.rollback().await?;
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
            .ok_or_else(invalid)?;
        if answer.text != answer_text {
            return Err(invalid());
        }
        let mut sources = creation.input_envelopes;
        sources.push(answer.data_envelope.clone().ok_or_else(invalid)?);
        let text = serde_json::to_string(&report).map_err(|_| invalid())?;
        if text.len() as u64 > group.limits.max_result_bytes {
            return Err(invalid());
        }
        let id = format!(
            "{}-report:{}:{}",
            held.conversation_id, held.input_revision, held.control_revision
        );
        let envelope = desk_diagnose_core::subagent::projection::envelope(
            &id,
            &text,
            "delegated_task_report",
            &sources,
        )
        .map_err(|_| invalid())?;
        run.report_corrections_used = held.subagent_report_corrections_used;
        replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
        let changed = run_row::Entity::update_many()
            .set(run_row::ActiveModel {
                result_envelope_json: Set(Some(
                    serde_json::to_string(&envelope).map_err(|_| invalid())?,
                )),
                ..Default::default()
            })
            .filter(run_row::Column::Id.eq(row.id))
            .filter(
                run_row::Column::StateRevision
                    .eq(i64::try_from(run.state_revision).map_err(|_| invalid())?),
            )
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        let mut next = held.clone();
        if run.state.is_terminal() {
            next.pending_auto_triggers.clear();
        }
        next.turn_state = TurnState::Idle;
        let version = write_child_session_on(&txn, &next, now).await?;
        append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
        txn.commit().await?;
        next.version = version;
        *held = next;
        Ok(Ok(disposition))
    }
}
