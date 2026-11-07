//! Native permission preparation for one admitted physical device.
use super::*;
use crate::provider_preflight::{ArtifactCallPreflight, IworkCallPreflight};
use desk_agent_protocol::capability_provider::ProductSurface;

#[allow(clippy::too_many_arguments)]
pub(super) fn permission_resources(
    session: &PersistedAgentSession,
    device: &RequestDevice,
    registry: &ProviderRegistry,
    surface: ProductSurface,
    call: &ToolCall,
    original: Option<&crate::input_read_context::ReadContextSelection>,
    now: u64,
) -> Result<Option<Vec<String>>, AgentError> {
    let target = device
        .read_target
        .as_ref()
        .ok_or_else(crate::directory_tools::unavailable)?;
    if call.name == crate::application_launch::TOOL_NAME {
        let input: desk_agent_protocol::application_launch::LaunchApplicationRequest =
            serde_json::from_str(&call.arguments_json)
                .map_err(|_| crate::directory_tools::unavailable())?;
        input
            .validate()
            .map_err(|_| crate::directory_tools::unavailable())?;
        return Ok(None);
    }
    if call.name == "prepare_outlook_draft" {
        let input: desk_agent_protocol::communication::OutlookNewDraftHandoffInput =
            serde_json::from_str(&call.arguments_json)
                .map_err(|_| crate::directory_tools::unavailable())?;
        input
            .validate()
            .map_err(|_| crate::directory_tools::unavailable())?;
        return Ok(None);
    }
    if call.name == "send_raw_input" {
        require_desktop_action_target(
            session,
            &target.device_id,
            &serde_json::from_str(&call.arguments_json)
                .map_err(|_| crate::application_ui::invalid())?,
        )?;
        return crate::provider_preflight::RawInputCallPreflight::build(
            registry, surface, call, now,
        )
        .map(|_| None);
    }
    if call.name == crate::ai_assistant::linux::OUTPUT_TOOL {
        require_desktop_action_target(
            session,
            &target.device_id,
            &serde_json::from_str(&call.arguments_json)
                .map_err(|_| crate::application_ui::invalid())?,
        )?;
        return crate::provider_preflight::WaylandOutputInputPreflight::from_history(
            registry,
            surface,
            call,
            &session.conversation,
            now,
        )
        .map(|preflight| Some(preflight.resource_scope().to_vec()));
    }
    if ArtifactCallPreflight::supports(&call.name) {
        return ArtifactCallPreflight::from_device_session(
            registry,
            surface,
            session,
            &target.device_id,
            call,
            now,
        )
        .map(|preflight| Some(preflight.resource_scope().to_vec()));
    }
    if crate::provider_preflight::text_file::TextMutationPreflight::supports(&call.name) {
        return crate::provider_preflight::text_file::TextMutationPreflight::from_device_session(
            session,
            &target.device_id,
            surface,
            call,
            now,
        )
        .map(|preflight| Some(preflight.resource_scope().to_vec()));
    }
    if IworkCallPreflight::supports(&call.name) {
        let readiness = device
            .native_readiness
            .as_ref()
            .ok_or_else(crate::directory_tools::unavailable)?;
        return IworkCallPreflight::from_device_session(
            registry,
            surface,
            call,
            original.ok_or_else(crate::directory_tools::unavailable)?,
            session,
            &target.device_id,
            &readiness.interactive_session_incarnation,
            now,
        )
        .map(|preflight| Some(preflight.resource_scope().to_vec()));
    }
    let browser =
        crate::provider_preflight::browser_action_from_call(call, "permission-preflight")?;
    crate::provider_preflight::require_browser_device_page(
        &browser,
        session,
        &target.device_id,
        now,
    )?;
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn permission_request(
    session: &PersistedAgentSession,
    device: &RequestDevice,
    registry: &ProviderRegistry,
    surface: ProductSurface,
    original: Option<&crate::input_read_context::ReadContextSelection>,
    call: &ToolCall,
    id: &str,
    created_at: &str,
    now: u64,
) -> Result<PermissionRequest, AgentError> {
    let invalid = || {
        crate::subagent::invalid(
            "Select one ready device and valid native resources for each permission item",
        )
    };
    let target = device.read_target.as_ref().ok_or_else(invalid)?;
    let original_input: serde_json::Value =
        serde_json::from_str(&call.arguments_json).map_err(|_| invalid())?;
    let mut request = build_permission_request(
        call,
        registry,
        id.into(),
        session.input_revision,
        created_at.into(),
    )?;
    crate::application_ui::bind_request(&mut request, &session.conversation)?;
    crate::permission_tools::include_desktop_action_reads(&mut request, registry)?;
    for (index, item) in request.items.iter_mut().enumerate() {
        let descriptor = registry
            .capability_for_tool(&item.tool_name)
            .ok_or_else(invalid)?;
        if !device.ready_for_read(descriptor, session, original) {
            return Err(invalid());
        }
        if item.canonical_input_json.is_none()
            && crate::permission_tools::permission_input_mode(descriptor) == "optional_exact_input"
        {
            item.selected_device_target = Some(target.clone());
            continue;
        }
        let mut native = ToolCall {
            id: call.id.clone(),
            name: item.tool_name.clone(),
            arguments_json: original_input["items"][index]
                .get("exact_input")
                .map(serde_json::Value::to_string)
                .or_else(|| item.canonical_input_json.clone())
                .unwrap_or_else(|| "{}".into()),
        };
        let mut input = serde_json::from_str(&native.arguments_json).map_err(|_| invalid())?;
        crate::file_model_input::complete_device_input(
            &native.name,
            &mut input,
            session,
            original,
            &target.device_id,
            now,
        )?;
        native.arguments_json = input.to_string();
        require_observed_desktop_target(session, &target.device_id, &native.name, &input)?;
        if crate::application_ui::supports(&native.name) {
            require_desktop_action_target(session, &target.device_id, &input)?;
        } else if selected_action_descriptor(descriptor) {
            if let Some(resources) =
                permission_resources(session, device, registry, surface, &native, original, now)?
            {
                item.resource_scope = resources;
            }
        } else {
            crate::read_tools::build_read_operation(&native)?;
        }
        if let Some(display) = input.get("display").and_then(serde_json::Value::as_str)
            && descriptor.required_capability == Capability::ScreenCaptureCurrent
            && !observed_display(session, &target.device_id, display)
        {
            return Err(invalid());
        }
        if crate::file_model_input::is_read(&native.name) {
            crate::provider_preflight::text_file::validate_read_permission_on_device(
                session,
                &target.device_id,
                &native.name,
                Some(&native.arguments_json),
                now,
                original,
            )?;
        }
        if !crate::application_ui::supports(&native.name) {
            let canonical =
                canonical_selected_read_input(&native.name, input).map_err(|_| invalid())?;
            item.canonical_input_digest_sha256 =
                Some(format!("{:x}", Sha256::digest(canonical.as_bytes())));
            item.canonical_input_json = Some(canonical);
        }
        item.selected_device_target = Some(target.clone());
    }
    if crate::assistant_policy::permits_permission_request(session, &request) {
        Ok(request)
    } else {
        Err(invalid())
    }
}

pub(super) fn require_desktop_action_target(
    session: &PersistedAgentSession,
    device: &str,
    input: &serde_json::Value,
) -> Result<(), AgentError> {
    let mut requested = Vec::new();
    crate::ui_model_ids::references(input, &mut requested);
    if requested.is_empty() {
        return Err(crate::application_ui::invalid());
    }
    let mut observed = Vec::new();
    for message in &session.conversation {
        let source = message.trusted_tool_result();
        let Some(call) = source.tool_call_id.as_deref() else {
            continue;
        };
        let Some(envelope) = &source.data_envelope else {
            continue;
        };
        if message.tool_ok != Some(true)
            || envelope.provenance.source_object_id.as_deref()
                != Some(format!("{device}:{call}").as_str())
        {
            continue;
        }
        if let Some(value) = crate::ui_model_ids::observation_value(&source.text) {
            crate::ui_model_ids::references(&value, &mut observed);
        }
    }
    if requested
        .iter()
        .all(|reference| observed.contains(reference))
    {
        Ok(())
    } else {
        Err(crate::subagent::invalid(
            "Use desktop objects observed on the same selected device",
        ))
    }
}
