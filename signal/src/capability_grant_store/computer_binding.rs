//! Identity and provenance frozen on the original, already-claimed transport.
//! Neither a binding nor acceptance is a new dispatch, grant, or task record.

use super::*;
use desk_agent_protocol::capability_provider::{CapabilityEffect, ExecutionPolicy};
use desk_agent_protocol::computer_use::{ComputerActionStarted, SealedComputerActionPlan};
use desk_diagnose_core::{action_result::ActionResultOrigin, chat::ToolCall};
use sea_orm::DatabaseTransaction;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComputerExecutionContract {
    pub effect: CapabilityEffect,
    pub policy: ExecutionPolicy,
    pub foreground_budget_ms: u32,
    pub chain_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComputerBinding {
    pub schema_version: u16,
    pub connection_id: String,
    pub dispatch_sha256: String,
    pub plan: SealedComputerActionPlan,
    pub origin: ActionResultOrigin,
    /// Absent on older bindings; never inferred retrospectively for promotion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ComputerExecutionContract>,
    /// Original user-selected model sink; execution grants never imply export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_export: Option<super::computer_export::ComputerExportContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComputerAcceptance {
    pub schema_version: u16,
    pub binding_sha256: String,
    pub accepted_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptanceOutcome {
    Stored,
    Duplicate,
    Legacy,
    InlineObservation,
    Stale,
}

fn invalid() -> DbErr {
    DbErr::Custom("invalid original Computer Action binding".into())
}

pub(super) async fn original_on(
    txn: &DatabaseTransaction,
    generation: &str,
) -> Result<
    (
        agent_capability_dispatch_outbox::Model,
        agent_action_item::Model,
        CapabilityDispatchPayload,
    ),
    DbErr,
> {
    let outbox = agent_capability_dispatch_outbox::Entity::find()
        .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(generation))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let payload: CapabilityDispatchPayload =
        serde_json::from_str(&outbox.payload_json).map_err(|_| invalid())?;
    let work = agent_action_item::Entity::find_by_id(outbox.work_id)
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let prepared: PreparedCapabilityPayload =
        serde_json::from_str(&work.payload_json).map_err(|_| invalid())?;
    if outbox.payload_schema_version != 1
        || work.payload_schema_version != 1
        || work.kind != CAPABILITY_WORK_KIND
        || outbox.dispatch_id != payload.dispatch_id
        || payload.dispatch_id != generation
        || outbox.work_id != payload.work_id
        || payload.work_id != work.id
        || outbox.call_id != payload.call_id
        || payload.call_id != work.action_request_id
        || work.tool_call_id != payload.call_id
        || outbox.reservation_id != payload.reservation_id
        || u64::try_from(outbox.generation).ok() != Some(payload.generation)
        || payload.generation == 0
        || work.draft_hash != payload.canonical_input_digest_sha256
        || work.dispatched_attempt != Some(1)
        || work.dispatch_intent_at.is_none()
        || work.execution_id.as_deref()
            != Some(format!("capability:{}:{}", payload.call_id, payload.generation).as_str())
        || prepared.grant_id != payload.grant_id
        || prepared.reservation_id != payload.reservation_id
        || prepared.call_id != payload.call_id
        || prepared.generation != payload.generation
        || prepared.input_revision != payload.input_revision
        || prepared.input_watermark != payload.input_watermark
        || prepared.canonical_input_json != payload.canonical_input_json
        || prepared.canonical_input_digest_sha256 != payload.canonical_input_digest_sha256
        || prepared.provider_id != payload.provider_id
        || prepared.capability_id != payload.capability_id
        || prepared.tool_name != payload.tool_name
        || format!(
            "{:x}",
            Sha256::digest(payload.canonical_input_json.as_bytes())
        ) != payload.canonical_input_digest_sha256
    {
        return Err(invalid());
    }
    let reservation = agent_grant_reservation::Entity::find()
        .filter(agent_grant_reservation::Column::ReservationId.eq(&outbox.reservation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if reservation.work_id != work.id
        || reservation.call_id != payload.call_id
        || reservation.run_id != work.conversation_id
        || reservation.grant_id != payload.grant_id
        || reservation.canonical_input_digest_sha256 != payload.canonical_input_digest_sha256
        || reservation.state != RESERVATION_STATUS_COMMITTED
        || u64::try_from(reservation.generation).ok() != Some(payload.generation)
    {
        return Err(invalid());
    }
    Ok((outbox, work, payload))
}

pub(super) fn validate_binding(
    binding: &ComputerBinding,
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
) -> Result<(), DbErr> {
    binding.plan.validate().map_err(|_| invalid())?;
    binding.origin.validate().map_err(|_| invalid())?;
    let plan = &binding.plan;
    let origin = &binding.origin;
    if let Some(export) = &binding.model_export {
        export.validate()?;
    }
    if let Some(execution) = &binding.execution {
        execution
            .policy
            .validate(plan.timeout_ms)
            .map_err(|_| invalid())?;
        if execution.effect.is_side_effecting() != work.is_side_effecting
            || execution.foreground_budget_ms == 0
            || execution.foreground_budget_ms > 8_000
            || execution.chain_id.len() > 256
            || execution.chain_id.chars().any(char::is_control)
            || matches!(execution.policy, ExecutionPolicy::InlineOnly)
            || matches!(execution.policy, ExecutionPolicy::Adaptive { foreground_budget_ms }
                if foreground_budget_ms != execution.foreground_budget_ms)
        {
            return Err(invalid());
        }
    }
    if binding.schema_version != 1
        || binding.connection_id.trim().is_empty()
        || binding.dispatch_sha256
            != format!("{:x}", Sha256::digest(outbox.payload_json.as_bytes()))
        || plan.work_id != work.id.to_string()
        || plan.action_request_id != payload.call_id
        || plan.execution_generation != outbox.dispatch_id
        || plan.device_id != work.target_device_id
        || plan.approved_actor_id != work.actor_id
        || plan.approval_id != payload.grant_id
        || plan.draft_hash != payload.canonical_input_digest_sha256
        || origin.turn_fence.conversation_id != work.conversation_id
        || origin.turn_fence.turn_id != work.turn_id
        || origin.turn_fence.actor_id != work.actor_id
        || origin.turn_fence.device_id != work.target_device_id
        || origin.turn_fence.input_revision != payload.input_revision
        || origin.provider_id != payload.provider_id
        || origin.tool_name != payload.tool_name
        || stable_id(
            "capability-call",
            &format!(
                "{}:{}:{}",
                work.conversation_id, work.turn_id, origin.tool_call_id
            ),
        ) != payload.call_id
    {
        return Err(invalid());
    }
    Ok(())
}

impl SignalCapabilityGrantStore {
    /// Called before the first socket write, using the actual original tool
    /// call. The server-minted capability call ID is not the model's call ID.
    pub(crate) async fn bind_computer_transport(
        &self,
        connection_id: &str,
        plan: &SealedComputerActionPlan,
        session: &PersistedAgentSession,
        call: &ToolCall,
        model_policy: Option<&desk_diagnose_core::model_egress::ModelEgressPolicy>,
    ) -> Result<(), DbErr> {
        let txn = self.db.begin().await?;
        let result = async {
            let (outbox, work, payload) = original_on(&txn, &plan.execution_generation).await?;
            if !work.is_side_effecting {
                return Err(invalid());
            }
            if stable_id(
                "capability-call",
                &format!("{}:{}:{}", work.conversation_id, work.turn_id, call.id),
            ) != payload.call_id
                || desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
                    &call.name,
                    serde_json::from_str(&call.arguments_json).map_err(|_| invalid())?,
                )
                .map_err(|_| invalid())?
                    != payload.canonical_input_json
            {
                return Err(invalid());
            }
            let registry =
                desk_diagnose_core::device_assistant::device_assistant_provider_registry();
            let capability = registry
                .capability_for_tool(&call.name)
                .ok_or_else(invalid)?;
            if capability.wire.capability_id != payload.capability_id
                || plan.actions.len() != 1
                || plan.actions[0].action.required_capability() != capability.required_capability
            {
                return Err(invalid());
            }
            let origin =
                ActionResultOrigin::capture(&registry, session, call).map_err(|_| invalid())?;
            let binding = ComputerBinding {
                schema_version: 1,
                connection_id: connection_id.into(),
                dispatch_sha256: format!("{:x}", Sha256::digest(outbox.payload_json.as_bytes())),
                plan: plan.clone(),
                origin,
                model_export: model_policy
                    .map(|policy| {
                        super::computer_export::ComputerExportContext::capture(
                            policy,
                            session,
                            outbox.created_at.timestamp_millis(),
                        )
                    })
                    .transpose()?,
                execution: Some(ComputerExecutionContract {
                    effect: capability.wire.effect,
                    policy: capability.wire.execution_policy,
                    foreground_budget_ms: match capability.wire.execution_policy {
                        ExecutionPolicy::Adaptive {
                            foreground_budget_ms,
                        } => foreground_budget_ms,
                        _ => 8_000,
                    },
                    chain_id: session.chain_id.clone(),
                }),
            };
            validate_binding(&binding, &outbox, &work, &payload)?;
            let json = serde_json::to_string(&binding).map_err(|_| invalid())?;
            if let Some(existing) = &outbox.computer_binding_json {
                if existing != &json {
                    return Err(invalid());
                }
                // This is metadata replay only; callers retain their original
                // one-shot claim and cannot use this return value to resend.
                return Ok(());
            }
            if outbox.state != DISPATCH_OUTBOX_SENDING
                || work.status != CAPABILITY_WORK_DISPATCHING
                || work.result_json.is_some()
                || work.manual_resolved_at.is_some()
                || outbox.computer_acceptance_json.is_some()
                || chrono::DateTime::parse_from_rfc3339(&plan.expires_at).map_err(|_| invalid())?
                    <= Utc::now()
            {
                return Err(invalid());
            }
            let changed = agent_capability_dispatch_outbox::Entity::update_many()
                .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
                .filter(agent_capability_dispatch_outbox::Column::State.eq(DISPATCH_OUTBOX_SENDING))
                .filter(agent_capability_dispatch_outbox::Column::ComputerBindingJson.is_null())
                .col_expr(
                    agent_capability_dispatch_outbox::Column::ComputerBindingJson,
                    Expr::value(json),
                )
                .exec(&txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
            #[cfg(test)]
            pause_crash_fixture_before_commit("computer_binding_before_commit");
            Ok(())
        }
        .await;
        match result {
            Ok(()) => txn.commit().await,
            Err(error) => {
                txn.rollback().await.ok();
                Err(error)
            }
        }
    }

    pub(crate) async fn accept_computer_started(
        &self,
        connection_id: &str,
        audience: &str,
        frame_request_id: &str,
        started: &ComputerActionStarted,
    ) -> Result<AcceptanceOutcome, DbErr> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(SQLITE_COMPLETION_BUSY_BUDGET_MS);
        let mut delay_ms = SQLITE_COMPLETION_BUSY_INITIAL_DELAY_MS;
        loop {
            match self
                .accept_computer_started_once(connection_id, audience, frame_request_id, started)
                .await
            {
                Ok(result) => return Ok(result),
                Err(error) if retryable_sqlite_write_contention(&error) => {
                    // Only retry this immutable receipt, never the socket write.
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Err(error);
                    }
                    tokio::time::sleep(
                        std::time::Duration::from_millis(delay_ms)
                            .min(deadline.saturating_duration_since(now)),
                    )
                    .await;
                    if tokio::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    delay_ms = delay_ms
                        .saturating_mul(2)
                        .min(SQLITE_COMPLETION_BUSY_MAX_DELAY_MS);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn accept_computer_started_once(
        &self,
        connection_id: &str,
        audience: &str,
        frame_request_id: &str,
        started: &ComputerActionStarted,
    ) -> Result<AcceptanceOutcome, DbErr> {
        if !started.executor_accepted {
            return Ok(AcceptanceOutcome::Legacy);
        }
        if !started.confirms_executor_acceptance()
            || frame_request_id != started.execution_generation
        {
            return Err(invalid());
        }
        let txn = self.db.begin().await?;
        let result = async {
            let (outbox, work, payload) = original_on(&txn, frame_request_id).await?;
            if !work.is_side_effecting {
                // Browser observation uses this transport but is still an
                // inline read, not a background mutation or acceptance proof.
                if !matches!(
                    payload.tool_name.as_str(),
                    "browser_take_snapshot" | "browser_wait_for"
                ) || work.id.to_string() != started.work_id
                    || payload.call_id != started.action_request_id
                    || work.target_device_id != audience
                    || outbox.computer_binding_json.is_some()
                    || outbox.computer_acceptance_json.is_some()
                {
                    return Err(invalid());
                }
                return Ok(AcceptanceOutcome::InlineObservation);
            }
            let json = outbox
                .computer_binding_json
                .as_deref()
                .ok_or_else(invalid)?;
            let binding: ComputerBinding = serde_json::from_str(json).map_err(|_| invalid())?;
            validate_binding(&binding, &outbox, &work, &payload)?;
            if binding.connection_id != connection_id
                || binding.plan.device_id != audience
                || binding.plan.work_id != started.work_id
                || binding.plan.action_request_id != started.action_request_id
                || binding.plan.execution_generation != started.execution_generation
            {
                return Err(invalid());
            }
            let binding_sha256 = format!("{:x}", Sha256::digest(json.as_bytes()));
            if let Some(existing) = &outbox.computer_acceptance_json {
                let receipt: ComputerAcceptance =
                    serde_json::from_str(existing).map_err(|_| invalid())?;
                if receipt.schema_version != 1
                    || receipt.binding_sha256 != binding_sha256
                    || receipt.accepted_at_unix_ms == 0
                    || i64::try_from(receipt.accepted_at_unix_ms).map_err(|_| invalid())?
                        >= chrono::DateTime::parse_from_rfc3339(&binding.plan.expires_at)
                            .map_err(|_| invalid())?
                            .timestamp_millis()
                {
                    return Err(invalid());
                }
                return Ok(AcceptanceOutcome::Duplicate);
            }
            let now = Utc::now();
            if outbox.state != DISPATCH_OUTBOX_SENDING
                || work.status != CAPABILITY_WORK_DISPATCHING
                || work.result_json.is_some()
                || work.manual_resolved_at.is_some()
                || chrono::DateTime::parse_from_rfc3339(&binding.plan.expires_at)
                    .map_err(|_| invalid())?
                    <= now
            {
                return Ok(AcceptanceOutcome::Stale);
            }
            let receipt = ComputerAcceptance {
                schema_version: 1,
                binding_sha256,
                accepted_at_unix_ms: u64::try_from(now.timestamp_millis())
                    .map_err(|_| invalid())?,
            };
            let changed = agent_capability_dispatch_outbox::Entity::update_many()
                .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
                .filter(agent_capability_dispatch_outbox::Column::ComputerAcceptanceJson.is_null())
                .col_expr(
                    agent_capability_dispatch_outbox::Column::ComputerAcceptanceJson,
                    Expr::value(serde_json::to_string(&receipt).map_err(|_| invalid())?),
                )
                .exec(&txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
            #[cfg(test)]
            pause_crash_fixture_before_commit("computer_acceptance_before_commit");
            Ok(AcceptanceOutcome::Stored)
        }
        .await;
        match result {
            Ok(result) => {
                txn.commit().await?;
                Ok(result)
            }
            Err(error) => {
                txn.rollback().await.ok();
                Err(error)
            }
        }
    }
}
