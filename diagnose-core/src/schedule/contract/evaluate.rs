use super::*;

/// Constructed from provider-validated input, durable step state and trusted lineage.
/// Model arguments alone are never a source of these scope/count/identity fields.
pub struct TaskCall<'a> {
    pub target_device_id: &'a str,
    pub provider_id: &'a str,
    pub capability_id: &'a str,
    pub tool_name: &'a str,
    pub tool_schema_version: u16,
    pub effect: CapabilityEffect,
    pub risk_tier: CapabilityRiskTier,
    pub input: &'a Value,
    pub resources: &'a [String],
    pub operations: &'a [String],
    pub export_destinations: &'a [DestinationIdentity],
    pub byte_count: u64,
    pub item_count: u32,
    pub rule_call_count: u32,
    pub run_call_count: u32,
    pub step_id: Option<&'a str>,
    pub step_states: &'a std::collections::BTreeMap<String, TaskStepStatus>,
    pub message_destination: Option<&'a TaskMessageDestination>,
    pub source_scopes: &'a [String],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskDenyReason {
    NoMatchingRule,
    Identity,
    Input,
    Step,
    Destination,
    SourceScope,
    HardBoundary,
    Budget,
    ApprovalDisabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskDecision {
    Allowed { rule_id: String },
    ApprovalRequired { rule_id: String },
    Denied(TaskDenyReason),
}

fn fits(call: &TaskCall<'_>, scope: &TaskPermissionScope) -> bool {
    !call.resources.is_empty()
        && !call.operations.is_empty()
        && subset(call.resources, &scope.resources)
        && subset(call.operations, &scope.operations)
        && subset(call.export_destinations, &scope.export_destinations)
        && call.byte_count <= scope.limits.max_bytes_per_call
        && call.item_count <= scope.limits.max_items_per_call
        && call.rule_call_count < scope.limits.max_calls
}

impl ValidatedTaskContract {
    /// This classification does not reserve a budget or grant permission to dispatch.
    pub fn evaluate(&self, call: &TaskCall<'_>) -> TaskDecision {
        use TaskDecision::Denied;
        let Some(rule) = self.contract.permissions.iter().find(|rule| {
            rule.provider_id == call.provider_id
                && rule.tool_name == call.tool_name
                && rule.tool_schema_version == call.tool_schema_version
        }) else {
            return Denied(TaskDenyReason::NoMatchingRule);
        };
        if self.contract.target_device_id != call.target_device_id
            || rule.capability_id != call.capability_id
            || rule.effect != call.effect
            || rule.risk_tier != call.risk_tier
        {
            return Denied(TaskDenyReason::Identity);
        }
        if call.run_call_count >= self.contract.budget.max_calls_per_run {
            return Denied(TaskDenyReason::Budget);
        }
        let step = self
            .contract
            .steps
            .iter()
            .find(|step| step.rule_id == rule.rule_id);
        match (step, call.step_id) {
            (Some(step), Some(id))
                if step.step_id == id
                    && call.step_states.get(id) == Some(&TaskStepStatus::Pending)
                    && step
                        .depends_on
                        .iter()
                        .all(|id| call.step_states.get(id) == Some(&TaskStepStatus::Succeeded)) => {
            }
            (None, None) => {}
            _ => return Denied(TaskDenyReason::Step),
        }
        if !call.input.is_object() {
            return Denied(TaskDenyReason::Input);
        }
        match &rule.input {
            TaskInputConstraint::Exact { canonical_json } => {
                if canonical(call.input.clone()).as_ref().ok() != Some(canonical_json) {
                    return Denied(TaskDenyReason::Input);
                }
            }
            TaskInputConstraint::ScopedRead => {}
            TaskInputConstraint::GeneratedTextArtifact {
                file_name,
                max_content_bytes,
            } => {
                let Ok(artifact) =
                    serde_json::from_value::<TaskGeneratedTextArtifact>(call.input.clone())
                else {
                    return Denied(TaskDenyReason::Input);
                };
                if artifact.file_name != *file_name
                    || artifact.content_utf8.len() > *max_content_bytes as usize
                {
                    return Denied(TaskDenyReason::Input);
                }
                let Some(TaskFixedStep {
                    binding:
                        TaskStepBinding::ProduceTextArtifact {
                            allowed_source_scopes,
                            ..
                        },
                    ..
                }) = step
                else {
                    return Denied(TaskDenyReason::Step);
                };
                if call.source_scopes.is_empty()
                    || !subset(call.source_scopes, allowed_source_scopes)
                {
                    return Denied(TaskDenyReason::SourceScope);
                }
            }
            TaskInputConstraint::GeneratedMessage {
                max_subject_bytes,
                max_body_bytes,
                ..
            } => {
                let Ok(message) =
                    serde_json::from_value::<TaskGeneratedMessage>(call.input.clone())
                else {
                    return Denied(TaskDenyReason::Input);
                };
                if message.body.is_empty()
                    || message.body.len() > *max_body_bytes as usize
                    || message.subject.len() > *max_subject_bytes as usize
                {
                    return Denied(TaskDenyReason::Input);
                }
                let Some(TaskFixedStep {
                    binding:
                        TaskStepBinding::SendMessage {
                            destination,
                            allowed_source_scopes,
                        },
                    ..
                }) = step
                else {
                    return Denied(TaskDenyReason::Step);
                };
                if call.message_destination != Some(destination)
                    || call.export_destinations != rule.automatic.export_destinations
                    || (destination.channel == CommunicationChannel::Chat
                        && !message.subject.is_empty())
                {
                    return Denied(TaskDenyReason::Destination);
                }
                if !subset(call.source_scopes, allowed_source_scopes) {
                    return Denied(TaskDenyReason::SourceScope);
                }
            }
        }
        if !fits(call, &rule.approval_ceiling) {
            return Denied(TaskDenyReason::HardBoundary);
        }
        if fits(call, &rule.automatic) {
            return TaskDecision::Allowed {
                rule_id: rule.rule_id.clone(),
            };
        }
        match self.contract.exception_mode {
            TaskExceptionMode::RequestApproval => TaskDecision::ApprovalRequired {
                rule_id: rule.rule_id.clone(),
            },
            TaskExceptionMode::Deny => Denied(TaskDenyReason::ApprovalDisabled),
        }
    }
}
