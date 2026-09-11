//! Closed background input and application authority shared by both orchestrators.

use super::*;
use desk_agent_protocol::background_input::BackgroundInputAction;

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "background input or original application/window is unavailable",
        false,
        true,
    )
}

pub fn background_input_from_call(
    call: &ToolCall,
) -> Result<(ObjectRef, BackgroundInputAction), AgentError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        target: ObjectRef,
        action: BackgroundInputAction,
        application: ObjectRef,
        #[serde(default, rename = "remaining_steps")]
        _remaining_steps: Option<Vec<serde_json::Value>>,
        #[serde(default)]
        #[serde(rename = "geometry")]
        _geometry: Option<desk_agent_protocol::background_input::WindowInputGeometry>,
    }
    if call.name != crate::device_assistant::EXECUTE_BACKGROUND_INPUT_TOOL
        || call.arguments_json.len() > 64 * 1024
    {
        return Err(unavailable());
    }
    let input: Input = serde_json::from_str(&call.arguments_json).map_err(|_| {
        error(
            AgentErrorKind::InvalidInput,
            r#"Invalid background input. Required: {"application_id":"<app>","window_id":"<window>","action":{"kind":"type_text","text":"hello"}}. Mouse: action {"kind":"click","position":{"x":500,"y":500}} or {"kind":"click","element_id":"<control>"}, exactly one locator. Coordinates are 0–1000 within the observed window. Keys: {"kind":"key_press","key":"ArrowLeft","modifiers":["Command"]}. No input was dispatched."#,
            false,
            true,
        )
    })?;
    if input.target.object_kind != ObjectKind::Window
        || input.target.token.trim().is_empty()
        || input.target.snapshot_id.trim().is_empty()
    {
        return Err(unavailable());
    }
    validate_application(&input.application)?;
    input
        .action
        .validate()
        .map_err(|message| error(AgentErrorKind::InvalidInput, message, false, true))?;
    Ok((input.target, input.action))
}

fn validate_application(application: &ObjectRef) -> Result<(), AgentError> {
    if application.object_kind != ObjectKind::Application
        || application.token.is_empty()
        || application.snapshot_id.is_empty()
    {
        return Err(unavailable());
    }
    Ok(())
}

pub fn background_application_from_call(call: &ToolCall) -> Result<ObjectRef, AgentError> {
    background_input_from_call(call)?;
    let value: serde_json::Value =
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    serde_json::from_value(value["application"].clone()).map_err(|_| unavailable())
}

/// Parsing does not establish native object ownership. The original edge must
/// still resolve its opaque reference and verify application/window ownership.
pub struct BackgroundInputCallPreflight {
    steps: Vec<desk_agent_protocol::computer_use::ComputerActionStep>,
    target: ObjectRef,
    action: BackgroundInputAction,
    geometry: Option<desk_agent_protocol::background_input::WindowInputGeometry>,
    application: ObjectRef,
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

impl BackgroundInputCallPreflight {
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
            || capability.required_capability != Capability::DesktopBackgroundInputConfirmed
            || capability.wire.authorization_hint.resources
                != [AuthorizationResourceKind::FreshObjectReference]
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
        {
            return Err(unavailable());
        }
        let (target, action) = background_input_from_call(call)?;
        let application = background_application_from_call(call)?;
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
        let steps = crate::application_batch::actions(call)?;
        let operation_scope = crate::application_batch::operation_scope(&steps);
        Ok(Self {
            steps,
            resource_scope: crate::application_ui::resource(&application),
            geometry: serde_json::from_str::<serde_json::Value>(&call.arguments_json)
                .ok()
                .and_then(|v| serde_json::from_value(v["geometry"].clone()).ok()),
            application,
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

    pub fn steps(&self) -> &[desk_agent_protocol::computer_use::ComputerActionStep] {
        &self.steps
    }
    pub fn computer_action(&self) -> desk_agent_protocol::computer_use::ComputerActionKind {
        desk_agent_protocol::computer_use::ComputerActionKind::BackgroundInput {
            application: self.application.clone(),
            input: self.action.clone(),
            geometry: self.geometry.clone(),
        }
    }
    pub fn resource_scope(&self) -> &[String] {
        &self.resource_scope
    }

    pub fn target(&self) -> &ObjectRef {
        &self.target
    }
    pub fn action(&self) -> &BackgroundInputAction {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn both_surfaces_bind_application_and_background_action_without_reference_deadline() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let app = json!({"token":"app","snapshot_id":"native","object_kind":"application","expires_at":"2000-01-01T00:00:00Z"});
        let window = json!({"token":"window","snapshot_id":"native","object_kind":"window","expires_at":"2000-01-01T00:00:00Z"});
        let call=ToolCall{id:"input".into(),name:"execute_background_inputs".into(),arguments_json:json!({"application":app,"target":window,"action":{"kind":"type_text","text":"中文🙂"},"geometry":null}).to_string()};
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let input =
                BackgroundInputCallPreflight::build(&registry, surface, &call, 1_800_000_000_000)
                    .unwrap();
            let subject = ProviderCallSubject {
                actor_id: "owner",
                run_id: "run",
                input_revision: 3,
                target_device_id: "device",
                policy_revision: crate::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                readiness_revision: 8,
                now_unix_ms: 1_800_000_000_000,
            };
            let grant = input.grant_call(&subject).unwrap();
            assert_eq!(grant.operation_scope, ["background_input:type_text"]);
            assert_eq!(grant.capability_id, "desktop.input.background.confirmed");
            assert_eq!(
                grant.resource_scope,
                crate::application_ui::resource(&input.application)
            );
            assert_eq!(grant.risk_tier, CapabilityRiskTier::R2);
            assert_eq!(
                input.computer_action().required_capability(),
                Capability::DesktopBackgroundInputConfirmed
            );
        }
        let mut broken = call.clone();
        broken.arguments_json =
            json!({"application":app,"target":window,"action":{"kind":"click"}}).to_string();
        assert!(
            background_input_from_call(&broken)
                .unwrap_err()
                .message
                .contains("exactly one")
        );
        broken.arguments_json =
            json!({"application":app,"target":window,"action":{"kind":"guess"}}).to_string();
        assert!(
            background_input_from_call(&broken)
                .unwrap_err()
                .message
                .contains("application_id")
        );
    }
}
