//! Server-derived Provider call facts shared by runtime-specific executors.
//!
//! These checks do not issue grants, consume uses, seal plans or authorize a
//! network send. Runtimes still verify current owner/session/readiness, source
//! lineage and destination policy under their durable dispatch fences.

use desk_agent_protocol::{
    AgentError, AgentErrorKind, Capability, ContextKind, OperationInput, ReadContextInput,
    browser_control::BrowserActionRequest,
    capability_grant::CapabilityRiskTier,
    capability_provider::{AuthorizationResourceKind, CapabilityDataCategory, ProductSurface},
    computer_use::{ComputerActionKind, ObjectKind, ObjectRef},
    data_lineage::DestinationIdentity,
};
use sha2::{Digest, Sha256};

use crate::{
    capability_grant::{
        CapabilityGrantCall, canonical_compiled_scope, fresh_object_resource_scope,
    },
    capability_risk::{CapabilityRiskSignals, classify_capability_risk},
    chat::ToolCall,
    permission_tools::canonical_tool_permission_input_json,
    provider_registry::{CapabilityDescriptor, ProviderRegistry},
    read_tools::build_read_operation,
};

mod browser_input;
pub use browser_input::browser_action_from_call;
mod semantic_ui;
pub use semantic_ui::{UiCallPreflight, ui_action_from_call};
mod semantic_raw_input;
pub use semantic_raw_input::{RawInputCallPreflight, raw_input_from_call};
mod semantic_iwork;
pub use semantic_iwork::IworkCallPreflight;
pub mod read;

/// Identity and clock resolved by the runtime, never deserialized from tool input.
pub struct ProviderCallSubject<'a> {
    pub actor_id: &'a str,
    pub run_id: &'a str,
    pub target_device_id: &'a str,
    pub policy_revision: i64,
    pub readiness_revision: u64,
    pub now_unix_ms: u64,
}

/// Closed browser input and immutable derived authority. Private fields prevent
/// callers from replacing scope/risk while retaining a validated action.
pub struct BrowserCallPreflight {
    request: BrowserActionRequest,
    capability: CapabilityDescriptor,
    provider_id: String,
    surface: ProductSurface,
    canonical_input_json: String,
    canonical_input_digest_sha256: String,
    resource_scope: Vec<String>,
    operation_scope: Vec<String>,
    export_destinations: Vec<DestinationIdentity>,
    risk_tier: CapabilityRiskTier,
    valid_until_unix_ms: u64,
}

impl BrowserCallPreflight {
    /// `browser_surface` must come from fresh, connection-fenced edge readiness.
    /// The opaque reference is not verified or minted by this pure evaluator.
    pub fn build(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        server_call_id: &str,
        browser_surface: &ObjectRef,
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
            || capability.wire.authorization_hint.resources
                != [AuthorizationResourceKind::FreshObjectReference]
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
            || browser_surface.object_kind != ObjectKind::BrowserSurface
            || browser_surface.token.trim().is_empty()
            || browser_surface.snapshot_id.trim().is_empty()
        {
            return Err(unavailable());
        }
        let expiry = chrono::DateTime::parse_from_rfc3339(&browser_surface.expires_at)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .filter(|expiry| now_unix_ms > 0 && *expiry > now_unix_ms)
            .ok_or_else(unavailable)?;
        let request = browser_action_from_call(call, server_call_id)?;
        if ComputerActionKind::Browser(request.clone()).required_capability()
            != capability.required_capability
        {
            return Err(unavailable());
        }
        let input = serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
        let canonical_input_json =
            canonical_tool_permission_input_json(&call.name, input).map_err(|_| unavailable())?;
        if canonical_input_json.len() > capability.wire.limits.max_input_bytes as usize {
            return Err(unavailable());
        }
        let canonical_input_digest_sha256 =
            format!("{:x}", Sha256::digest(canonical_input_json.as_bytes()));
        let operation_scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        )
        .ok_or_else(unavailable)?
        .operations;
        let export_destinations = match call.name.as_str() {
            "prepare_gmail_web_draft_handoff" => vec![DestinationIdentity::EmailAccount {
                account_id: crate::device_assistant::GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID.into(),
            }],
            "prepare_slack_web_message_handoff" => vec![DestinationIdentity::ChatAccount {
                account_id: crate::device_assistant::SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID.into(),
            }],
            _ => vec![],
        };
        Ok(Self {
            request,
            capability: capability.clone(),
            provider_id: provider.wire.provider_id.clone(),
            surface,
            canonical_input_json,
            canonical_input_digest_sha256,
            resource_scope: fresh_object_resource_scope(std::slice::from_ref(browser_surface)),
            operation_scope,
            export_destinations,
            risk_tier: classify_provider_call(capability, call)?,
            valid_until_unix_ms: expiry,
        })
    }

    pub fn request(&self) -> &BrowserActionRequest {
        &self.request
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
            export_destinations: &self.export_destinations,
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

pub fn classify_provider_call(
    capability: &CapabilityDescriptor,
    call: &ToolCall,
) -> Result<CapabilityRiskTier, AgentError> {
    let process_command_line_requested = capability
        .wire
        .data_policy
        .reads
        .contains(&CapabilityDataCategory::ProcessMetadata)
        && matches!(
            build_read_operation(call)?,
            (
                Capability::ProcessList,
                OperationInput::ReadContext(ReadContextInput {
                    kind: ContextKind::ProcessList(desk_agent_protocol::ProcessListParams {
                        include_command_line: true,
                        ..
                    }),
                })
            )
        );
    let sensitive_content = capability.wire.data_policy.reads.iter().any(|category| {
        matches!(
            category,
            CapabilityDataCategory::UiSemanticTree
                | CapabilityDataCategory::OfficeSelection
                | CapabilityDataCategory::FileContent
                | CapabilityDataCategory::TerminalOutput
                | CapabilityDataCategory::ScreenPixels
                | CapabilityDataCategory::LogContent
                | CapabilityDataCategory::CommandOutput
                | CapabilityDataCategory::ExternalContent
                | CapabilityDataCategory::CommunicationContent
                | CapabilityDataCategory::LiveDocumentContent
        ) || (*category == CapabilityDataCategory::ProcessMetadata
            && process_command_line_requested)
    });
    Ok(classify_capability_risk(
        capability.wire.effect,
        CapabilityRiskSignals {
            sensitive_content,
            external_egress: capability.wire.data_policy.may_export_data,
            destructive_or_overwrite: false,
            unpredictable_input: false,
        },
    ))
}

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "browser Provider input or selected surface is unavailable",
        false,
        true,
    )
}

fn error(
    kind: AgentErrorKind,
    message: impl Into<String>,
    retryable: bool,
    safe_for_model: bool,
) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model,
        error_code: None,
    }
}

#[cfg(test)]
mod tests;
