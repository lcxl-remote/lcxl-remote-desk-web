//! Strict Outlook (new) manual compose handoff preflight.

use super::{ProviderCallSubject, classify_provider_call, unavailable};
use crate::{
    capability_grant::{
        CapabilityGrantCall, canonical_compiled_scope, fresh_object_resource_scope,
    },
    chat::ToolCall,
    communication::canonicalize_email_address,
    permission_tools::canonical_tool_permission_input_json,
    provider_registry::{CapabilityDescriptor, ProviderRegistry},
};
use desk_agent_protocol::{
    AgentError, Capability,
    capability_provider::{AuthorizationResourceKind, ProductSurface},
    communication::{
        COMMUNICATION_SCHEMA_VERSION, CommunicationChannel, CommunicationSurfaceKind,
        CommunicationSurfaceRef, CommunicationSurfaceScope, OutlookNewComposeHandoffRequest,
        OutlookNewDraftHandoffInput, RecipientRole,
    },
    computer_use::{ComputerActionKind, ObjectKind, ObjectRef},
    data_lineage::DestinationIdentity,
};
use sha2::{Digest, Sha256};

/// Closed Outlook input and immutable server-derived authority. The request
/// always stops at a visible manual compose surface and never carries send
/// authority or attachment paths.
pub struct OutlookCallPreflight {
    request: OutlookNewComposeHandoffRequest,
    target: ObjectRef,
    capability: CapabilityDescriptor,
    provider_id: String,
    surface: ProductSurface,
    canonical_input_json: String,
    canonical_input_digest_sha256: String,
    resource_scope: Vec<String>,
    operation_scope: Vec<String>,
    export_destinations: Vec<DestinationIdentity>,
    risk_tier: desk_agent_protocol::capability_grant::CapabilityRiskTier,
    valid_until_unix_ms: u64,
}

impl OutlookCallPreflight {
    pub fn build(
        registry: &ProviderRegistry,
        product_surface: ProductSurface,
        call: &ToolCall,
        server_call_id: &str,
        run_id: &str,
        target_device_id: &str,
        interactive_session_incarnation: &str,
        readiness_revision: u64,
        application: &ObjectRef,
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        let capability = registry
            .capability_for_tool(&call.name)
            .filter(|_| call.name == "prepare_outlook_new_draft_handoff")
            .ok_or_else(unavailable)?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(unavailable)?;
        if !matches!(
            product_surface,
            ProductSurface::OssPersonalOwner | ProductSurface::ManagerPersonalOwner
        ) || !capability.wire.surfaces.contains(&product_surface)
            || capability.wire.authorization_hint.resources
                != [AuthorizationResourceKind::FreshObjectReference]
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
            || [
                server_call_id,
                run_id,
                target_device_id,
                interactive_session_incarnation,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
            || readiness_revision == 0
            || application.object_kind != ObjectKind::Application
            || application.token.trim().is_empty()
            || application.snapshot_id.trim().is_empty()
        {
            return Err(unavailable());
        }
        let expiry = chrono::DateTime::parse_from_rfc3339(&application.expires_at)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .filter(|expiry| now_unix_ms > 0 && *expiry > now_unix_ms)
            .ok_or_else(unavailable)?;
        let input: OutlookNewDraftHandoffInput =
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
        input.validate().map_err(|_| unavailable())?;
        if input.draft.recipients.iter().any(|recipient| {
            recipient.role == RecipientRole::ChatDestination
                || canonicalize_email_address(&recipient.address).is_err()
        }) {
            return Err(unavailable());
        }
        let communication_surface = CommunicationSurfaceRef {
            channel: CommunicationChannel::Email,
            kind: CommunicationSurfaceKind::OutlookNewDesktop,
            scope: CommunicationSurfaceScope::DesktopApplication {
                application_id: crate::device_assistant::OUTLOOK_NEW_APPLICATION_ID.into(),
            },
            device_id: target_device_id.into(),
            os_session_id: interactive_session_incarnation.into(),
            adapter_id: crate::device_assistant::OUTLOOK_NEW_MAILTO_ADAPTER_ID.into(),
            adapter_version: crate::device_assistant::OUTLOOK_NEW_MAILTO_ADAPTER_VERSION.into(),
            profile_id: interactive_session_incarnation.into(),
            account_id: crate::device_assistant::OUTLOOK_NEW_UNVERIFIED_ACCOUNT_ID.into(),
            revision: readiness_revision,
        };
        let request = OutlookNewComposeHandoffRequest {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            call_id: server_call_id.into(),
            run_id: run_id.into(),
            surface: communication_surface,
            draft: input.draft,
        };
        request.validate().map_err(|_| unavailable())?;
        if ComputerActionKind::Communication(request.clone()).required_capability()
            != capability.required_capability
        {
            return Err(unavailable());
        }
        let input_value = serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
        let canonical_input_json = canonical_tool_permission_input_json(&call.name, input_value)
            .map_err(|_| unavailable())?;
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
        Ok(Self {
            request,
            target: application.clone(),
            capability: capability.clone(),
            provider_id: provider.wire.provider_id.clone(),
            surface: product_surface,
            canonical_input_json,
            canonical_input_digest_sha256,
            resource_scope: fresh_object_resource_scope(std::slice::from_ref(application)),
            operation_scope,
            export_destinations: vec![DestinationIdentity::EmailAccount {
                account_id: crate::device_assistant::OUTLOOK_NEW_UNVERIFIED_ACCOUNT_ID.into(),
            }],
            risk_tier: classify_provider_call(capability, call)?,
            valid_until_unix_ms: expiry,
        })
    }

    pub fn request(&self) -> &OutlookNewComposeHandoffRequest {
        &self.request
    }

    pub fn target(&self) -> &ObjectRef {
        &self.target
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
                .any(|value| value.trim().is_empty())
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
