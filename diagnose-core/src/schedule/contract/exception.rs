//! A one-call owner exception supplements, but never replaces, task evaluation.
use crate::{
    capability_grant::{CapabilityGrantCall, match_capability_grant},
    dynamic_run::PermissionRequestState,
    session::{PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::capability_grant::{
    CapabilityGrant, CapabilityGrantIssuer, CapabilityGrantUsePolicy,
};
use sha2::{Digest, Sha256};

/// The caller supplies an immutable-issuance-validated database grant. The full
/// task evaluator must already have returned ApprovalRequired for this same call.
/// This predicate neither consumes the decision nor authorizes dispatch.
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
        || grant.issued_by != CapabilityGrantIssuer::UserDecision
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
                let id = format!(
                    "grant-{:x}",
                    Sha256::digest(
                        format!(
                            "{}:{}:{}:{}",
                            session.conversation_id,
                            request.request_id,
                            item.item_id,
                            request.input_revision
                        )
                        .as_bytes()
                    )
                );
                grant.grant_id == id
                    && item.provider_id == call.provider_id
                    && item.tool_name == call.tool_name
                    && item.expected_effect == call.effect
                    && item.canonical_input_digest_sha256.as_deref()
                        == Some(call.canonical_input_digest_sha256)
            })
    })
}

/// Narrow an owner-approved exact call to the frozen task ceiling before storing
/// its decision receipt. Source lineage, live surface provenance, step state and
/// remaining budgets must still be checked during actual Provider preparation.
pub fn constrain_approval(
    contract: &super::ValidatedTaskContract,
    session: &PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
    provider_device_id: &str,
    valid_until: u64,
    grants: &mut [CapabilityGrant],
) -> Result<(), super::TaskContractError> {
    use super::{TaskContractError as Error, TaskInputConstraint};
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || contract.contract().target_device_id != session.device_id
        || request.input_revision != session.input_revision
        || contract.contract().exception_mode != super::TaskExceptionMode::RequestApproval
    {
        return Err(Error::InvalidIdentity);
    }
    for grant in grants {
        let rule = contract
            .contract()
            .permissions
            .iter()
            .find(|rule| {
                rule.provider_id == grant.provider_id
                    && rule.tool_name == grant.tool_name
                    && rule.tool_schema_version == grant.tool_schema_version
            })
            .ok_or(Error::InvalidIdentity)?;
        let item = request
            .items
            .iter()
            .find(|item| {
                let id = format!(
                    "grant-{:x}",
                    Sha256::digest(
                        format!(
                            "{}:{}:{}:{}",
                            session.conversation_id,
                            request.request_id,
                            item.item_id,
                            request.input_revision
                        )
                        .as_bytes()
                    )
                );
                id == grant.grant_id
            })
            .ok_or(Error::InvalidIdentity)?;
        if grant.issued_by != CapabilityGrantIssuer::UserDecision
            || grant.run_id != session.conversation_id
            || grant.actor_id != session.actor_id
            || grant.target_device_id != session.device_id
            || grant.input_revision != session.input_revision
            || grant.capability_id != rule.capability_id
            || grant.effect != rule.effect
            || grant.risk_tier != rule.risk_tier
            || grant.remaining_uses != 1
            || grant.limits.max_calls != 1
            || grant.use_policy != CapabilityGrantUsePolicy::OneShotExact
            || item.canonical_input_digest_sha256 != grant.canonical_input_digest_sha256
            || !super::subset(&grant.operation_scope, &rule.approval_ceiling.operations)
            || !super::subset(
                &grant.export_destinations,
                &rule.approval_ceiling.export_destinations,
            )
        {
            return Err(Error::InvalidScope);
        }
        let canonical = item
            .canonical_input_json
            .as_deref()
            .ok_or(Error::InvalidInput)?;
        validate_requested_input(contract, session, rule, canonical, provider_device_id)?;
        if !matches!(
            rule.input,
            TaskInputConstraint::GeneratedMessage { .. }
                | TaskInputConstraint::GeneratedTextArtifact { .. }
        ) && !super::subset(&grant.resource_scope, &rule.approval_ceiling.resources)
        {
            return Err(Error::InvalidScope);
        }
        validate_artifact_resources(
            contract,
            session,
            rule,
            canonical,
            &grant.resource_scope,
            grant.issued_at_unix_ms,
        )?;
        grant.expires_at_unix_ms = grant.expires_at_unix_ms.min(valid_until);
        grant.limits.max_bytes_per_call = grant
            .limits
            .max_bytes_per_call
            .min(rule.approval_ceiling.limits.max_bytes_per_call);
        grant.limits.max_items_per_call = grant
            .limits
            .max_items_per_call
            .min(rule.approval_ceiling.limits.max_items_per_call);
        grant.validate().map_err(|_| Error::InvalidLimits)?;
    }
    Ok(())
}

fn validate_requested_input(
    contract: &super::ValidatedTaskContract,
    session: &PersistedAgentSession,
    rule: &super::TaskPermissionRule,
    canonical: &str,
    provider_device_id: &str,
) -> Result<(), super::TaskContractError> {
    use super::{
        TaskContractError as Error, TaskGeneratedMessage, TaskInputConstraint, TaskMessageTarget,
    };
    match &rule.input {
        TaskInputConstraint::Exact { canonical_json } => {
            if canonical != canonical_json {
                return Err(Error::InvalidInput);
            }
        }
        TaskInputConstraint::ScopedRead => {}
        TaskInputConstraint::GeneratedTextArtifact {
            file_name,
            max_content_bytes,
        } => {
            let tool = crate::chat::ToolCall {
                id: "artifact-approval".into(),
                name: rule.tool_name.clone(),
                arguments_json: canonical.into(),
            };
            let action = crate::provider_preflight::artifact_action_from_call(&tool)
                .map_err(|_| Error::InvalidInput)?;
            match action {
                desk_agent_protocol::computer_use::FilePatchAction::CreateTextArtifact {
                    file_name: actual,
                    content_utf8,
                } if actual == *file_name && content_utf8.len() <= *max_content_bytes as usize => {}
                _ => return Err(Error::InvalidInput),
            }
        }
        TaskInputConstraint::GeneratedMessage { .. } => {
            let step = contract
                .contract()
                .steps
                .iter()
                .find(|step| step.rule_id == rule.rule_id)
                .ok_or(Error::InvalidSteps)?;
            let target = TaskMessageTarget {
                task_device_id: &session.device_id,
                provider_device_id,
            };
            let destination = match rule.tool_name.as_str() {
                "prepare_gmail_web_draft_handoff" => {
                    let input: desk_agent_protocol::communication::GmailWebDraftHandoffInput =
                        serde_json::from_str(canonical).map_err(|_| Error::InvalidInput)?;
                    contract.verify_generated_gmail_draft_approval_scope(
                        &target,
                        &step.step_id,
                        &input,
                        &input.page,
                    )?
                }
                "prepare_slack_web_message_handoff" => {
                    let input: desk_agent_protocol::communication::SlackWebDraftHandoffInput =
                        serde_json::from_str(canonical).map_err(|_| Error::InvalidInput)?;
                    contract.verify_generated_slack_draft(
                        &target,
                        &step.step_id,
                        &input,
                        &input.page,
                    )?
                }
                "send_gmail_web_exact" => {
                    let input: desk_agent_protocol::communication::GmailWebExactSendInput =
                        serde_json::from_str(canonical).map_err(|_| Error::InvalidInput)?;
                    crate::communication::verify_gmail_web_exact_send_input(&input)
                        .map_err(|_| Error::InvalidInput)?;
                    let snapshot = input
                        .handoff
                        .send_payload_snapshot
                        .as_ref()
                        .ok_or(Error::InvalidInput)?;
                    contract.verify_generated_send_snapshot_approval_scope(
                        &target,
                        &session.conversation_id,
                        &step.step_id,
                        snapshot,
                        &snapshot.payload.surface,
                        &TaskGeneratedMessage {
                            subject: input.draft.subject.clone(),
                            body: input.draft.body_plain_text.clone(),
                        },
                    )?
                }
                "send_slack_web_exact" => {
                    let input: desk_agent_protocol::communication::SlackWebExactSendInput =
                        serde_json::from_str(canonical).map_err(|_| Error::InvalidInput)?;
                    crate::communication::verify_slack_web_exact_send_input(&input)
                        .map_err(|_| Error::InvalidInput)?;
                    let snapshot = input
                        .handoff
                        .send_payload_snapshot
                        .as_ref()
                        .ok_or(Error::InvalidInput)?;
                    contract.verify_generated_send_snapshot_approval_scope(
                        &target,
                        &session.conversation_id,
                        &step.step_id,
                        snapshot,
                        &snapshot.payload.surface,
                        &TaskGeneratedMessage {
                            subject: String::new(),
                            body: input.body_plain_text.clone(),
                        },
                    )?
                }
                _ => return Err(Error::InvalidInput),
            };
            if !super::subset(
                &super::task_message_resource_scope(&session.device_id, destination)?,
                &rule.approval_ceiling.resources,
            ) {
                return Err(Error::InvalidScope);
            }
        }
    }
    Ok(())
}

/// Validate the server-normalized proposal before presenting an exception card.
/// No user decision or execution grant is synthesized by this check.
pub fn validate_request(
    contract: &super::ValidatedTaskContract,
    session: &PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
    provider_device_id: &str,
) -> Result<(), super::TaskContractError> {
    use super::{TaskContractError as Error, TaskInputConstraint};
    request.validate().map_err(|_| Error::InvalidInput)?;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || request.input_revision != session.input_revision
        || contract.contract().target_device_id != session.device_id
        || contract.contract().exception_mode != super::TaskExceptionMode::RequestApproval
    {
        return Err(Error::InvalidIdentity);
    }
    for item in &request.items {
        let rule = contract
            .contract()
            .permissions
            .iter()
            .find(|rule| rule.provider_id == item.provider_id && rule.tool_name == item.tool_name)
            .ok_or(Error::InvalidIdentity)?;
        if item.expected_effect != rule.effect
            || item.suggested_max_uses != 1
            || !super::subset(&item.operation_scope, &rule.approval_ceiling.operations)
            || !super::subset(
                &item.export_destinations,
                &rule.approval_ceiling.export_destinations,
            )
            || (!matches!(
                rule.input,
                TaskInputConstraint::GeneratedMessage { .. }
                    | TaskInputConstraint::GeneratedTextArtifact { .. }
            ) && !super::subset(&item.resource_scope, &rule.approval_ceiling.resources))
        {
            return Err(Error::InvalidScope);
        }
        validate_requested_input(
            contract,
            session,
            rule,
            item.canonical_input_json
                .as_deref()
                .ok_or(Error::InvalidInput)?,
            provider_device_id,
        )?;
        if matches!(
            rule.input,
            TaskInputConstraint::GeneratedTextArtifact { .. }
        ) {
            let created = chrono::DateTime::parse_from_rfc3339(&request.created_at)
                .ok()
                .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                .ok_or(Error::InvalidInput)?;
            validate_artifact_resources(
                contract,
                session,
                rule,
                item.canonical_input_json
                    .as_deref()
                    .ok_or(Error::InvalidInput)?,
                &item.resource_scope,
                created,
            )?;
        }
    }
    Ok(())
}

/// Build a pending, exact one-call request from an authenticated Provider
/// preflight. The caller must first obtain ApprovalRequired from full evaluation
/// (including lineage and step state), then persist under the same task/session
/// fences. This value is not a grant and cannot authorize dispatch.
#[expect(
    clippy::too_many_arguments,
    reason = "The exact approval request binds the original call, authenticated subject, registry and validity window explicitly"
)]
pub fn request_for_original_call(
    contract: &super::ValidatedTaskContract,
    session: &PersistedAgentSession,
    original: &crate::chat::ToolCall,
    call: &CapabilityGrantCall<'_>,
    registry: &crate::provider_registry::ProviderRegistry,
    provider_device_id: &str,
    valid_until_unix_ms: u64,
    created_at: String,
) -> Result<crate::dynamic_run::PermissionRequest, super::TaskContractError> {
    use super::TaskContractError as Error;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.conversation_id != call.run_id
        || session.actor_id != call.actor_id
        || session.device_id != call.target_device_id
        || session.input_revision != call.input_revision
        || original.name != call.tool_name
        || original.id.trim().is_empty()
        || !unique_original_call(&session.conversation, original)
    {
        return Err(Error::InvalidIdentity);
    }
    let capability = registry
        .capability_for_tool(call.tool_name)
        .ok_or(Error::InvalidIdentity)?;
    if capability.wire.capability_id != call.capability_id
        || capability.wire.input_schema_version != call.tool_schema_version
        || capability.wire.effect != call.effect
    {
        return Err(Error::InvalidIdentity);
    }
    let rule = contract
        .contract()
        .permissions
        .iter()
        .find(|rule| {
            rule.provider_id == call.provider_id
                && rule.capability_id == call.capability_id
                && rule.tool_name == call.tool_name
                && rule.tool_schema_version == call.tool_schema_version
        })
        .ok_or(Error::InvalidIdentity)?;
    if rule.effect != call.effect
        || rule.risk_tier != call.risk_tier
        || call.byte_count > rule.approval_ceiling.limits.max_bytes_per_call
        || call.item_count > rule.approval_ceiling.limits.max_items_per_call
    {
        return Err(Error::InvalidLimits);
    }
    // Request timestamps must describe this authenticated preflight, since they
    // also anchor validation of ephemeral directory selections.
    let created_ms = chrono::DateTime::parse_from_rfc3339(&created_at)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .ok_or(Error::InvalidInput)?;
    if created_ms != call.now_unix_ms {
        return Err(Error::InvalidInput);
    }
    let ttl = valid_until_unix_ms
        .checked_sub(call.now_unix_ms)
        .filter(|remaining| *remaining >= 1000)
        .ok_or(Error::InvalidLimits)?
        / 1000;
    let canonical = crate::permission_tools::canonical_tool_permission_input_json(
        &original.name,
        serde_json::from_str(&original.arguments_json).map_err(|_| Error::InvalidInput)?,
    )
    .map_err(|_| Error::InvalidInput)?;
    if format!("{:x}", Sha256::digest(canonical.as_bytes())) != call.canonical_input_digest_sha256 {
        return Err(Error::InvalidInput);
    }
    let identity = serde_json::to_vec(&(
        &session.conversation_id,
        session.input_revision,
        contract.digest(),
        &original.id,
        call.canonical_input_digest_sha256,
    ))
    .map_err(|_| Error::InvalidInput)?;
    let request_id = format!("task-permission-{:x}", Sha256::digest(identity));
    let planning = crate::chat::ToolCall {
        id: original.id.clone(),
        name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
        arguments_json: serde_json::json!({ "items": [{
            "item_id": original.id, "provider_id": call.provider_id, "tool_name": call.tool_name,
            "expected_effect": call.effect, "resource_scope": call.resource_scope,
            "operation_scope": call.operation_scope,
            "exact_input": serde_json::from_str::<serde_json::Value>(&canonical).map_err(|_| Error::InvalidInput)?,
            "suggested_ttl_seconds": ttl.min(300), "suggested_max_uses": 1,
            "reason": "This scheduled operation requires approval beyond its automatic permission."
        }] }).to_string(),
    };
    let request = crate::permission_tools::build_permission_request(
        &planning,
        registry,
        request_id,
        session.input_revision,
        created_at,
    )
    .map_err(|_| Error::InvalidInput)?;
    let item = request.items.first().ok_or(Error::InvalidInput)?;
    if request.items.len() != 1
        || item.canonical_input_json.as_deref() != Some(canonical.as_str())
        || item.canonical_input_digest_sha256.as_deref() != Some(call.canonical_input_digest_sha256)
        || !super::subset(&item.resource_scope, call.resource_scope)
        || !super::subset(call.resource_scope, &item.resource_scope)
        || !super::subset(&item.operation_scope, call.operation_scope)
        || !super::subset(call.operation_scope, &item.operation_scope)
        || !super::subset(&item.export_destinations, call.export_destinations)
        || !super::subset(call.export_destinations, &item.export_destinations)
    {
        return Err(Error::InvalidScope);
    }
    validate_request(contract, session, &request, provider_device_id)?;
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

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use crate::chat::{ChatMessage, ChatRole, ToolCall};

    #[test]
    fn candidate_requires_unique_unchanged_original_call() {
        let original = ToolCall {
            id: "call-1".into(),
            name: "send_slack_web_exact".into(),
            arguments_json: "{\"body_plain_text\":\"Original\"}".into(),
        };
        let mut message = ChatMessage::text("proposal", ChatRole::Assistant, "");
        message.tool_calls.push(original.to_ref());
        assert!(unique_original_call(&[message.clone()], &original));
        assert!(!unique_original_call(&[], &original));
        assert!(!unique_original_call(
            &[message.clone(), message.clone()],
            &original
        ));
        let changed = ToolCall {
            arguments_json: "{\"body_plain_text\":\"Replacement\"}".into(),
            ..original
        };
        assert!(!unique_original_call(&[message], &changed));
    }
}

fn validate_artifact_resources(
    contract: &super::ValidatedTaskContract,
    session: &PersistedAgentSession,
    rule: &super::TaskPermissionRule,
    canonical: &str,
    resources: &[String],
    now: u64,
) -> Result<(), super::TaskContractError> {
    if !matches!(
        rule.input,
        super::TaskInputConstraint::GeneratedTextArtifact { .. }
    ) {
        return Ok(());
    }
    let step = contract
        .contract()
        .steps
        .iter()
        .find(|step| step.rule_id == rule.rule_id)
        .ok_or(super::TaskContractError::InvalidSteps)?;
    let tool = crate::chat::ToolCall {
        id: "artifact-permission".into(),
        name: rule.tool_name.clone(),
        arguments_json: canonical.into(),
    };
    let stable =
        super::artifact::bind_directory(contract, session, &tool, &step.step_id, resources, now)?;
    if !super::subset(&stable, &rule.approval_ceiling.resources) {
        return Err(super::TaskContractError::InvalidScope);
    }
    Ok(())
}
