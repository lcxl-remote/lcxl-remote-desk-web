//! Current compiled Provider bounds; historical execution does not freeze these forever.
use super::*;
use crate::{
    capability_risk::classify_provider_descriptor_floor, chat::ToolCall,
    provider_registry::ProviderRegistry,
};
use desk_agent_protocol::{Capability, capability_provider::ProductSurface};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskCatalogError {
    Identity,
    Unavailable,
    Risk,
    Limits,
    Input,
}

impl ValidatedTaskContract {
    /// One publication check, not a grant. Allowed capabilities must come from
    /// current server policy; resource/destination approval, readiness, owner,
    /// model, budgets and successful rehearsal evidence still require verification.
    pub fn validate_current_catalog(
        &self,
        registry: &ProviderRegistry,
        surface: ProductSurface,
        allowed_capabilities: &[Capability],
    ) -> Result<(), TaskCatalogError> {
        for rule in &self.contract.permissions {
            let capability = registry
                .capability_for_tool(&rule.tool_name)
                .ok_or(TaskCatalogError::Unavailable)?;
            let provider = registry
                .provider_for_capability(&rule.capability_id)
                .ok_or(TaskCatalogError::Identity)?;
            let wire = &capability.wire;
            if provider.wire.provider_id != rule.provider_id
                || wire.capability_id != rule.capability_id
                || wire.input_schema_version != rule.tool_schema_version
                || wire.effect != rule.effect
            {
                return Err(TaskCatalogError::Identity);
            }
            if !wire.surfaces.contains(&surface)
                || !allowed_capabilities.contains(&capability.required_capability)
            {
                return Err(TaskCatalogError::Unavailable);
            }
            let minimum = match &rule.input {
                TaskInputConstraint::Exact { canonical_json } => {
                    crate::provider_preflight::classify_provider_call(
                        capability,
                        &ToolCall {
                            id: rule.rule_id.clone(),
                            name: rule.tool_name.clone(),
                            arguments_json: canonical_json.clone(),
                        },
                    )
                    .map_err(|_| TaskCatalogError::Input)?
                }
                _ => classify_provider_descriptor_floor(wire.effect, &wire.data_policy),
            };
            if (rule.risk_tier as u8) < (minimum as u8) {
                return Err(TaskCatalogError::Risk);
            }
            for scope in [&rule.automatic, &rule.approval_ceiling] {
                if scope.limits.max_bytes_per_call > wire.limits.max_input_bytes
                    || scope.limits.max_items_per_call > wire.limits.max_objects
                {
                    return Err(TaskCatalogError::Limits);
                }
            }
        }
        Ok(())
    }
}
