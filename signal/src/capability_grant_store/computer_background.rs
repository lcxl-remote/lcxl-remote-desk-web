//! Background presentation of original accepted work, never a second dispatch.

use super::computer_binding::{ComputerAcceptance, ComputerBinding, original_on, validate_binding};
use super::computer_completion::terminal_result;
use super::*;
use desk_agent_protocol::capability_provider::CapabilityTaskRef;
use desk_agent_protocol::computer_use::ComputerActionResultClass;
use desk_diagnose_core::dynamic_run::{
    BACKGROUND_TASK_SCHEMA_VERSION, BackgroundTaskRecord, BackgroundTaskState,
};
use sea_orm::{DatabaseTransaction, QueryOrder};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Promotion {
    schema_version: u16,
    binding_sha256: String,
    acceptance_sha256: String,
    pub promoted_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<super::computer_cancel::CancelIntent>,
}

fn invalid() -> DbErr {
    DbErr::Custom("invalid original Computer Action background task".into())
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub(super) fn bound(
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
) -> Result<ComputerBinding, DbErr> {
    let binding = serde_json::from_str(
        outbox
            .computer_binding_json
            .as_deref()
            .ok_or_else(invalid)?,
    )
    .map_err(|_| invalid())?;
    validate_binding(&binding, outbox, work, payload)?;
    Ok(binding)
}

/// Both deadlines are anchored to the original dispatch intent. Restarting or
/// presenting a task can only consume the remaining time, never renew it.
pub(super) fn deadlines(
    outbox: &agent_capability_dispatch_outbox::Model,
    binding: &ComputerBinding,
) -> Result<(u64, u64), DbErr> {
    let start = u64::try_from(outbox.created_at.timestamp_millis()).map_err(|_| invalid())?;
    let hard = start
        .checked_add(u64::from(binding.plan.timeout_ms))
        .ok_or_else(invalid)?
        .min(
            u64::try_from(
                chrono::DateTime::parse_from_rfc3339(&binding.plan.expires_at)
                    .map_err(|_| invalid())?
                    .timestamp_millis(),
            )
            .map_err(|_| invalid())?,
        );
    let budget = binding
        .execution
        .as_ref()
        .map_or(binding.plan.timeout_ms, |execution| {
            execution.foreground_budget_ms
        });
    Ok((
        start
            .checked_add(u64::from(budget))
            .ok_or_else(invalid)?
            .min(hard),
        hard,
    ))
}

fn acceptance(
    outbox: &agent_capability_dispatch_outbox::Model,
    binding: &ComputerBinding,
) -> Result<Option<ComputerAcceptance>, DbErr> {
    let Some(json) = &outbox.computer_acceptance_json else {
        return Ok(None);
    };
    let accepted: ComputerAcceptance = serde_json::from_str(json).map_err(|_| invalid())?;
    if accepted.schema_version != 1
        || accepted.accepted_at_unix_ms == 0
        || accepted.binding_sha256
            != digest(
                outbox
                    .computer_binding_json
                    .as_deref()
                    .ok_or_else(invalid)?,
            )
        || accepted.accepted_at_unix_ms >= deadlines(outbox, binding)?.1
    {
        return Err(invalid());
    }
    Ok(Some(accepted))
}

pub(crate) async fn task_on(
    txn: &DatabaseTransaction,
    work: &agent_action_item::Model,
    now_ms: u64,
) -> Result<Option<BackgroundTaskRecord>, DbErr> {
    let Some(outbox) = agent_capability_dispatch_outbox::Entity::find()
        .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(work.id))
        .one(txn)
        .await?
    else {
        return Ok(None);
    };
    let Some(json) = &outbox.computer_background_json else {
        return Ok(None);
    };
    let (outbox, original, payload) = original_on(txn, &outbox.dispatch_id).await?;
    if original != *work {
        return Err(invalid());
    }
    let binding = bound(&outbox, work, &payload)?;
    let execution = binding.execution.as_ref().ok_or_else(invalid)?;
    let accepted = acceptance(&outbox, &binding)?.ok_or_else(invalid)?;
    let promotion: Promotion = serde_json::from_str(json).map_err(|_| invalid())?;
    super::computer_cancel::validate_intent(&promotion.cancel, work, &promotion)?;
    let (foreground, hard) = deadlines(&outbox, &binding)?;
    if promotion.schema_version != 1
        || promotion.binding_sha256 != accepted.binding_sha256
        || promotion.acceptance_sha256
            != digest(
                outbox
                    .computer_acceptance_json
                    .as_deref()
                    .ok_or_else(invalid)?,
            )
        || promotion.promoted_at_unix_ms < foreground.max(accepted.accepted_at_unix_ms)
        || promotion.promoted_at_unix_ms >= hard
    {
        return Err(invalid());
    }
    let started_at = timestamp(accepted.accepted_at_unix_ms)?.to_rfc3339();
    let updated_at = work
        .updated_at
        .max(timestamp(promotion.promoted_at_unix_ms)?)
        .to_rfc3339();
    let mut record = BackgroundTaskRecord {
        schema_version: BACKGROUND_TASK_SCHEMA_VERSION,
        task: CapabilityTaskRef {
            task_id: payload.call_id.clone(),
            call_id: binding.origin.tool_call_id.clone(),
            run_id: work.conversation_id.clone(),
            provider_id: payload.provider_id.clone(),
            capability_id: payload.capability_id.clone(),
            input_revision: payload.input_revision,
            generation: payload.generation,
        },
        turn_id: work.turn_id.clone(),
        tool_name: payload.tool_name.clone(),
        canonical_input_digest_sha256: payload.canonical_input_digest_sha256.clone(),
        effect: execution.effect,
        execution_policy: execution.policy,
        supports_cancel: true,
        state: BackgroundTaskState::Running,
        progress_sequence: 0,
        started_at,
        updated_at,
        terminal_at: None,
        cancel_request_id: None,
        result_envelope_ids: vec![],
    };
    if let Some(result) = terminal_result(&outbox, original, &payload)? {
        record.state = match result.outcome {
            CapabilityDispatchOutcome::Succeeded => BackgroundTaskState::Succeeded,
            CapabilityDispatchOutcome::Failed
                if result.native_result == ComputerActionResultClass::PausedByUser =>
            {
                BackgroundTaskState::Cancelled
            }
            CapabilityDispatchOutcome::Failed => BackgroundTaskState::Failed,
        };
        record.result_envelope_ids = vec![result.receipt.envelope.envelope_id];
        record.progress_sequence = 3;
        record.terminal_at = Some(record.updated_at.clone());
    } else if work.status != CAPABILITY_WORK_DISPATCHING
        || work.manual_resolved_at.is_some()
        || now_ms >= hard
    {
        record.state = BackgroundTaskState::OutcomeUnknown;
        record.progress_sequence = 2;
        if now_ms >= hard {
            record.updated_at = timestamp(hard)?.to_rfc3339();
        }
        record.terminal_at = Some(record.updated_at.clone());
    } else if promotion.cancel.is_some() {
        record.state = BackgroundTaskState::CancelRequested;
        record.progress_sequence = 1;
    }
    record.cancel_request_id = promotion.cancel.map(|intent| intent.request_id);
    record.validate().map_err(|_| invalid())?;
    Ok(Some(record))
}

pub(crate) async fn list_on(
    txn: &DatabaseTransaction,
    run: &str,
    actor: &str,
    device: &str,
    now_ms: u64,
) -> Result<Vec<BackgroundTaskRecord>, DbErr> {
    let rows = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
        .filter(agent_action_item::Column::ConversationId.eq(run))
        .filter(agent_action_item::Column::ActorId.eq(actor))
        .filter(agent_action_item::Column::TargetDeviceId.eq(device))
        .order_by_asc(agent_action_item::Column::Id)
        .all(txn)
        .await?;
    let mut tasks = vec![];
    for row in rows {
        if let Some(task) = task_on(txn, &row, now_ms).await? {
            tasks.push(task);
        }
    }
    Ok(tasks)
}

impl SignalCapabilityGrantStore {
    pub(crate) async fn promote_computer_background(
        &self,
        generation: &str,
        run: &str,
        actor: &str,
        device: &str,
    ) -> Result<bool, DbErr> {
        let txn = self.db.begin().await?;
        let result = async {
            let (outbox, work, payload) = original_on(&txn, generation).await?;
            if work.conversation_id != run
                || work.actor_id != actor
                || work.target_device_id != device
            {
                return Err(invalid());
            }
            let binding = bound(&outbox, &work, &payload)?;
            if binding.execution.is_none() {
                return Ok(false);
            }
            let Some(accepted) = acceptance(&outbox, &binding)? else {
                return Ok(false);
            };
            let now = u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?;
            let (foreground, hard) = deadlines(&outbox, &binding)?;
            if outbox.state != DISPATCH_OUTBOX_SENDING
                || work.status != CAPABILITY_WORK_DISPATCHING
                || work.result_json.is_some()
                || work.manual_resolved_at.is_some()
                || work.cancel_requested_at.is_some()
                || now < foreground.max(accepted.accepted_at_unix_ms)
                || now >= hard
            {
                return Ok(false);
            }
            if outbox.computer_background_json.is_some() {
                task_on(&txn, &work, now).await?.ok_or_else(invalid)?;
            } else {
                let promotion = Promotion {
                    schema_version: 1,
                    binding_sha256: accepted.binding_sha256,
                    acceptance_sha256: digest(
                        outbox
                            .computer_acceptance_json
                            .as_deref()
                            .ok_or_else(invalid)?,
                    ),
                    promoted_at_unix_ms: now,
                    cancel: None,
                };
                let changed = agent_capability_dispatch_outbox::Entity::update_many()
                    .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
                    .filter(
                        agent_capability_dispatch_outbox::Column::ComputerBackgroundJson.is_null(),
                    )
                    .col_expr(
                        agent_capability_dispatch_outbox::Column::ComputerBackgroundJson,
                        Expr::value(serde_json::to_string(&promotion).map_err(|_| invalid())?),
                    )
                    .exec(&txn)
                    .await?;
                if changed.rows_affected != 1 {
                    return Err(invalid());
                }
            }
            Ok(true)
        }
        .await;
        match result {
            Ok(promoted) => {
                txn.commit().await?;
                Ok(promoted)
            }
            Err(error) => {
                txn.rollback().await?;
                Err(error)
            }
        }
    }

    pub(crate) async fn computer_foreground_remaining(
        &self,
        generation: &str,
    ) -> Result<std::time::Duration, DbErr> {
        let txn = self.db.begin().await?;
        let result = async {
            let (outbox, work, payload) = original_on(&txn, generation).await?;
            let binding = bound(&outbox, &work, &payload)?;
            let deadline = deadlines(&outbox, &binding)?.0;
            let now = u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?;
            Ok(std::time::Duration::from_millis(
                deadline.saturating_sub(now),
            ))
        }
        .await;
        txn.rollback().await?;
        result
    }
}
