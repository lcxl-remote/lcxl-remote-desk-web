//! Exact native-launch authority shared by durable orchestration runtimes.
use super::{CAPABILITY_ID, LaunchApprovalBinding, PROVIDER_ID, TOOL_NAME, approved_binding};
use crate::capability_grant::CapabilityGrantCall;
use crate::{
    chat::ToolCall, provider_preflight::ProviderCallSubject, session::PersistedAgentSession,
};
use desk_agent_protocol::capability_grant::CapabilityRiskTier;
use desk_agent_protocol::capability_provider::{CapabilityEffect, ProductSurface};
use desk_agent_protocol::{AgentError, Capability};
use sha2::{Digest, Sha256};

pub struct LaunchCallPreflight {
    binding: LaunchApprovalBinding,
    canonical: String,
    digest: String,
    resources: Vec<String>,
    operations: Vec<String>,
    surface: ProductSurface,
}
impl LaunchCallPreflight {
    pub fn build(
        session: &PersistedAgentSession,
        call: &ToolCall,
        readiness_revision: u64,
        surface: ProductSurface,
    ) -> Result<Self, AgentError> {
        let binding = approved_binding(session, call, readiness_revision)?;
        let canonical = crate::permission_tools::canonical_tool_permission_input_json(
            TOOL_NAME,
            serde_json::to_value(binding.request())
                .map_err(|_| super::permission_error("Invalid launch input"))?,
        )
        .map_err(|_| super::permission_error("Invalid launch input"))?;
        Ok(Self {
            resources: binding.resource_scope(),
            binding,
            digest: format!("{:x}", Sha256::digest(canonical.as_bytes())),
            canonical,
            operations: vec![TOOL_NAME.into()],
            surface,
        })
    }
    pub fn binding(&self) -> &LaunchApprovalBinding {
        &self.binding
    }
    pub fn canonical_input_json(&self) -> &str {
        &self.canonical
    }
    pub fn required_capability(&self) -> Capability {
        Capability::ApplicationLaunchConfirmed
    }
    pub fn valid_until_unix_ms(&self) -> u64 {
        u64::MAX
    }
    pub fn grant_call<'a>(
        &'a self,
        subject: &'a ProviderCallSubject<'_>,
    ) -> Result<CapabilityGrantCall<'a>, AgentError> {
        let frozen = self.binding.subject();
        if frozen.actor_id != subject.actor_id
            || frozen.device_id != subject.target_device_id
            || frozen.input_revision != subject.input_revision
            || frozen.policy_revision != subject.policy_revision
            || frozen.readiness_revision != subject.readiness_revision
            || subject.now_unix_ms == 0
        {
            return Err(super::permission_error("Native launch approval changed"));
        }
        Ok(CapabilityGrantCall {
            actor_id: subject.actor_id,
            run_id: subject.run_id,
            input_revision: subject.input_revision,
            surface: self.surface,
            target_device_id: subject.target_device_id,
            target_session_id: None,
            provider_id: PROVIDER_ID,
            capability_id: CAPABILITY_ID,
            tool_name: TOOL_NAME,
            tool_schema_version: 1,
            effect: CapabilityEffect::LaunchApplication,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: &self.resources,
            operation_scope: &self.operations,
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: &self.digest,
            byte_count: self.canonical.len() as u64,
            item_count: 1,
            policy_revision: subject.policy_revision,
            readiness_revision: subject.readiness_revision,
            now_unix_ms: subject.now_unix_ms,
        })
    }
}
