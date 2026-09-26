//! Whole-output Wayland input and exact authority shared by both orchestrators.

use super::*;
use desk_agent_protocol::computer_use::WaylandOutputInputAction;
mod observation;
pub use observation::{bind_observation, observed_output};

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "whole-output input or original output reference is unavailable",
        false,
        true,
    )
}

pub fn wayland_output_input_from_call(
    call: &ToolCall,
) -> Result<(ObjectRef, WaylandOutputInputAction), AgentError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        target: ObjectRef,
        action: WaylandOutputInputAction,
    }
    if call.name != crate::ai_assistant::linux::OUTPUT_TOOL || call.arguments_json.len() > 64 * 1024
    {
        return Err(unavailable());
    }
    let input: Input = serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    if input.target.object_kind != ObjectKind::DesktopOutput
        || input.target.token.trim().is_empty()
        || input.target.snapshot_id.trim().is_empty()
        || input.action.validate().is_err()
    {
        return Err(unavailable());
    }
    Ok((input.target, input.action))
}

/// Parsing cannot prove output ownership, frame freshness, or input preemption.
/// The original worker must resolve the output reference and verify every gate.
pub struct WaylandOutputInputPreflight {
    target: ObjectRef,
    action: WaylandOutputInputAction,
    capability: CapabilityDescriptor,
    provider_id: String,
    surface: ProductSurface,
    canonical_input_json: String,
    canonical_input_digest_sha256: String,
    resource_scope: Vec<String>,
    operation_scope: Vec<String>,
    risk_tier: CapabilityRiskTier,
    valid_until_unix_ms: u64,
}

impl WaylandOutputInputPreflight {
    pub fn from_history(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        history: &[crate::chat::ChatMessage],
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        let preflight = Self::build(registry, surface, call, now_unix_ms)?;
        let (reference, screen, frame) =
            bind_observation(history, &preflight.target.token, now_unix_ms)?;
        if reference != preflight.target
            || screen != preflight.action.screen
            || frame != preflight.action.frame
        {
            return Err(unavailable());
        }
        Ok(preflight)
    }
    pub fn build(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
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
            || capability.required_capability != Capability::DesktopOutputInputConfirmed
            || capability.wire.authorization_hint.resources
                != [AuthorizationResourceKind::FreshObjectReference]
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
        {
            return Err(unavailable());
        }
        let (target, action) = wayland_output_input_from_call(call)?;
        if now_unix_ms == 0 {
            return Err(unavailable());
        }
        // Object identity has no deadline; grant and dispatch leases still bound execution.
        let expiry = u64::MAX;
        let canonical_input_json = canonical_tool_permission_input_json(
            &call.name,
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?,
        )
        .map_err(|_| unavailable())?;
        let canonical_input_digest_sha256 =
            format!("{:x}", Sha256::digest(canonical_input_json.as_bytes()));
        let operation_scope = vec!["wayland_output_input:exact_step".into()];
        Ok(Self {
            resource_scope: output_resource_scope(&target),
            target,
            action,
            capability: capability.clone(),
            provider_id: provider.wire.provider_id.clone(),
            surface,
            canonical_input_json,
            canonical_input_digest_sha256,
            operation_scope,
            risk_tier: classify_provider_call(capability, call)?,
            valid_until_unix_ms: expiry,
        })
    }

    pub fn target(&self) -> &ObjectRef {
        &self.target
    }
    pub fn resource_scope(&self) -> &[String] {
        &self.resource_scope
    }
    pub fn action(&self) -> &WaylandOutputInputAction {
        &self.action
    }
    pub fn canonical_input_json(&self) -> &str {
        &self.canonical_input_json
    }
    pub fn required_capability(&self) -> Capability {
        self.capability.required_capability
    }
    pub fn valid_until_unix_ms(&self) -> u64 {
        self.valid_until_unix_ms
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
            input_revision: subject.input_revision,
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
            byte_count: self.canonical_input_json.len() as u64,
            item_count: 1,
            policy_revision: subject.policy_revision,
            readiness_revision: subject.readiness_revision,
            now_unix_ms: subject.now_unix_ms,
        })
    }
}

/// A distinct scope namespace prevents application or generic object grants
/// from authorizing an entire output, even if a caller reuses a token string.
pub fn output_resource_scope(target: &ObjectRef) -> Vec<String> {
    let encoded = serde_json::to_vec(target).expect("output reference is serializable");
    vec![format!("wayland_output:{:x}", Sha256::digest(encoded))]
}

pub fn validate_completion(
    completed: &desk_agent_protocol::computer_use::ComputerActionCompleted,
) -> Result<(), AgentError> {
    use desk_agent_protocol::computer_use::ComputerActionResultClass as Class;
    if completed.output.is_some()
        || completed.facts.len() > 1
        || completed
            .facts
            .iter()
            .any(|fact| fact.index != 0 || fact.verified)
    {
        return Err(unavailable());
    }
    match completed.result {
        Class::ChangedButUnverified if completed.facts.len() == 1 && completed.facts[0].changed => {
            Ok(())
        }
        Class::OutcomeUnknown => Ok(()),
        Class::DefinitelyNotStarted
        | Class::StaleObservation
        | Class::PausedByUser
        | Class::NotReady
        | Class::Failed
            if completed.facts.iter().all(|fact| !fact.changed) =>
        {
            Ok(())
        }
        _ => Err(unavailable()),
    }
}
