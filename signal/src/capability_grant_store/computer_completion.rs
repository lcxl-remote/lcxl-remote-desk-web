//! Original Computer Action observations and terminal receipts on existing work.

use super::computer_binding::{ComputerBinding, original_on, validate_binding};
use super::*;
use crate::remote_tool_edge::completion::{Projection, project};
use desk_agent_protocol::computer_use::ComputerActionCompleted;
use desk_diagnose_core::{
    action_result::ActionResultReceipt,
    seam::{ExecOutcome, ToolRunOutput, WaitOutcome},
    session::{ActionIdentity, WorkKind},
};
use sea_orm::DatabaseTransaction;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    native: ComputerActionCompleted,
    observed_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Terminal {
    observation: Observation,
    projection: Projection,
    receipt: ActionResultReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema_version: u16,
    binding_sha256: String,
    unknown: Option<Observation>,
    terminal: Option<Terminal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionObservation {
    Stored,
    Unknown,
    Duplicate,
    InlineOrLegacy,
    Stale,
}

pub(crate) struct OriginalResult {
    pub work: agent_action_item::Model,
    pub original_call_id: String,
    pub output: ToolRunOutput,
    pub receipt: ActionResultReceipt,
    pub outcome: CapabilityDispatchOutcome,
    pub native_result: desk_agent_protocol::computer_use::ComputerActionResultClass,
}

impl OriginalResult {
    pub(crate) fn into_exec(self) -> ExecOutcome {
        let event_id = Some(self.work.completion_event_id);
        let data_envelope = Some(self.receipt.envelope);
        match self.outcome {
            CapabilityDispatchOutcome::Succeeded => ExecOutcome::Executed {
                output: self.output,
                event_id,
                data_envelope,
            },
            CapabilityDispatchOutcome::Failed => ExecOutcome::Failed {
                output: self.output,
                event_id,
                data_envelope,
            },
        }
    }

    pub(crate) fn into_wait(self) -> WaitOutcome {
        match self.outcome {
            CapabilityDispatchOutcome::Succeeded => WaitOutcome::CompletedWithReceipt {
                action: self.receipt.action,
                original_call_id: self.original_call_id,
                output: self.output,
                event_id: self.work.completion_event_id,
                data_envelope: self.receipt.envelope,
            },
            CapabilityDispatchOutcome::Failed => WaitOutcome::FailedWithReceipt {
                action: self.receipt.action,
                original_call_id: self.original_call_id,
                output: self.output,
                event_id: self.work.completion_event_id,
                data_envelope: self.receipt.envelope,
            },
        }
    }
}

fn invalid() -> DbErr {
    DbErr::Custom("invalid original Computer Action completion".into())
}

fn identity(
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
) -> ActionIdentity {
    ActionIdentity::new(
        work.id,
        &payload.call_id,
        &payload.dispatch_id,
        WorkKind::ComputerAction,
    )
}

fn binding(
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
) -> Result<ComputerBinding, DbErr> {
    let value: ComputerBinding = serde_json::from_str(
        outbox
            .computer_binding_json
            .as_deref()
            .ok_or_else(invalid)?,
    )
    .map_err(|_| invalid())?;
    validate_binding(&value, outbox, work, payload)?;
    if !work.is_side_effecting
        || work.completion_event_id != stable_id("capability-completion", &payload.call_id)
        || !matches!(
            work.completion_delivery_state.as_str(),
            "pending" | "consumed"
        )
    {
        return Err(invalid());
    }
    Ok(value)
}

fn project_on(
    binding: &ComputerBinding,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
    native: &ComputerActionCompleted,
) -> Result<Option<Projection>, DbErr> {
    project(
        &binding.plan,
        &payload.tool_name,
        &work.conversation_id,
        &payload.canonical_input_json,
        native,
    )
    .map_err(|_| invalid())
}

fn decode(
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
    binding: &ComputerBinding,
) -> Result<Record, DbErr> {
    if work.result_schema_version != Some(2) {
        return Err(invalid());
    }
    let json = work.result_json.as_deref().ok_or_else(invalid)?;
    if json.len() > 512 * 1024 {
        return Err(invalid());
    }
    let record: Record = serde_json::from_str(json).map_err(|_| invalid())?;
    if record.schema_version != 2
        || record.binding_sha256
            != format!(
                "{:x}",
                Sha256::digest(
                    outbox
                        .computer_binding_json
                        .as_deref()
                        .ok_or_else(invalid)?
                        .as_bytes()
                )
            )
    {
        return Err(invalid());
    }
    if let Some(unknown) = &record.unknown {
        if unknown.observed_at_unix_ms == 0
            || project_on(binding, work, payload, &unknown.native)?.is_some()
        {
            return Err(invalid());
        }
    }
    if let Some(terminal) = &record.terminal {
        if terminal.observation.observed_at_unix_ms == 0
            || outbox.state != DISPATCH_OUTBOX_COMPLETED
            || work.status
                != match terminal.projection.outcome {
                    CapabilityDispatchOutcome::Succeeded => CAPABILITY_WORK_SUCCEEDED,
                    CapabilityDispatchOutcome::Failed => CAPABILITY_WORK_FAILED,
                }
            || project_on(binding, work, payload, &terminal.observation.native)?.as_ref()
                != Some(&terminal.projection)
        {
            return Err(invalid());
        }
        let output = ToolRunOutput {
            content: terminal.projection.content.clone(),
            image_data_url: None,
        };
        terminal
            .receipt
            .validate_for(&binding.origin, identity(work, payload), 1, &output)
            .map_err(|_| invalid())?;
        if terminal.receipt.received_at_unix_ms != terminal.observation.observed_at_unix_ms {
            return Err(invalid());
        }
    } else if record.unknown.is_none()
        || work.status != CAPABILITY_WORK_OUTCOME_UNKNOWN
        || outbox.state != DISPATCH_OUTBOX_OUTCOME_UNKNOWN
    {
        return Err(invalid());
    }
    Ok(record)
}

pub(super) fn terminal_result(
    outbox: &agent_capability_dispatch_outbox::Model,
    work: agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
) -> Result<Option<OriginalResult>, DbErr> {
    let binding = binding(outbox, &work, payload)?;
    if work.result_json.is_none() {
        return Ok(None);
    }
    let Some(terminal) = decode(outbox, &work, payload, &binding)?.terminal else {
        return Ok(None);
    };
    Ok(Some(OriginalResult {
        work,
        original_call_id: binding.origin.tool_call_id,
        output: ToolRunOutput {
            content: terminal.projection.content,
            image_data_url: None,
        },
        receipt: terminal.receipt,
        outcome: terminal.projection.outcome,
        native_result: terminal.observation.native.result,
    }))
}

impl SignalCapabilityGrantStore {
    pub(crate) async fn wait_computer_result(
        &self,
        action: &str,
        generation: &str,
        run_id: &str,
        actor_id: &str,
        device_id: &str,
    ) -> Result<Option<WaitOutcome>, DbErr> {
        let txn = self.db.begin().await?;
        let result = async {
            let Some(outbox) = agent_capability_dispatch_outbox::Entity::find()
                .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(generation))
                .one(&txn)
                .await?
            else {
                return Ok(None);
            };
            if outbox.computer_binding_json.is_none() {
                return Ok(None);
            }
            let (outbox, work, payload) = original_on(&txn, generation).await?;
            if work.conversation_id != run_id
                || work.actor_id != actor_id
                || work.target_device_id != device_id
                || work.action_request_id != action
            {
                return Err(invalid());
            }
            let binding = binding(&outbox, &work, &payload)?;
            let identity = identity(&work, &payload);
            let running = super::computer_background::task_on(
                &txn,
                &work,
                u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?,
            )
            .await?
            .is_some_and(|task| {
                matches!(
                    task.state,
                    desk_diagnose_core::dynamic_run::BackgroundTaskState::Running
                        | desk_diagnose_core::dynamic_run::BackgroundTaskState::CancelRequested
                )
            });
            Ok(Some(match terminal_result(&outbox, work, &payload)? {
                Some(original) => original.into_wait(),
                None if running => WaitOutcome::StillRunning,
                None => WaitOutcome::UnknownWithIdentity {
                    action: identity,
                    original_call_id: binding.origin.tool_call_id,
                },
            }))
        }
        .await;
        txn.rollback().await?;
        result
    }

    pub(crate) async fn accept_computer_completion(
        &self,
        connection: &str,
        audience: &str,
        frame: &str,
        completed: &ComputerActionCompleted,
    ) -> Result<CompletionObservation, DbErr> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(SQLITE_COMPLETION_BUSY_BUDGET_MS);
        let mut delay = SQLITE_COMPLETION_BUSY_INITIAL_DELAY_MS;
        loop {
            let txn = self.db.begin().await?;
            let result = self
                .accept_computer_completion_on(&txn, connection, audience, frame, completed)
                .await;
            let result = match result {
                Ok(result) => txn.commit().await.map(|_| result),
                Err(error) => {
                    txn.rollback().await.ok();
                    Err(error)
                }
            };
            match result {
                Err(error)
                    if retryable_sqlite_write_contention(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(
                        std::time::Duration::from_millis(delay)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                    )
                    .await;
                    delay = delay
                        .saturating_mul(2)
                        .min(SQLITE_COMPLETION_BUSY_MAX_DELAY_MS);
                }
                other => return other,
            }
        }
    }

    async fn accept_computer_completion_on(
        &self,
        txn: &DatabaseTransaction,
        connection: &str,
        audience: &str,
        frame: &str,
        completed: &ComputerActionCompleted,
    ) -> Result<CompletionObservation, DbErr> {
        if frame != completed.execution_generation {
            return Err(invalid());
        }
        let (outbox, work, payload) = original_on(txn, frame).await?;
        if completed.work_id != work.id.to_string()
            || completed.action_request_id != payload.call_id
            || audience != work.target_device_id
        {
            return Err(invalid());
        }
        if outbox.computer_binding_json.is_none() {
            // Pre-binding work and the two inline browser reads cannot acquire
            // new provenance retrospectively. Their existing waiter still checks
            // its own connection and handles the original inline completion.
            return Ok(CompletionObservation::InlineOrLegacy);
        }
        let binding = binding(&outbox, &work, &payload)?;
        if binding.connection_id != connection || binding.plan.device_id != audience {
            return Err(invalid());
        }
        let projection = project_on(&binding, &work, &payload, completed)?;
        if work.result_json.is_some() && work.result_schema_version != Some(2) {
            return Ok(CompletionObservation::Stale);
        }
        let mut record = if work.result_json.is_some() {
            decode(&outbox, &work, &payload, &binding)?
        } else {
            Record {
                schema_version: 2,
                binding_sha256: format!(
                    "{:x}",
                    Sha256::digest(
                        outbox
                            .computer_binding_json
                            .as_deref()
                            .ok_or_else(invalid)?
                            .as_bytes()
                    )
                ),
                unknown: None,
                terminal: None,
            }
        };
        if let Some(terminal) = &record.terminal {
            return Ok(if terminal.observation.native == *completed {
                CompletionObservation::Duplicate
            } else {
                CompletionObservation::Stale
            });
        }
        if projection.is_none()
            && let Some(unknown) = &record.unknown
        {
            return Ok(if unknown.native == *completed {
                CompletionObservation::Duplicate
            } else {
                CompletionObservation::Stale
            });
        }
        if !matches!(
            outbox.state.as_str(),
            DISPATCH_OUTBOX_SENDING | DISPATCH_OUTBOX_OUTCOME_UNKNOWN
        ) || !matches!(
            work.status.as_str(),
            CAPABILITY_WORK_DISPATCHING | CAPABILITY_WORK_OUTCOME_UNKNOWN
        ) {
            return Ok(CompletionObservation::Stale);
        }
        let now = Utc::now();
        let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
        let observation = Observation {
            native: completed.clone(),
            observed_at_unix_ms: now_ms,
        };
        let (work_status, outbox_status, result) = if let Some(projection) = projection {
            let receipt = binding
                .origin
                .receipt(
                    identity(&work, &payload),
                    1,
                    now_ms,
                    &ToolRunOutput {
                        content: projection.content.clone(),
                        image_data_url: None,
                    },
                )
                .map_err(|_| invalid())?;
            let status = match projection.outcome {
                CapabilityDispatchOutcome::Succeeded => CAPABILITY_WORK_SUCCEEDED,
                CapabilityDispatchOutcome::Failed => CAPABILITY_WORK_FAILED,
            };
            record.terminal = Some(Terminal {
                observation,
                projection,
                receipt,
            });
            (
                status,
                DISPATCH_OUTBOX_COMPLETED,
                CompletionObservation::Stored,
            )
        } else {
            record.unknown = Some(observation);
            (
                CAPABILITY_WORK_OUTCOME_UNKNOWN,
                DISPATCH_OUTBOX_OUTCOME_UNKNOWN,
                CompletionObservation::Unknown,
            )
        };
        let json = serde_json::to_string(&record).map_err(|_| invalid())?;
        if json.len() > 512 * 1024 {
            return Err(invalid());
        }
        let changed = agent_action_item::Entity::update_many()
            .filter(agent_action_item::Column::Id.eq(work.id))
            .filter(agent_action_item::Column::Status.eq(&work.status))
            .col_expr(agent_action_item::Column::ResultJson, Expr::value(json))
            .col_expr(
                agent_action_item::Column::ResultSchemaVersion,
                Expr::value(2),
            )
            .col_expr(agent_action_item::Column::Status, Expr::value(work_status))
            .col_expr(
                agent_action_item::Column::Resolution,
                Expr::value(work_status),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .exec(txn)
            .await?;
        let updated = agent_capability_dispatch_outbox::Entity::update_many()
            .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
            .filter(agent_capability_dispatch_outbox::Column::State.eq(&outbox.state))
            .col_expr(
                agent_capability_dispatch_outbox::Column::State,
                Expr::value(outbox_status),
            )
            .col_expr(
                agent_capability_dispatch_outbox::Column::UpdatedAt,
                Expr::value(now),
            )
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 || updated.rows_affected != 1 {
            return Err(invalid());
        }
        #[cfg(test)]
        pause_crash_fixture_before_commit("computer_completion_before_commit");
        Ok(result)
    }

    pub(crate) async fn read_computer_result(
        &self,
        generation: &str,
        run_id: &str,
        actor_id: &str,
        device_id: &str,
    ) -> Result<Option<OriginalResult>, DbErr> {
        let txn = self.db.begin().await?;
        let result = async {
            let (outbox, work, payload) = original_on(&txn, generation).await?;
            if work.conversation_id != run_id
                || work.actor_id != actor_id
                || work.target_device_id != device_id
            {
                return Err(invalid());
            }
            terminal_result(&outbox, work, &payload)
        }
        .await;
        txn.rollback().await?;
        result
    }

    pub(crate) async fn consume_computer_result(
        &self,
        event: &str,
        run_id: &str,
        actor_id: &str,
        device_id: &str,
    ) -> Result<bool, DbErr> {
        let txn = self.db.begin().await?;
        let result = async {
            let Some(row) = agent_action_item::Entity::find()
                .filter(agent_action_item::Column::CompletionEventId.eq(event))
                .one(&txn)
                .await?
            else {
                return Ok(false);
            };
            if row.kind != CAPABILITY_WORK_KIND || row.result_schema_version != Some(2) {
                return Ok(false);
            }
            let outbox = agent_capability_dispatch_outbox::Entity::find()
                .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(row.id))
                .one(&txn)
                .await?
                .ok_or_else(invalid)?;
            let (outbox, work, payload) = original_on(&txn, &outbox.dispatch_id).await?;
            if work.conversation_id != run_id
                || work.actor_id != actor_id
                || work.target_device_id != device_id
            {
                return Err(invalid());
            }
            if terminal_result(&outbox, work, &payload)?.is_none() {
                return Err(invalid());
            }
            agent_action_item::Entity::update_many()
                .filter(agent_action_item::Column::Id.eq(row.id))
                .filter(agent_action_item::Column::CompletionDeliveryState.eq("pending"))
                .col_expr(
                    agent_action_item::Column::CompletionDeliveryState,
                    Expr::value("consumed"),
                )
                .exec(&txn)
                .await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(value) => {
                txn.commit().await?;
                Ok(value)
            }
            Err(error) => {
                txn.rollback().await.ok();
                Err(error)
            }
        }
    }
}
