//! Closed raw-input fallback and exact authority shared by both orchestrators.

use super::*;
use desk_agent_protocol::computer_use::RawInputAction;

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "raw-input fallback or original application reference is unavailable",
        false,
        true,
    )
}

pub fn raw_input_from_call(call: &ToolCall) -> Result<(ObjectRef, RawInputAction), AgentError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        target: ObjectRef,
        action: RawInputAction,
    }
    if call.name != crate::device_assistant::EXECUTE_CONFIRMED_RAW_INPUT_TOOL
        || call.arguments_json.len() > 64 * 1024
    {
        return Err(unavailable());
    }
    let input: Input = serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    if input.target.object_kind != ObjectKind::Application
        || input.target.token.trim().is_empty()
        || input.target.snapshot_id.trim().is_empty()
        || chrono::DateTime::parse_from_rfc3339(&input.target.expires_at).is_err()
        || input.action.validate().is_err()
    {
        return Err(unavailable());
    }
    Ok((input.target, input.action))
}

/// Parsing does not establish native application ownership or screen freshness.
/// The original edge must still resolve the opaque reference and re-observe the
/// foreground application, selected display and DPI immediately before input.
pub struct RawInputCallPreflight {
    target: ObjectRef,
    action: RawInputAction,
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

impl RawInputCallPreflight {
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
            || capability.required_capability != Capability::DesktopInputFallbackConfirmed
            || capability.wire.authorization_hint.resources
                != [AuthorizationResourceKind::FreshObjectReference]
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
        {
            return Err(unavailable());
        }
        let (target, action) = raw_input_from_call(call)?;
        let expiry = chrono::DateTime::parse_from_rfc3339(&target.expires_at)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .filter(|expiry| now_unix_ms > 0 && *expiry > now_unix_ms)
            .ok_or_else(unavailable)?;
        let canonical_input_json = canonical_tool_permission_input_json(
            &call.name,
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?,
        )
        .map_err(|_| unavailable())?;
        let canonical_input_digest_sha256 =
            format!("{:x}", Sha256::digest(canonical_input_json.as_bytes()));
        let operation_scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        )
        .ok_or_else(unavailable)?
        .operations;
        Ok(Self {
            resource_scope: fresh_object_resource_scope(std::slice::from_ref(&target)),
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
    pub fn action(&self) -> &RawInputAction {
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

#[cfg(test)]
mod tests;
