//! Derived authority for bounded device reads, never a grant or send permit.

use super::*;
use crate::input_read_context::object_read::{ObjectReadBinding, requires_objects};
use desk_agent_protocol::capability_provider::{CapabilityEffect, ExecutionLocality};

pub mod limits;

pub struct ReadCallPreflight {
    capability: CapabilityDescriptor,
    provider_id: String,
    surface: ProductSurface,
    canonical_input_digest_sha256: String,
    root_count: u32,
    resource_scope: Vec<String>,
    operation_scope: Vec<String>,
    risk_tier: CapabilityRiskTier,
    valid_until_unix_ms: u64,
}

impl ReadCallPreflight {
    /// The original binding is loaded from the accepted input. The runtime must
    /// independently verify current owner, input, lease, model and readiness.
    pub fn build(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        binding: &ObjectReadBinding<'_>,
    ) -> Result<Self, AgentError> {
        binding.original.validate()?;
        let capability = registry
            .capability_for_tool(&call.name)
            .ok_or_else(unavailable)?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(unavailable)?;
        if !matches!(
            surface,
            ProductSurface::OssPersonalOwner | ProductSurface::ManagerPersonalOwner
        ) || !capability.wire.surfaces.contains(&surface)
            || capability.wire.execution_locality != ExecutionLocality::Edge
            || !matches!(
                capability.wire.effect,
                CapabilityEffect::ReadDevice
                    | CapabilityEffect::ReadFile
                    | CapabilityEffect::CaptureScreen
            )
            || !binding.original.tool_names.contains(&call.name)
            || call.id.trim().is_empty()
            || call.id.len() > 512
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
        {
            return Err(unavailable());
        }
        let (cap, mut operation) = build_read_operation(call)?;
        if cap != capability.required_capability {
            return Err(unavailable());
        }
        let canonical = canonical_tool_permission_input_json(
            &call.name,
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?,
        )
        .map_err(|_| unavailable())?;
        if canonical.len() > capability.wire.limits.max_input_bytes as usize {
            return Err(unavailable());
        }
        let mut scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        )
        .ok_or_else(unavailable)?;
        let mut deadline = binding
            .now_unix_ms
            .checked_add(120_000)
            .ok_or_else(unavailable)?;
        if let Some(expiry) = &binding.original.expires_at {
            deadline = deadline.min(
                chrono::DateTime::parse_from_rfc3339(expiry)
                    .ok()
                    .and_then(|d| u64::try_from(d.timestamp_millis()).ok())
                    .ok_or_else(unavailable)?,
            );
        }
        let mut root_count = 1;
        if requires_objects(&call.name) {
            binding.bind(call, &mut operation)?;
            deadline = deadline.min(binding.expiry(call)?);
            let refs = if crate::input_read_context::live_read::target_kind(&call.name).is_some() {
                vec![
                    crate::input_read_context::live_read::target(
                        binding.original,
                        &call.name,
                        binding.now_unix_ms,
                    )?
                    .object_ref
                    .clone(),
                ]
            } else {
                binding
                    .selected(call)?
                    .iter()
                    .map(|object| {
                        serde_json::from_str::<ObjectRef>(&object.object_ref.opaque_token)
                            .map_err(|_| unavailable())
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            root_count = u32::try_from(refs.len()).map_err(|_| unavailable())?;
            scope.resources = fresh_object_resource_scope(&refs);
        } else if capability.wire.authorization_hint.resources
            != [AuthorizationResourceKind::TargetDevice]
        {
            return Err(unavailable());
        }
        if binding.now_unix_ms == 0 || deadline <= binding.now_unix_ms {
            return Err(unavailable());
        }
        Ok(Self {
            capability: capability.clone(),
            provider_id: provider.wire.provider_id.clone(),
            surface,
            canonical_input_digest_sha256: format!("{:x}", Sha256::digest(canonical.as_bytes())),
            root_count,
            resource_scope: scope.resources,
            operation_scope: scope.operations,
            risk_tier: classify_provider_call(capability, call)?,
            valid_until_unix_ms: deadline,
        })
    }

    pub fn valid_until_unix_ms(&self) -> u64 {
        self.valid_until_unix_ms
    }

    pub fn resource_scope(&self) -> &[String] {
        &self.resource_scope
    }

    pub fn output_limits(&self) -> desk_agent_protocol::capability_grant::CapabilityGrantLimits {
        limits::descriptor_limits(&self.capability)
    }

    pub fn grant_call<'a>(
        &'a self,
        subject: &'a ProviderCallSubject<'_>,
    ) -> Result<CapabilityGrantCall<'a>, AgentError> {
        crate::assistant_policy::require_current_policy(subject.policy_revision)?;
        if subject.readiness_revision == 0
            || subject.now_unix_ms == 0
            || subject.now_unix_ms >= self.valid_until_unix_ms
            || [subject.actor_id, subject.run_id, subject.target_device_id]
                .iter()
                .any(|id| id.trim().is_empty())
        {
            return Err(unavailable());
        }
        Ok(CapabilityGrantCall {
            actor_id: subject.actor_id,
            run_id: subject.run_id,
            surface: self.surface,
            target_device_id: subject.target_device_id,
            target_session_id: None,
            provider_id: &self.provider_id,
            capability_id: &self.capability.wire.capability_id,
            tool_name: &self.capability.wire.tool_name,
            tool_schema_version: self.capability.wire.input_schema_version,
            effect: self.capability.wire.effect,
            risk_tier: self.risk_tier,
            resource_scope: &self.resource_scope,
            operation_scope: &self.operation_scope,
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: &self.canonical_input_digest_sha256,
            // Output size is unknown before dispatch. The runtime must enforce
            // the original grant's output limits before retaining any result.
            byte_count: 0,
            item_count: self.root_count,
            policy_revision: subject.policy_revision,
            readiness_revision: subject.readiness_revision,
            now_unix_ms: subject.now_unix_ms,
        })
    }
}

#[cfg(test)]
mod tests;
