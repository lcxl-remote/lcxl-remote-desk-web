//! Bind each model budget reservation to the currently held fresh-task session.
use super::{ScheduleStore, ScheduleStoreError, TaskBudgetKind, TaskBudgetRequest};
use crate::entity::{agent_session, agent_task_budget_reservation};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set};

impl ScheduleStore {
    /// The caller holds current owner/device policy locks and derives the budget
    /// digest and token upper bound from the final provider request. This checks
    /// both durable execution identities, but does not grant model egress or commit.
    /// Add current model policy and the provider-start record in this transaction,
    /// recheck authority after any further lock wait, then commit before HTTP.
    pub async fn reserve_fresh_model_budget_on(
        txn: &DatabaseTransaction,
        held: &PersistedAgentSession,
        request: &TaskBudgetRequest<'_>,
    ) -> Result<agent_task_budget_reservation::Model, ScheduleStoreError> {
        if request.kind != TaskBudgetKind::ModelTokens
            || request.rule_id.is_some()
            || held.actor_id != request.owner.to_string()
            || held.device_id != request.device
            || held.conversation_id != request.run_id
            || held.lease_token == 0
            || held.lease_token > i64::MAX as u64
            || held.version <= 0
            || held.surface != AgentSessionSurface::DeviceAssistant
            || held.trigger_origin != TriggerOrigin::ScheduledTask
            || held.turn_state != desk_diagnose_core::session::TurnState::Running
            || held.current_request_id.as_deref() != Some(request.run_id)
            || held.current_turn_id.as_deref() != Some(format!("{}-turn", request.run_id).as_str())
            || held.active_control_connection_id.is_some()
            || held.input_revision != 1
            || held.execution_state.has_unresolved_outcome()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        // Task -> session matches cancellation, paired renewal and tool dispatch.
        Self::lock_run_authority(
            txn,
            request.owner,
            request.device,
            request.run_id,
            request.node,
            request.lease_epoch,
        )
        .await?;
        let locked = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                lease_token: Set(held.lease_token as i64),
                ..Default::default()
            })
            .filter(agent_session::Column::ConversationId.eq(request.run_id))
            .filter(agent_session::Column::ActorId.eq(&held.actor_id))
            .filter(agent_session::Column::DeviceId.eq(request.device))
            .filter(agent_session::Column::Version.eq(held.version))
            .filter(agent_session::Column::LeaseToken.eq(held.lease_token as i64))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(request.run_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let current = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let now = super::authority::authority_now(txn).await?;
        if current != *held
            || row
                .lease_deadline
                .is_none_or(|d| d.timestamp_millis() <= now)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        // This rechecks the parent after the session lock and reserves exactly one
        // logical request. A quota record alone is never proof of provider dispatch.
        Self::reserve_task_budget(txn, request).await
    }
}

/// Metadata returned only after all transaction-local checks succeed.
/// The caller still must commit successfully before contacting the provider.
pub struct FreshModelDispatch {
    pub budget: agent_task_budget_reservation::Model,
    pub receipt: crate::entity::model_egress_receipt::Model,
}

impl ScheduleStore {
    /// Atomically bind quota, current session and a unique dispatch intent.
    /// The projection must come from the current model egress authorizer; this
    /// routine does not infer authorization from historical labels or receipts.
    /// Any error requires rollback of the caller's entire transaction.
    pub async fn reserve_fresh_model_dispatch_on(
        txn: &DatabaseTransaction,
        held: &PersistedAgentSession,
        request: &TaskBudgetRequest<'_>,
        export_authorization_id: &str,
        ordinal: u64,
        audit: &desk_diagnose_core::sink_authorizer::SinkProjectionAudit,
        inputs: &[desk_agent_protocol::data_lineage::DataEnvelope],
    ) -> Result<FreshModelDispatch, ScheduleStoreError> {
        super::publication::key(export_authorization_id)?;
        if ordinal == 0
            || ordinal > i32::MAX as u64
            || request.logical_key != format!("model-step-{ordinal}")
            || !matches!(
                audit.destination,
                desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
            )
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let budget = Self::reserve_fresh_model_budget_on(txn, held, request).await?;
        let receipt = crate::model_egress_store::SignalModelEgressStore::record_dispatch_intent_on(
            txn,
            format!("task-model-{}", budget.reservation_id),
            export_authorization_id.to_owned(),
            ordinal,
            audit,
            inputs,
        )
        .await?;
        // A duplicate receipt is an error, never evidence that sending is safe.
        // Revalidate after the insert lock wait. The same logical budget is reused,
        // not charged twice, and a failure rolls back both quota and intent.
        let current = Self::reserve_fresh_model_budget_on(txn, held, request).await?;
        if current != budget {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(FreshModelDispatch { budget, receipt })
    }
}

impl ScheduleStore {
    /// Late usage is accounting only and never restores a cancelled/revoked run.
    /// A missing or incomplete provider usage receipt leaves the quota reserved.
    pub async fn settle_fresh_model_dispatch_on(
        txn: &DatabaseTransaction,
        owner: i32,
        run_id: &str,
        ordinal: u32,
    ) -> Result<agent_task_budget_reservation::Model, ScheduleStoreError> {
        let budget = agent_task_budget_reservation::Entity::find()
            .filter(agent_task_budget_reservation::Column::OwnerUserId.eq(owner))
            .filter(agent_task_budget_reservation::Column::RunId.eq(run_id))
            .filter(agent_task_budget_reservation::Column::Kind.eq("model_tokens"))
            .filter(
                agent_task_budget_reservation::Column::LogicalKeySha256
                    .eq(super::digest(&format!("model-step-{ordinal}"))),
            )
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let receipt = crate::entity::model_egress_receipt::Entity::find_by_id(format!(
            "task-model-{}",
            budget.reservation_id
        ))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
        if owner <= 0
            || ordinal == 0
            || i32::try_from(ordinal).ok() != Some(receipt.model_call_ordinal)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let usage: desk_diagnose_core::chat::TokenUsage = serde_json::from_str(
            receipt
                .usage_json
                .as_deref()
                .ok_or(ScheduleStoreError::Conflict)?,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
        let units = desk_diagnose_core::schedule::model_usage::terminal_token_units(&usage)
            .ok_or(ScheduleStoreError::Invalid)?;
        let digest = super::digest(&super::json(&(
            receipt.receipt_id,
            receipt.export_authorization_id,
            receipt.projection_digest_sha256,
            receipt.authorized_at,
            usage,
        ))?);
        Self::settle_task_model_budget(txn, owner, run_id, &budget.reservation_id, units, &digest)
            .await
    }
}
