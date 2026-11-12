//! Exact call grants for one scheduled run, issued atomically with the run budget
//! and the call's own approval.
use crate::config::connection::DatabaseTransaction;
mod exception;
use super::*;
use crate::entity::agent_task_budget_reservation as ledger;
use crate::schedule_store::{self, ScheduleStore, TaskBudgetKind, TaskBudgetRequest};
use desk_agent_protocol::capability_grant::{
    CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer, CapabilityGrantLimits,
    CapabilityGrantUsePolicy,
};

pub(crate) fn identity(run: &str, call: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(run.as_bytes());
    hash.update([0]);
    hash.update(call.as_bytes());
    format!("task-call-{:x}", hash.finalize())
}

fn invalid() -> DbErr {
    DbErr::Custom("invalid task Provider authorization".into())
}

/// Caller holds owner, task, and session fences. Only the trusted Provider
/// preflight supplies scopes; model arguments never supply authority fields.
pub(super) async fn issue_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &PrepareCapabilityCall<'_>,
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
) -> Result<Option<desk_diagnose_core::dynamic_run::PermissionRequest>, DbErr> {
    let call = &request.call;
    let grant_id = request.grant_id;
    if session.actor_id != call.actor_id
        || session.device_id != call.target_device_id
        || session.input_revision != request.input_revision
        || session.latest_input_seq != request.input_watermark
        || session.current_turn_id.as_deref() != Some(request.turn_id)
        || !session.unclosed_tool_call_ids().iter().any(|id| {
            desk_diagnose_core::native_ids::background_action(
                &session.conversation_id,
                request.turn_id,
                id,
            ) == request.call_id
        })
    {
        return Err(invalid());
    }
    if session.is_subagent_result_turn() {
        return Err(invalid());
    }
    let authority = desk_diagnose_core::future::boxed(|| async {
        schedule_store::fresh_action_authority_on(txn, session).await
    })
    .await?;
    if grant_id != identity(&session.conversation_id, request.call_id) {
        return Err(invalid());
    }
    // Every device action of a scheduled run is approved on its own, by the
    // AI reviewer or the owner. A consumed one-call approval links to this
    // dispatch; without one, the run pauses with an exact approval request.
    let exception_grant_id = match desk_diagnose_core::future::boxed(|| async {
        exception::consume(txn, session, call, authority.verified_at()).await
    })
    .await?
    {
        Some(approval) => approval,
        None => {
            let original = session
                .conversation
                .iter()
                .flat_map(|message| &message.tool_calls)
                .find(|tool| {
                    desk_diagnose_core::native_ids::background_action(
                        &session.conversation_id,
                        request.turn_id,
                        &tool.id,
                    ) == request.call_id
                })
                .ok_or_else(invalid)?;
            let original = desk_diagnose_core::chat::ToolCall {
                id: original.id.clone(),
                name: original.name.clone(),
                arguments_json: original.arguments_json.clone(),
            };
            let mut checked = call.clone();
            checked.now_unix_ms = u64::try_from(authority.verified_at()).map_err(|_| invalid())?;
            let candidate = desk_diagnose_core::schedule::run_approval::request_for_original_call(
                session,
                &original,
                &checked,
                registry,
                u64::try_from(authority.valid_until()).map_err(|_| invalid())?,
                chrono::DateTime::from_timestamp_millis(authority.verified_at())
                    .ok_or_else(invalid)?
                    .to_rfc3339(),
            )
            .map_err(|_| invalid())?;
            return Ok(Some(candidate));
        }
    };
    let allocation = desk_diagnose_core::future::boxed(|| async {
        ScheduleStore::reserve_task_budget(
            txn,
            &TaskBudgetRequest {
                owner: authority.run().owner_user_id,
                device: call.target_device_id,
                run_id: &session.conversation_id,
                node: authority.run().lease_owner.as_deref().ok_or_else(invalid)?,
                lease_epoch: authority.run().lease_epoch,
                kind: TaskBudgetKind::ToolCall,
                logical_key: grant_id,
                input_sha256: call.canonical_input_digest_sha256,
                units: 1,
            },
        )
        .await
    })
    .await
    .map_err(|_| invalid())?;
    {
        let (approval_id, _) = &exception_grant_id;
        // The original approval, budget allocation and derived task grant commit
        // together. Replayed Provider work returns before reaching this path.
        let changed = desk_diagnose_core::future::boxed(|| async {
            ledger::Entity::update_many()
                .set(ledger::ActiveModel {
                    exception_grant_id: sea_orm::Set(Some(approval_id.clone())),
                    ..Default::default()
                })
                .filter(ledger::Column::Id.eq(allocation.id))
                .filter(ledger::Column::ExceptionGrantId.is_null())
                .exec(txn)
                .await
        })
        .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
    }
    let now = u64::try_from(authority.verified_at()).map_err(|_| invalid())?;
    let expiry = u64::try_from(authority.valid_until())
        .map_err(|_| invalid())?
        .min(call.now_unix_ms.saturating_add(120_000));
    let expiry = expiry.min(exception_grant_id.1);
    if expiry <= now || call.now_unix_ms > now {
        return Err(invalid());
    }
    let grant = CapabilityGrant {
        schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
        grant_id: grant_id.into(),
        actor_id: call.actor_id.into(),
        run_id: call.run_id.into(),
        input_revision: call.input_revision,
        surface: call.surface,
        target_device_id: call.target_device_id.into(),
        target_session_id: call.target_session_id.map(str::to_owned),
        provider_id: call.provider_id.into(),
        capability_id: call.capability_id.into(),
        tool_name: call.tool_name.into(),
        tool_schema_version: call.tool_schema_version,
        effect: call.effect,
        risk_tier: call.risk_tier,
        resource_scope: call.resource_scope.to_vec(),
        operation_scope: call.operation_scope.to_vec(),
        export_destinations: call.export_destinations.to_vec(),
        allowed_envelope_ids: call.envelope_ids.to_vec(),
        use_policy: CapabilityGrantUsePolicy::OneShotExact,
        canonical_input_digest_sha256: Some(call.canonical_input_digest_sha256.into()),
        issued_by: CapabilityGrantIssuer::ScheduledRun(authority.provenance().clone()),
        issued_at_unix_ms: call.now_unix_ms,
        expires_at_unix_ms: expiry,
        remaining_uses: 1,
        limits: CapabilityGrantLimits {
            max_calls: 1,
            max_bytes_per_call: call.byte_count.max(1),
            max_items_per_call: call.item_count.max(1),
        },
        policy_revision: call.policy_revision,
        readiness_revision: call.readiness_revision,
        revoked_at_unix_ms: None,
        revoked_reason: None,
    };
    desk_diagnose_core::future::boxed(|| async {
        SignalCapabilityGrantStore::issue_on(txn, &grant).await
    })
    .await?;
    Ok(None)
}
