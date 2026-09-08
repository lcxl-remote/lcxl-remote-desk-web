//! Build an unapproved contract from server-verified observations.
use super::*;
use crate::{provider_preflight::ObservedCapabilityAuthority, provider_registry::ProviderRegistry};
use desk_agent_protocol::capability_grant::CapabilityGrantLimits;

pub enum ObservedMessage<'a> {
    Sent(&'a crate::communication_handoff::SentWebMessageEvidence),
    Prepared(&'a crate::communication_handoff::PreparedWebMessageEvidence),
}

pub fn observed_rule(
    contract: &mut TaskContract,
    observed: &ObservedCapabilityAuthority,
    canonical_input: Option<&str>,
    message: Option<ObservedMessage<'_>>,
    registry: &ProviderRegistry,
) -> Result<(), TaskContractError> {
    let descriptor = registry
        .capability_for_tool(&observed.tool_name)
        .ok_or(TaskContractError::InvalidIdentity)?;
    let id = format!("rule-{}", contract.permissions.len() + 1);
    let scope = TaskPermissionScope {
        resources: observed.resources.clone(),
        operations: observed.operations.clone(),
        export_destinations: observed.export_destinations.clone(),
        limits: CapabilityGrantLimits {
            max_bytes_per_call: descriptor.wire.limits.max_input_bytes,
            max_items_per_call: descriptor.wire.limits.max_objects,
            max_calls: 1,
        },
    };
    let mut rule = TaskPermissionRule {
        rule_id: id.clone(),
        provider_id: observed.provider_id.clone(),
        capability_id: observed.capability_id.clone(),
        tool_name: observed.tool_name.clone(),
        tool_schema_version: observed.tool_schema_version,
        effect: observed.effect,
        risk_tier: observed.risk_tier,
        input: TaskInputConstraint::ScopedRead,
        automatic: scope.clone(),
        approval_ceiling: scope,
    };
    let binding = if let Some(message) = message {
        let (snapshot, subject, body, effect) = match message {
            ObservedMessage::Sent(sent) => (
                &sent.snapshot,
                &sent.subject,
                &sent.body_plain_text,
                CapabilityEffect::SendExternal,
            ),
            ObservedMessage::Prepared(prepared) => (
                &prepared.snapshot,
                &prepared.subject,
                &prepared.body_plain_text,
                CapabilityEffect::WriteExternalDraft,
            ),
        };
        if observed.effect != effect {
            return Err(TaskContractError::InvalidInput);
        }
        let surface = &snapshot.payload.surface;
        let destination = TaskMessageDestination {
            channel: surface.channel,
            surface_kind: surface.kind,
            scope: surface.scope.clone(),
            adapter_id: surface.adapter_id.clone(),
            adapter_version: surface.adapter_version.clone(),
            profile_id: surface.profile_id.clone(),
            account_id: surface.account_id.clone(),
            recipients: snapshot.payload.recipients.clone(),
        };
        rule.automatic.resources =
            task_message_resource_scope(&contract.target_device_id, &destination)?;
        rule.approval_ceiling.resources = rule.automatic.resources.clone();
        rule.input = TaskInputConstraint::GeneratedMessage {
            attachment_policy: attachment::observed_attachment_policy(
                &snapshot.payload.attachments,
            )?,
            max_subject_bytes: u32::try_from(subject.len().max(256))
                .map_err(|_| TaskContractError::InvalidInput)?,
            max_body_bytes: u32::try_from(body.len().max(4096))
                .map_err(|_| TaskContractError::InvalidInput)?,
        };
        Some(TaskStepBinding::SendMessage {
            destination,
            allowed_source_scopes: vec![crate::schedule::rehearsal::fixed_input::task_input_scope(
                &validate_contract(contract)?,
            )],
        })
    } else if matches!(
        observed.effect,
        CapabilityEffect::ReadDevice | CapabilityEffect::ReadFile | CapabilityEffect::ReadExternal
    ) {
        None
    } else {
        let canonical_json = canonical_input.ok_or(TaskContractError::InvalidInput)?;
        if format!("{:x}", Sha256::digest(canonical_json.as_bytes()))
            != observed.canonical_input_sha256
        {
            return Err(TaskContractError::InvalidInput);
        }
        rule.input = TaskInputConstraint::Exact {
            canonical_json: canonical_json.into(),
        };
        Some(TaskStepBinding::Exact)
    };
    if let Some(previous) = contract
        .permissions
        .iter_mut()
        .find(|item| item.tool_name == rule.tool_name)
    {
        let mut compare = rule.clone();
        compare.rule_id = previous.rule_id.clone();
        compare.automatic.limits.max_calls = previous.automatic.limits.max_calls;
        compare.approval_ceiling.limits.max_calls = previous.approval_ceiling.limits.max_calls;
        // Multiple distinct mutations need a separately reviewed execution plan.
        if binding.is_some() || *previous != compare {
            return Err(TaskContractError::ConflictingRules);
        }
        previous.automatic.limits.max_calls = previous
            .automatic
            .limits
            .max_calls
            .checked_add(1)
            .ok_or(TaskContractError::InvalidLimits)?;
        previous.approval_ceiling.limits.max_calls = previous.automatic.limits.max_calls;
        return Ok(());
    }
    if let Some(binding) = binding {
        contract.steps.push(TaskFixedStep {
            step_id: format!("step-{}", contract.steps.len() + 1),
            rule_id: id,
            depends_on: contract
                .steps
                .last()
                .map(|step| vec![step.step_id.clone()])
                .unwrap_or_default(),
            binding,
        });
    }
    contract.permissions.push(rule);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_reads_keep_observed_scope_and_reject_a_different_scope() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let descriptor = registry.capability_for_tool("read_system_info").unwrap();
        let observed = ObservedCapabilityAuthority {
            target_session_id: None,
            envelope_ids: vec![],
            content_digests_sha256: vec![],
            provider_id: registry
                .provider_for_capability(&descriptor.wire.capability_id)
                .unwrap()
                .wire
                .provider_id
                .clone(),
            capability_id: descriptor.wire.capability_id.clone(),
            tool_name: "read_system_info".into(),
            tool_schema_version: descriptor.wire.input_schema_version,
            effect: descriptor.wire.effect,
            risk_tier: crate::capability_risk::classify_provider_descriptor_floor(
                descriptor.wire.effect,
                &descriptor.wire.data_policy,
            ),
            canonical_input_sha256: format!("{:x}", Sha256::digest(b"{}")),
            resources: vec!["device:1".into()],
            operations: vec!["read".into()],
            export_destinations: vec![],
        };
        let mut contract = TaskContract {
            schema_version: 1,
            schedule_id: "task".into(),
            task_revision: 1,
            contract_revision: 1,
            target_device_id: "1".into(),
            prompt_sha256: "a".repeat(64),
            permissions: vec![],
            steps: vec![],
            exception_mode: TaskExceptionMode::Deny,
            budget: crate::schedule::TASK_PUBLICATION_BUDGET,
        };
        observed_rule(&mut contract, &observed, None, None, &registry).unwrap();
        observed_rule(&mut contract, &observed, None, None, &registry).unwrap();
        assert_eq!(contract.permissions.len(), 1);
        assert_eq!(contract.permissions[0].automatic.limits.max_calls, 2);
        assert!(
            validate_contract(&contract)
                .unwrap()
                .observed_scoped_read_rule("1", &observed)
                .is_some()
        );
        let before = contract.clone();
        let mut different = observed;
        different.resources.push("unobserved".into());
        assert!(observed_rule(&mut contract, &different, None, None, &registry).is_err());
        assert_eq!(contract, before);
    }
}

/// Generalize only text content after verifying the original create receipt and
/// owner-selected directory. The resulting contract still requires review.
pub fn generalize_text_artifact(
    contract: &mut TaskContract,
    session: &crate::session::PersistedAgentSession,
    tool: &crate::chat::ToolCall,
    observed: &ObservedCapabilityAuthority,
    output: &desk_agent_protocol::computer_use::CreatedFileArtifactOutput,
    completed_at: u64,
) -> Result<(), TaskContractError> {
    let canonical_input = crate::permission_tools::canonical_tool_permission_input_json(
        &tool.name,
        serde_json::from_str(&tool.arguments_json).map_err(|_| TaskContractError::InvalidInput)?,
    )
    .map_err(|_| TaskContractError::InvalidInput)?;
    if session.device_id != contract.target_device_id
        || tool.name != observed.tool_name
        || format!("{:x}", Sha256::digest(canonical_input.as_bytes()))
            != observed.canonical_input_sha256
    {
        return Err(TaskContractError::InvalidIdentity);
    }
    crate::schedule::source_graph::attachment::verify_text_artifact_output(
        &tool.name,
        &canonical_input,
        output,
    )
    .map_err(|_| TaskContractError::InvalidInput)?;
    let generated = artifact::generated_text_input(tool)?;
    let path = artifact::observed_directory(session, tool, &observed.resources, completed_at)?;
    let scope = artifact::directory_resource_scope(&session.device_id, path)?;
    let input_scope =
        crate::schedule::rehearsal::fixed_input::task_input_scope(&validate_contract(contract)?);
    let rule = contract
        .permissions
        .iter_mut()
        .find(|rule| rule.tool_name == tool.name)
        .ok_or(TaskContractError::InvalidIdentity)?;
    let step = contract
        .steps
        .iter_mut()
        .find(|step| step.rule_id == rule.rule_id)
        .ok_or(TaskContractError::InvalidSteps)?;
    rule.input = TaskInputConstraint::GeneratedTextArtifact {
        file_name: generated.file_name,
        max_content_bytes: u32::try_from(generated.content_utf8.len().max(4096))
            .map_err(|_| TaskContractError::InvalidLimits)?,
    };
    rule.automatic.resources = scope.clone();
    rule.approval_ceiling.resources = scope;
    step.binding = TaskStepBinding::ProduceTextArtifact {
        canonical_directory: path.into(),
        allowed_source_scopes: vec![input_scope],
    };
    Ok(())
}
