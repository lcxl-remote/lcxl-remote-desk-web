//! Per-call approval inside one scheduled run. Every device action of a fresh
//! task is approved on its own, by the AI reviewer or the owner.
use crate::{
    capability_grant::{CapabilityGrantCall, match_capability_grant},
    dynamic_run::PermissionRequestState,
    session::{PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::capability_grant::{
    CapabilityGrant, CapabilityGrantIssuer, CapabilityGrantUsePolicy,
};
use sha2::{Digest, Sha256};

fn subset<T: PartialEq>(items: &[T], allowed: &[T]) -> bool {
    items.iter().all(|item| allowed.contains(item))
}

fn approval_grant_id(
    session: &PersistedAgentSession,
    request_id: &str,
    item_id: &str,
    revision: u64,
) -> String {
    format!(
        "grant-{:x}",
        Sha256::digest(
            format!(
                "{}:{request_id}:{item_id}:{revision}",
                session.conversation_id
            )
            .as_bytes()
        )
    )
}

fn decided(grant: &CapabilityGrant) -> bool {
    matches!(
        grant.issued_by,
        CapabilityGrantIssuer::UserDecision | CapabilityGrantIssuer::AiApproval(_)
    )
}

/// The caller supplies an immutable-issuance-validated database grant. This
/// predicate neither consumes the decision nor authorizes dispatch.
pub fn matches_one_call_approval(
    session: &PersistedAgentSession,
    grant: &CapabilityGrant,
    call: &CapabilityGrantCall<'_>,
) -> bool {
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.conversation_id != call.run_id
        || session.actor_id != call.actor_id
        || session.device_id != call.target_device_id
        || session.input_revision != call.input_revision
        || !decided(grant)
        || grant.use_policy != CapabilityGrantUsePolicy::OneShotExact
        || grant.remaining_uses != 1
        || grant.limits.max_calls != 1
        || match_capability_grant(grant, call).is_err()
    {
        return false;
    }
    session.permission_requests.iter().any(|request| {
        request.input_revision == session.input_revision
            && request.validate().is_ok()
            && matches!(
                request.state,
                PermissionRequestState::Approved | PermissionRequestState::PartiallyApproved
            )
            && request.items.iter().any(|item| {
                grant.grant_id
                    == approval_grant_id(
                        session,
                        &request.request_id,
                        &item.item_id,
                        request.input_revision,
                    )
                    && item.provider_id == call.provider_id
                    && item.tool_name == call.tool_name
                    && item.expected_effect == call.effect
                    && item.canonical_input_digest_sha256.as_deref()
                        == Some(call.canonical_input_digest_sha256)
            })
    })
}

/// Bind decided grants to the exact one-call request of this run and its
/// remaining runtime before storing the decision receipt.
pub fn constrain_approval(
    session: &PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
    valid_until: u64,
    grants: &mut [CapabilityGrant],
) -> Result<(), &'static str> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || request.input_revision != session.input_revision
    {
        return Err("approval belongs to another run");
    }
    for grant in grants {
        let item = request
            .items
            .iter()
            .find(|item| {
                approval_grant_id(
                    session,
                    &request.request_id,
                    &item.item_id,
                    request.input_revision,
                ) == grant.grant_id
            })
            .ok_or("approval item is missing")?;
        if !decided(grant)
            || grant.run_id != session.conversation_id
            || grant.actor_id != session.actor_id
            || grant.target_device_id != session.device_id
            || grant.provider_id != item.provider_id
            || grant.tool_name != item.tool_name
            || grant.effect != item.expected_effect
            || grant.remaining_uses != 1
            || grant.limits.max_calls != 1
            || grant.use_policy != CapabilityGrantUsePolicy::OneShotExact
            || item.canonical_input_digest_sha256 != grant.canonical_input_digest_sha256
            || !subset(&grant.resource_scope, &item.resource_scope)
            || !subset(&grant.operation_scope, &item.operation_scope)
            || !subset(&grant.export_destinations, &item.export_destinations)
        {
            return Err("approval exceeds the requested call");
        }
        grant.expires_at_unix_ms = grant.expires_at_unix_ms.min(valid_until);
        grant.validate().map_err(|_| "invalid approval limits")?;
    }
    Ok(())
}

/// Validate a server-normalized request before presenting it for review.
/// No decision or execution grant is synthesized by this check.
pub fn validate_request(
    session: &PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
) -> Result<(), &'static str> {
    request.validate().map_err(|_| "invalid approval request")?;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || request.input_revision != session.input_revision
        || request
            .items
            .iter()
            .any(|item| item.suggested_max_uses != 1 || item.canonical_input_json.is_none())
    {
        return Err("a scheduled run approves one exact call at a time");
    }
    Ok(())
}

/// Build a pending, exact one-call request from an authenticated Provider
/// preflight. The caller persists it under the run and session fences. This
/// value is not a grant and cannot authorize dispatch.
pub fn request_for_original_call(
    session: &PersistedAgentSession,
    original: &crate::chat::ToolCall,
    call: &CapabilityGrantCall<'_>,
    registry: &crate::provider_registry::ProviderRegistry,
    valid_until_unix_ms: u64,
    created_at: String,
) -> Result<crate::dynamic_run::PermissionRequest, &'static str> {
    let invalid = "approval request does not match the original call";
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.conversation_id != call.run_id
        || session.actor_id != call.actor_id
        || session.device_id != call.target_device_id
        || session.input_revision != call.input_revision
        || original.name != call.tool_name
        || original.id.trim().is_empty()
        || !unique_original_call(&session.conversation, original)
    {
        return Err(invalid);
    }
    let capability = registry
        .capability_for_tool(call.tool_name)
        .ok_or(invalid)?;
    if capability.wire.capability_id != call.capability_id
        || capability.wire.input_schema_version != call.tool_schema_version
        || capability.wire.effect != call.effect
    {
        return Err(invalid);
    }
    // Request timestamps describe this authenticated preflight; they also
    // anchor validation of ephemeral directory selections.
    let created_ms = chrono::DateTime::parse_from_rfc3339(&created_at)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .ok_or(invalid)?;
    if created_ms != call.now_unix_ms {
        return Err(invalid);
    }
    let ttl = valid_until_unix_ms
        .checked_sub(call.now_unix_ms)
        .filter(|remaining| *remaining >= 1000)
        .ok_or("the run has no time left for an approval")?
        / 1000;
    let executed =
        crate::ui_model_ids::resolve_recorded_call(original, session).map_err(|_| invalid)?;
    let canonical = crate::permission_tools::canonical_tool_permission_input_json(
        &executed.name,
        serde_json::from_str(&executed.arguments_json).map_err(|_| invalid)?,
    )
    .map_err(|_| invalid)?;
    if format!("{:x}", Sha256::digest(canonical.as_bytes())) != call.canonical_input_digest_sha256 {
        return Err(invalid);
    }
    let identity = serde_json::to_vec(&(
        &session.conversation_id,
        session.input_revision,
        &original.id,
        call.canonical_input_digest_sha256,
    ))
    .map_err(|_| invalid)?;
    let request_id = format!("task-permission-{:x}", Sha256::digest(identity));
    let planning = crate::chat::ToolCall {
        id: original.id.clone(),
        name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
        arguments_json: serde_json::json!({ "items": [{
            "item_id": original.id, "provider_id": call.provider_id, "tool_name": call.tool_name,
            "expected_effect": call.effect, "resource_scope": call.resource_scope,
            "operation_scope": call.operation_scope,
            "exact_input": serde_json::from_str::<serde_json::Value>(&canonical).map_err(|_| invalid)?,
            "suggested_ttl_seconds": ttl.min(300), "suggested_max_uses": 1,
            "reason": "This scheduled operation needs approval before it runs."
        }] })
        .to_string(),
    };
    let request = crate::permission_tools::build_permission_request(
        &planning,
        registry,
        request_id,
        session.input_revision,
        created_at,
    )
    .map_err(|_| invalid)?;
    let item = request.items.first().ok_or(invalid)?;
    if request.items.len() != 1
        || item.canonical_input_json.as_deref() != Some(canonical.as_str())
        || item.canonical_input_digest_sha256.as_deref() != Some(call.canonical_input_digest_sha256)
        || !subset(&item.resource_scope, call.resource_scope)
        || !subset(call.resource_scope, &item.resource_scope)
        || !subset(&item.operation_scope, call.operation_scope)
        || !subset(call.operation_scope, &item.operation_scope)
        || !subset(&item.export_destinations, call.export_destinations)
        || !subset(call.export_destinations, &item.export_destinations)
    {
        return Err(invalid);
    }
    validate_request(session, &request)?;
    Ok(request)
}

pub(crate) fn unique_original_call(
    messages: &[crate::chat::ChatMessage],
    original: &crate::chat::ToolCall,
) -> bool {
    let mut matches = messages
        .iter()
        .flat_map(|message| message.tool_calls.iter().map(move |call| (message, call)))
        .filter(|(_, call)| call.id == original.id);
    let Some((message, call)) = matches.next() else {
        return false;
    };
    if message.role != crate::chat::ChatRole::Assistant {
        return false;
    }
    call == &original.to_ref() && matches.next().is_none()
}
