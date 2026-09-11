//! Single-account scheduled approval projection and paired occurrence claim.
use super::*;
use crate::schedule_store::{
    ClaimedContinuation, ContinuationClaim, ContinuationPermissionClaim, ScheduleStore,
};
use desk_signal_facade::model::{auth_context::AuthKind, signal::RemoteDeskTypeEnum};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

pub struct ClaimedScheduledPermission {
    pub target_connection_id: String,
    pub claimed: ClaimedContinuation,
}

/// Takes both leases; immediately pass the result to `resume_scheduled_turn`.
/// Only the single OSS owner can own a scheduled occurrence.
pub async fn claim_scheduled_permission(
    connections: &SharedConnectionMap,
    db: &DatabaseConnection,
    gate: &crate::device_assistant_gate::DeviceAssistantGate,
    run_id: &str,
    lease_seconds: u32,
) -> Result<ClaimedScheduledPermission, AgentError> {
    let invalid = || transport_error("scheduled approval is unavailable or no longer current");
    if !gate.is_enabled() {
        return Err(invalid());
    }
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let work = crate::entity::agent_schedule_run::Entity::find()
        .filter(crate::entity::agent_schedule_run::Column::RunId.eq(run_id))
        .filter(crate::entity::agent_schedule_run::Column::OwnerUserId.eq(owner))
        .one(db)
        .await
        .map_err(|_| invalid())?
        .ok_or_else(invalid)?;
    if work.status != "awaiting_permission"
        || work.cancel_requested_at.is_some()
        || work.failure_accounted
    {
        return Err(invalid());
    }
    let request = work
        .result_ref
        .as_deref()
        .and_then(|value| value.strip_prefix("permission:"))
        .filter(|value| !value.is_empty())
        .ok_or_else(invalid)?;
    let row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&work.conversation_id))
        .filter(crate::entity::agent_session::Column::ActorId.eq(owner.to_string()))
        .one(db)
        .await
        .map_err(|_| invalid())?
        .ok_or_else(invalid)?;
    let (session, grants) = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .pending_scheduled_permission(&work.conversation_id, request, &row.device_id)
        .await?
        .ok_or_else(invalid)?;
    let target_connection_id = {
        let map = connections.read().await;
        let mut targets = map.values().filter(|target| {
            target.auth_context.auth_kind == AuthKind::TokenAuth
                && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && target.model.version_info.client_id.as_deref()
                    == Some(session.device_id.as_str())
        });
        let target = targets.next().ok_or_else(invalid)?;
        if targets.next().is_some() {
            return Err(invalid());
        }
        target.model.connection_id.clone()
    };
    let config = crate::model_provider::load(db)
        .await
        .map_err(|_| invalid())?;
    let (providers, inventory, now, readiness) = current_capability_projection(
        db,
        connections,
        &target_connection_id,
        ModelCapabilities {
            image_input: config.supports_image_input,
        },
    )
    .await;
    let readiness = readiness.ok_or_else(invalid)?;
    let exact = desk_diagnose_core::permission_tools::active_exact_authorized_tool_names(
        &grants,
        &session.permission_requests,
        now,
        session.input_revision,
        readiness.revision,
    );
    let mut scope = session.scope_snapshot.clone();
    for grant in &grants {
        let Some(capability) = providers.capability_for_tool(&grant.tool_name) else {
            continue;
        };
        let Some(provider) = providers.provider_for_capability(&capability.wire.capability_id)
        else {
            continue;
        };
        if grant.validate().is_err()
            || !matches!(
                grant.issued_by,
                desk_agent_protocol::capability_grant::CapabilityGrantIssuer::UserDecision
            )
            || grant.surface
                != desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner
            || grant.actor_id != session.actor_id
            || grant.run_id != session.conversation_id
            || grant.target_device_id != session.device_id
            || grant.policy_revision != session.policy_revision
            || grant.readiness_revision != readiness.revision
            || grant.revoked_at_unix_ms.is_some()
            || grant.expires_at_unix_ms <= now
            || grant.issued_at_unix_ms > now
            || grant.remaining_uses == 0
            || grant.provider_id != provider.wire.provider_id
            || grant.capability_id != capability.wire.capability_id
            || grant.effect != capability.wire.effect
            || grant.tool_schema_version != capability.wire.input_schema_version
            || !inventory
                .iter()
                .any(|item| item.tool_name == grant.tool_name && item.callable())
        {
            continue;
        }
        let scoped = grant.canonical_input_digest_sha256.is_none()
            && (providers.registered_tools().iter().any(|tool| {
                tool.name() == grant.tool_name
                    && tool.effect == desk_diagnose_core::registry::ToolEffect::ReadOnly
            }) || matches!(
                grant.tool_name.as_str(),
                "create_text_artifact_in_selected_directory"
                    | "create_workbook_from_merge_preview"
                    | "create_word_report_from_merge_preview"
                    | "create_local_communication_draft"
            ));
        if (scoped || exact.contains(&grant.tool_name))
            && !scope.granted.contains(&capability.required_capability)
        {
            scope.granted.push(capability.required_capability);
        }
    }
    scope.mode = if scope.granted.iter().any(capability_enables_mutation) {
        config
            .execution_mode
            .restrict_to(ExecutionMode::ConfirmEachAction)
    } else {
        ExecutionMode::ReadOnly
    };
    let claimed = ScheduleStore::new(db.clone())
        .claim_continuation_permission(ContinuationPermissionClaim {
            continuation: ContinuationClaim {
                owner,
                run_id,
                node_id: "oss-scheduler",
                lease_seconds,
                policy_revision: session.policy_revision,
                scope,
            },
            request_id: request,
            expected_session_version: session.version,
            expected_run_epoch: work.lease_epoch,
            grants: &grants,
        })
        .await
        .map_err(|_| invalid())?;
    Ok(ClaimedScheduledPermission {
        target_connection_id,
        claimed,
    })
}
