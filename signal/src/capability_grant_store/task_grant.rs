//! Exact call grants issued atomically with task budget and Provider preparation.
mod artifact;
mod exception;
mod message;
mod steps;
use super::*;
use crate::entity::agent_task_budget_reservation as ledger;
use crate::schedule_store::{self, ScheduleStore, TaskBudgetKind, TaskBudgetRequest};
use desk_agent_protocol::capability_grant::{
    CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer, CapabilityGrantLimits,
    CapabilityGrantUsePolicy,
};
use desk_diagnose_core::schedule::contract::{TaskCall, TaskDecision};
use sea_orm::DatabaseTransaction;
use std::collections::BTreeMap;

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
            let original = format!("{}:{}:{}", session.conversation_id, request.turn_id, id);
            format!("capability-call-{:x}", Sha256::digest(original.as_bytes())) == request.call_id
        })
    {
        return Err(invalid());
    }
    let authority = schedule_store::fresh_action_authority_on(txn, session).await?;
    let contract = authority.contract();
    let rule = contract
        .contract()
        .permissions
        .iter()
        .find(|rule| {
            rule.provider_id == call.provider_id
                && rule.tool_name == call.tool_name
                && rule.tool_schema_version == call.tool_schema_version
        })
        .ok_or_else(invalid)?;
    if grant_id != identity(&session.conversation_id, request.call_id) {
        return Err(invalid());
    }
    let rows = ledger::Entity::find()
        .filter(ledger::Column::RunId.eq(&session.conversation_id))
        .filter(ledger::Column::Kind.eq("tool_call"))
        .all(txn)
        .await?;
    let input = if matches!(
        rule.input,
        desk_agent_protocol::schedule::contract::TaskInputConstraint::GeneratedTextArtifact { .. }
    ) {
        let tool = desk_diagnose_core::chat::ToolCall {
            id: request.call_id.into(),
            name: call.tool_name.into(),
            arguments_json: request.canonical_input_json.into(),
        };
        serde_json::to_value(
            desk_diagnose_core::schedule::contract::artifact::generated_text_input(&tool)
                .map_err(|_| invalid())?,
        )
        .map_err(|_| invalid())?
    } else {
        serde_json::from_str(request.canonical_input_json).map_err(|_| invalid())?
    };
    let steps = steps::states(txn, &authority, &rows).await?;
    let step_id = contract
        .contract()
        .steps
        .iter()
        .find(|step| step.rule_id == rule.rule_id)
        .map(|step| step.step_id.as_str());
    let original = if matches!(
        rule.input,
        desk_agent_protocol::schedule::contract::TaskInputConstraint::GeneratedMessage { .. }
            | desk_agent_protocol::schedule::contract::TaskInputConstraint::GeneratedTextArtifact { .. }
    ) {
        let mut tools = session
            .conversation
            .iter()
            .flat_map(|message| &message.tool_calls)
            .filter(|tool| {
                let original = format!(
                    "{}:{}:{}",
                    session.conversation_id, request.turn_id, tool.id
                );
                format!("capability-call-{:x}", Sha256::digest(original.as_bytes()))
                    == request.call_id
            });
        let tool = tools.next().ok_or_else(invalid)?;
        if tools.next().is_some() || tool.name != call.tool_name {
            return Err(invalid());
        }
        let raw = serde_json::from_str(&tool.arguments_json).map_err(|_| invalid())?;
        if desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
            &tool.name, raw,
        )
        .map_err(|_| invalid())?
            != request.canonical_input_json
        {
            return Err(invalid());
        }
        let original = desk_diagnose_core::chat::ToolCall {
            id: tool.id.clone(),
            name: tool.name.clone(),
            arguments_json: tool.arguments_json.clone(),
        };
        Some(original)
    } else {
        None
    };
    let message = if matches!(
        rule.input,
        desk_agent_protocol::schedule::contract::TaskInputConstraint::GeneratedMessage { .. }
    ) {
        Some(
            message::bind(
                txn,
                session,
                &authority,
                original.as_ref().ok_or_else(invalid)?,
                step_id.ok_or_else(invalid)?,
            )
            .await?,
        )
    } else {
        None
    };
    let artifact_sources = if matches!(
        rule.input,
        desk_agent_protocol::schedule::contract::TaskInputConstraint::GeneratedTextArtifact { .. }
    ) {
        artifact::sources(
            txn,
            session,
            &authority,
            &original.as_ref().ok_or_else(invalid)?.id,
        )
        .await?
    } else {
        Vec::new()
    };
    let artifact_resources = if matches!(
        rule.input,
        desk_agent_protocol::schedule::contract::TaskInputConstraint::GeneratedTextArtifact { .. }
    ) {
        desk_diagnose_core::schedule::contract::artifact::bind_directory(
            contract,
            session,
            original.as_ref().ok_or_else(invalid)?,
            step_id.ok_or_else(invalid)?,
            call.resource_scope,
            u64::try_from(authority.verified_at()).map_err(|_| invalid())?,
        )
        .map_err(|_| invalid())?
    } else {
        call.resource_scope.to_vec()
    };
    let decision = contract.evaluate(&TaskCall {
        target_device_id: call.target_device_id,
        provider_id: call.provider_id,
        capability_id: call.capability_id,
        tool_name: call.tool_name,
        tool_schema_version: call.tool_schema_version,
        effect: call.effect,
        risk_tier: call.risk_tier,
        input: message.as_ref().map_or(&input, |message| &message.input),
        resources: message
            .as_ref()
            .map_or(artifact_resources.as_slice(), |message| {
                message.resources.as_slice()
            }),
        operations: call.operation_scope,
        export_destinations: call.export_destinations,
        byte_count: call.byte_count,
        item_count: call.item_count,
        rule_call_count: u32::try_from(
            rows.iter()
                .filter(|row| row.rule_id.as_deref() == Some(rule.rule_id.as_str()))
                .count(),
        )
        .map_err(|_| invalid())?,
        run_call_count: u32::try_from(rows.len()).map_err(|_| invalid())?,
        step_id,
        step_states: &steps,
        message_destination: message.as_ref().map(|message| &message.destination),
        source_scopes: message
            .as_ref()
            .map_or(artifact_sources.as_slice(), |message| {
                message.sources.as_slice()
            }),
    });
    let decision = match &message {
        Some(message) => message.attachment_decision.intersect(decision),
        None => decision,
    };
    let exception_grant_id = match decision {
        TaskDecision::Allowed { rule_id } if rule_id == rule.rule_id => None,
        TaskDecision::ApprovalRequired { rule_id } if rule_id == rule.rule_id => {
            match exception::consume(txn, session, call, authority.verified_at()).await? {
                Some(approval) => Some(approval),
                None => {
                    let original = session
                        .conversation
                        .iter()
                        .flat_map(|message| &message.tool_calls)
                        .find(|tool| {
                            let key = format!(
                                "{}:{}:{}",
                                session.conversation_id, request.turn_id, tool.id
                            );
                            format!("capability-call-{:x}", Sha256::digest(key.as_bytes()))
                                == request.call_id
                        })
                        .ok_or_else(invalid)?;
                    let original = desk_diagnose_core::chat::ToolCall {
                        id: original.id.clone(),
                        name: original.name.clone(),
                        arguments_json: original.arguments_json.clone(),
                    };
                    let mut checked = call.clone();
                    checked.now_unix_ms =
                        u64::try_from(authority.verified_at()).map_err(|_| invalid())?;
                    let candidate = desk_diagnose_core::schedule::contract::exception::request_for_original_call(
                        contract, session, &original, &checked, registry, &session.device_id,
                        u64::try_from(authority.valid_until()).map_err(|_| invalid())?,
                        chrono::DateTime::from_timestamp_millis(authority.verified_at()).ok_or_else(invalid)?.to_rfc3339(),
                    ).map_err(|_| invalid())?;
                    return Ok(Some(candidate));
                }
            }
        }
        _ => return Err(invalid()),
    };
    let allocation = ScheduleStore::reserve_task_budget(
        txn,
        &TaskBudgetRequest {
            owner: authority.run().owner_user_id,
            device: call.target_device_id,
            run_id: &session.conversation_id,
            node: authority.run().lease_owner.as_deref().ok_or_else(invalid)?,
            lease_epoch: authority.run().lease_epoch,
            kind: TaskBudgetKind::ToolCall,
            rule_id: Some(rule.rule_id.as_str()),
            logical_key: grant_id,
            input_sha256: call.canonical_input_digest_sha256,
            units: 1,
        },
    )
    .await
    .map_err(|_| invalid())?;
    if let Some((approval_id, _)) = &exception_grant_id {
        // The original approval, budget allocation and derived task grant commit
        // together. Replayed Provider work returns before reaching this path.
        let changed = ledger::Entity::update_many()
            .set(ledger::ActiveModel {
                exception_grant_id: sea_orm::Set(Some(approval_id.clone())),
                ..Default::default()
            })
            .filter(ledger::Column::Id.eq(allocation.id))
            .filter(ledger::Column::ExceptionGrantId.is_null())
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
    }
    let now = u64::try_from(authority.verified_at()).map_err(|_| invalid())?;
    let expiry = u64::try_from(authority.valid_until())
        .map_err(|_| invalid())?
        .min(call.now_unix_ms.saturating_add(120_000));
    let expiry = expiry.min(
        exception_grant_id
            .as_ref()
            .map_or(u64::MAX, |(_, expiry)| *expiry),
    );
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
        allowed_content_digests_sha256: call.content_digests_sha256.to_vec(),
        use_policy: CapabilityGrantUsePolicy::OneShotExact,
        canonical_input_digest_sha256: Some(call.canonical_input_digest_sha256.into()),
        issued_by: CapabilityGrantIssuer::TaskAuthorization(authority.provenance().clone()),
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
    SignalCapabilityGrantStore::issue_on(txn, &grant).await?;
    Ok(None)
}

pub(crate) async fn all_steps_succeeded_on(
    txn: &DatabaseTransaction,
    authority: &schedule_store::CurrentTaskAuthority,
) -> Result<bool, DbErr> {
    let rows = ledger::Entity::find()
        .filter(ledger::Column::RunId.eq(&authority.run().run_id))
        .filter(ledger::Column::Kind.eq("tool_call"))
        .all(txn)
        .await?;
    Ok(steps::states(txn, authority, &rows)
        .await?
        .values()
        .all(|state| *state == desk_agent_protocol::schedule::contract::TaskStepStatus::Succeeded))
}

/// Read original step receipts using historical identity, never execution authority.
pub(crate) async fn step_states_for_recovery(
    txn: &DatabaseTransaction,
    context: &schedule_store::TaskReceiptContext,
) -> Result<BTreeMap<String, desk_agent_protocol::schedule::contract::TaskStepStatus>, DbErr> {
    let rows = ledger::Entity::find()
        .filter(ledger::Column::RunId.eq(context.run_id()))
        .filter(ledger::Column::Kind.eq("tool_call"))
        .all(txn)
        .await?;
    steps::historical_states(txn, context, &rows).await
}
