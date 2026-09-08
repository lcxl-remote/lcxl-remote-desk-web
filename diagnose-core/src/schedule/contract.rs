//! Deterministic task-contract validation. Provider adapters resolve input and stable scopes first.
pub mod artifact;
pub mod attachment;
pub mod draft;
pub mod exception;
use desk_agent_protocol::{
    browser_control::BrowserOriginKind,
    capability_grant::{CapabilityRiskTier, MAX_GRANT_SCOPE_VALUES, MAX_GRANT_USES},
    capability_provider::CapabilityEffect,
    communication::{
        CommunicationChannel, CommunicationSurfaceKind, CommunicationSurfaceScope, GMAIL_WEB_HOST,
        MAX_BODY_BYTES, MAX_SUBJECT_BYTES, RecipientRole, SLACK_WEB_HOST,
    },
    data_lineage::DestinationIdentity,
    schedule::contract::*,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

mod catalog;
pub use catalog::TaskCatalogError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskContractError {
    InvalidJson,
    TooLarge,
    UnsupportedVersion,
    InvalidIdentity,
    InvalidScope,
    InvalidLimits,
    InvalidInput,
    ConflictingRules,
    InvalidSteps,
    InvalidDestination,
}

/// Only the validating constructor produces this type; wire data is never already approved.
#[derive(Debug, Clone)]
pub struct ValidatedTaskContract {
    contract: TaskContract,
    canonical_json: String,
    digest: String,
}

pub fn parse_contract(json: &str) -> Result<ValidatedTaskContract, TaskContractError> {
    if json.len() > MAX_TASK_CONTRACT_BYTES {
        return Err(TaskContractError::TooLarge);
    }
    let source: Value = serde_json::from_str(json).map_err(|_| TaskContractError::InvalidJson)?;
    let contract: TaskContract =
        serde_json::from_value(source.clone()).map_err(|_| TaskContractError::InvalidJson)?;
    let known = serde_json::to_value(&contract).map_err(|_| TaskContractError::InvalidJson)?;
    reject_unknown_fields(&source, &known)?;
    validate_contract(&contract)
}

fn reject_unknown_fields(source: &Value, known: &Value) -> Result<(), TaskContractError> {
    match (source, known) {
        (Value::Object(source), Value::Object(known)) => {
            for (key, value) in source {
                let known = known.get(key).ok_or(TaskContractError::InvalidJson)?;
                reject_unknown_fields(value, known)?;
            }
        }
        (Value::Array(source), Value::Array(known)) => {
            if source.len() != known.len() {
                return Err(TaskContractError::InvalidJson);
            }
            for (source, known) in source.iter().zip(known) {
                reject_unknown_fields(source, known)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn id(value: &str) -> Result<(), TaskContractError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(TaskContractError::InvalidIdentity);
    }
    Ok(())
}
fn revision(value: u64) -> Result<(), TaskContractError> {
    if value == 0 || value > i64::MAX as u64 {
        return Err(TaskContractError::InvalidIdentity);
    }
    Ok(())
}
fn digest(value: &str) -> Result<(), TaskContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(TaskContractError::InvalidIdentity);
    }
    Ok(())
}
fn ids(values: &mut [String], allow_empty: bool) -> Result<(), TaskContractError> {
    if (!allow_empty && values.is_empty()) || values.len() > MAX_GRANT_SCOPE_VALUES {
        return Err(TaskContractError::InvalidScope);
    }
    for value in values.iter() {
        id(value)?;
    }
    values.sort();
    if values.windows(2).any(|v| v[0] == v[1]) {
        return Err(TaskContractError::InvalidScope);
    }
    Ok(())
}
fn scope(value: &mut TaskPermissionScope) -> Result<(), TaskContractError> {
    ids(&mut value.resources, false)?;
    ids(&mut value.operations, false)?;
    if value.export_destinations.len() > MAX_GRANT_SCOPE_VALUES {
        return Err(TaskContractError::InvalidScope);
    }
    for destination in &value.export_destinations {
        destination
            .validate()
            .map_err(|_| TaskContractError::InvalidDestination)?;
    }
    value.export_destinations.sort();
    if value.export_destinations.windows(2).any(|v| v[0] == v[1]) {
        return Err(TaskContractError::InvalidScope);
    }
    let limits = &value.limits;
    if limits.max_calls == 0
        || limits.max_calls > MAX_GRANT_USES
        || limits.max_bytes_per_call == 0
        || limits.max_bytes_per_call > i64::MAX as u64
        || limits.max_items_per_call == 0
    {
        return Err(TaskContractError::InvalidLimits);
    }
    Ok(())
}
fn subset<T: PartialEq>(values: &[T], allowed: &[T]) -> bool {
    values.iter().all(|v| allowed.contains(v))
}
fn within(inner: &TaskPermissionScope, outer: &TaskPermissionScope) -> bool {
    subset(&inner.resources, &outer.resources)
        && subset(&inner.operations, &outer.operations)
        && subset(&inner.export_destinations, &outer.export_destinations)
        && inner.limits.max_calls <= outer.limits.max_calls
        && inner.limits.max_bytes_per_call <= outer.limits.max_bytes_per_call
        && inner.limits.max_items_per_call <= outer.limits.max_items_per_call
}
fn sorted_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sorted_json(v)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sorted_json).collect()),
        other => other,
    }
}
fn canonical(value: Value) -> Result<String, TaskContractError> {
    serde_json::to_string(&sorted_json(value)).map_err(|_| TaskContractError::InvalidJson)
}
fn destination(value: &TaskMessageDestination) -> Result<(), TaskContractError> {
    for value in [
        &value.adapter_id,
        &value.adapter_version,
        &value.profile_id,
        &value.account_id,
    ] {
        id(value)?;
    }
    // A browser profile or OS session is not a fixed signed-in account.
    if [
        crate::device_assistant::GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID,
        crate::device_assistant::SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID,
        crate::device_assistant::OUTLOOK_NEW_UNVERIFIED_ACCOUNT_ID,
    ]
    .contains(&value.account_id.as_str())
    {
        return Err(TaskContractError::InvalidDestination);
    }
    // These are the reviewed exact-send surfaces; other adapters need their own contract support.
    if value.surface_kind != CommunicationSurfaceKind::ChromeExtension
        || value.recipients.len() != 1
    {
        return Err(TaskContractError::InvalidDestination);
    }
    let CommunicationSurfaceScope::WebOrigin { origin } = &value.scope else {
        return Err(TaskContractError::InvalidDestination);
    };
    origin
        .validate()
        .map_err(|_| TaskContractError::InvalidDestination)?;
    let recipient = &value.recipients[0];
    recipient
        .validate()
        .map_err(|_| TaskContractError::InvalidDestination)?;
    if origin.kind != BrowserOriginKind::Https
        || origin.port != 443
        || !matches!(
            (value.channel, origin.host_ascii.as_str(), recipient.role),
            (
                CommunicationChannel::Email,
                GMAIL_WEB_HOST,
                RecipientRole::To
            ) | (
                CommunicationChannel::Chat,
                SLACK_WEB_HOST,
                RecipientRole::ChatDestination
            )
        )
    {
        return Err(TaskContractError::InvalidDestination);
    }
    Ok(())
}

pub fn validate_contract(input: &TaskContract) -> Result<ValidatedTaskContract, TaskContractError> {
    if serde_json::to_vec(input)
        .map_err(|_| TaskContractError::InvalidJson)?
        .len()
        > MAX_TASK_CONTRACT_BYTES
    {
        return Err(TaskContractError::TooLarge);
    }
    if input.schema_version != TASK_CONTRACT_SCHEMA_VERSION {
        return Err(TaskContractError::UnsupportedVersion);
    }
    id(&input.schedule_id)?;
    id(&input.target_device_id)?;
    revision(input.task_revision)?;
    revision(input.contract_revision)?;
    digest(&input.prompt_sha256)?;
    let budget = &input.budget;
    if !super::policy::valid_budget(budget) {
        return Err(TaskContractError::InvalidLimits);
    }
    if input.permissions.len() > MAX_TASK_PERMISSION_RULES
        || input.steps.len() > MAX_TASK_FIXED_STEPS
    {
        return Err(TaskContractError::TooLarge);
    }
    let mut contract = input.clone();
    let mut rule_ids = BTreeSet::new();
    let mut tools = BTreeSet::new();
    for rule in &mut contract.permissions {
        for value in [
            &rule.rule_id,
            &rule.provider_id,
            &rule.capability_id,
            &rule.tool_name,
        ] {
            id(value)?;
        }
        if rule.tool_schema_version == 0
            || !rule_ids.insert(rule.rule_id.clone())
            || !tools.insert((
                rule.provider_id.clone(),
                rule.tool_name.clone(),
                rule.tool_schema_version,
            ))
        {
            return Err(TaskContractError::ConflictingRules);
        }
        scope(&mut rule.automatic)?;
        scope(&mut rule.approval_ceiling)?;
        if !within(&rule.automatic, &rule.approval_ceiling)
            || rule.approval_ceiling.limits.max_calls > budget.max_calls_per_run
        {
            return Err(TaskContractError::InvalidLimits);
        }
        match &mut rule.input {
            TaskInputConstraint::Exact { canonical_json } => {
                if canonical_json.len() > 32 * 1024 {
                    return Err(TaskContractError::TooLarge);
                }
                let value: Value = serde_json::from_str(canonical_json)
                    .map_err(|_| TaskContractError::InvalidInput)?;
                if !value.is_object() {
                    return Err(TaskContractError::InvalidInput);
                }
                *canonical_json = canonical(value)?;
            }
            TaskInputConstraint::ScopedRead => {
                if !matches!(
                    rule.effect,
                    CapabilityEffect::ReadDevice
                        | CapabilityEffect::ReadFile
                        | CapabilityEffect::ReadExternal
                ) {
                    return Err(TaskContractError::InvalidInput);
                }
            }
            TaskInputConstraint::GeneratedTextArtifact {
                file_name,
                max_content_bytes,
            } => {
                if rule.effect != CapabilityEffect::WriteArtifact
                    || rule.tool_name != "create_text_artifact_in_selected_directory"
                    || *max_content_bytes == 0
                    || *max_content_bytes > 65_536
                {
                    return Err(TaskContractError::InvalidInput);
                }
                let probe = crate::chat::ToolCall {
                    id: "artifact-contract".into(),
                    name: rule.tool_name.clone(),
                    arguments_json: serde_json::json!({"file_name":file_name,"content_utf8":""})
                        .to_string(),
                };
                crate::provider_preflight::artifact_action_from_call(&probe)
                    .map_err(|_| TaskContractError::InvalidInput)?;
            }
            TaskInputConstraint::GeneratedMessage {
                max_subject_bytes,
                max_body_bytes,
                attachment_policy,
            } => {
                if let Some(policy) = attachment_policy {
                    attachment::validate_attachment_policy(policy)?;
                }
                if !matches!(
                    rule.effect,
                    CapabilityEffect::SendExternal | CapabilityEffect::WriteExternalDraft
                ) || *max_subject_bytes as usize > MAX_SUBJECT_BYTES
                    || *max_body_bytes == 0
                    || u64::from(*max_body_bytes) > MAX_BODY_BYTES
                {
                    return Err(TaskContractError::InvalidInput);
                }
            }
        }
    }
    let mut steps = BTreeSet::new();
    let mut step_rules = BTreeSet::new();
    for step in &mut contract.steps {
        id(&step.step_id)?;
        let rule = contract
            .permissions
            .iter()
            .find(|r| r.rule_id == step.rule_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        ids(&mut step.depends_on, true)?;
        if step.depends_on.iter().any(|s| !steps.contains(s))
            || !steps.insert(step.step_id.clone())
            || !step_rules.insert(step.rule_id.clone())
        {
            return Err(TaskContractError::InvalidSteps);
        }
        match (&mut step.binding, &rule.input) {
            (TaskStepBinding::Exact, TaskInputConstraint::Exact { .. }) => {}
            (
                TaskStepBinding::ProduceTextArtifact {
                    canonical_directory,
                    allowed_source_scopes,
                },
                TaskInputConstraint::GeneratedTextArtifact { .. },
            ) => {
                ids(allowed_source_scopes, false)?;
                let resources = artifact::directory_resource_scope(
                    &contract.target_device_id,
                    canonical_directory,
                )?;
                if rule.automatic.resources != resources
                    || rule.approval_ceiling.resources != resources
                {
                    return Err(TaskContractError::InvalidScope);
                }
                if !rule.automatic.export_destinations.is_empty()
                    || !rule.approval_ceiling.export_destinations.is_empty()
                {
                    return Err(TaskContractError::InvalidScope);
                }
            }
            (
                TaskStepBinding::SendMessage {
                    destination: target,
                    allowed_source_scopes,
                },
                TaskInputConstraint::GeneratedMessage { .. },
            ) => {
                destination(target)?;
                let resources = task_message_resource_scope(&contract.target_device_id, target)?;
                if rule.automatic.resources != resources
                    || rule.approval_ceiling.resources != resources
                {
                    return Err(TaskContractError::InvalidScope);
                }
                ids(allowed_source_scopes, false)?;
                let expected = match target.channel {
                    CommunicationChannel::Email => DestinationIdentity::EmailAccount {
                        account_id: target.account_id.clone(),
                    },
                    CommunicationChannel::Chat => DestinationIdentity::ChatAccount {
                        account_id: target.account_id.clone(),
                    },
                    _ => return Err(TaskContractError::InvalidDestination),
                };
                if rule.automatic.export_destinations != [expected.clone()]
                    || rule.approval_ceiling.export_destinations != [expected]
                {
                    return Err(TaskContractError::InvalidDestination);
                }
            }
            _ => return Err(TaskContractError::InvalidSteps),
        }
    }
    for rule in &contract.permissions {
        if rule.effect.is_side_effecting() && !step_rules.contains(&rule.rule_id) {
            return Err(TaskContractError::InvalidSteps);
        }
    }
    contract
        .permissions
        .sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
    let canonical_json =
        canonical(serde_json::to_value(&contract).map_err(|_| TaskContractError::InvalidJson)?)?;
    let digest = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
    Ok(ValidatedTaskContract {
        contract,
        canonical_json,
        digest,
    })
}

impl ValidatedTaskContract {
    pub fn contract(&self) -> &TaskContract {
        &self.contract
    }
    pub fn canonical_json(&self) -> &str {
        &self.canonical_json
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

mod evaluate;
mod message;
pub use evaluate::{TaskCall, TaskDecision, TaskDenyReason};
pub use message::{TaskMessageTarget, task_message_resource_scope};
#[cfg(test)]
mod tests;

impl ValidatedTaskContract {
    /// Match historical exact-input evidence for review, not dispatch authority.
    /// Automatic scope must equal the observed scope: one successful narrow call
    /// cannot prove coverage of additional resources, operations or destinations.
    /// Current policy, limits, step order and provider-specific stable bindings
    /// still require independent verification. Parameterized inputs never match here.
    pub fn observed_exact_rule(
        &self,
        device: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
    ) -> Option<&TaskPermissionRule> {
        self.observed_scope_rule(device, observed).filter(|rule| {
            matches!(&rule.input, TaskInputConstraint::Exact { canonical_json }
                if format!("{:x}", Sha256::digest(canonical_json.as_bytes())) == observed.canonical_input_sha256)
        })
    }

    /// A trusted successful read may cover changing query parameters within the
    /// same reviewed scope. This does not widen resources, operations or exports,
    /// prove the approval ceiling, or replace current provider/policy validation.
    /// Callers must use verified read receipts, never model permission summaries.
    pub fn observed_scoped_read_rule(
        &self,
        device: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
    ) -> Option<&TaskPermissionRule> {
        self.observed_scope_rule(device, observed)
            .filter(|rule| matches!(rule.input, TaskInputConstraint::ScopedRead))
    }

    fn observed_scope_rule(
        &self,
        device: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
    ) -> Option<&TaskPermissionRule> {
        if device != self.contract.target_device_id
            || observed.canonical_input_sha256.len() != 64
            || !observed
                .canonical_input_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || observed.resources.is_empty()
            || observed.operations.is_empty()
        {
            return None;
        }
        self.contract.permissions.iter().find(|rule| {
            rule.provider_id == observed.provider_id
                && rule.capability_id == observed.capability_id
                && rule.tool_name == observed.tool_name
                && rule.tool_schema_version == observed.tool_schema_version
                && rule.effect == observed.effect
                && rule.risk_tier == observed.risk_tier
                && subset(&observed.resources, &rule.automatic.resources)
                && subset(&rule.automatic.resources, &observed.resources)
                && subset(&observed.operations, &rule.automatic.operations)
                && subset(&rule.automatic.operations, &observed.operations)
                && subset(
                    &observed.export_destinations,
                    &rule.automatic.export_destinations,
                )
                && subset(
                    &rule.automatic.export_destinations,
                    &observed.export_destinations,
                )
        })
    }
}
