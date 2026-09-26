//! Linux provider declarations. Runtime evidence still controls readiness.
use super::*;

pub const TEXT_ADAPTER_ID: &str = "file.text.linux.edge";
pub const OUTPUT_PROVIDER_ID: &str = "desktop.output.input";
pub const OUTPUT_CAPABILITY_ID: &str = "desktop.output.input.confirmed";
pub const OUTPUT_ADAPTER_ID: &str = "linux.wayland.output";
pub const OUTPUT_ADAPTER_VERSION: &str = "linux-wayland-output/v2";
pub const OUTPUT_TOOL: &str = "execute_wayland_output_input";

// Central processes can serve devices on any OS; never gate this guidance on
// the OS compiling/running the center. Device readiness remains authoritative.
pub(super) const PROMPT_GUIDANCE: &str = "\nFor Linux devices, desktop providers are limited to the reported, logged-in GNOME Wayland session. File and browser capabilities do not imply desktop input is available. Use only tools exposed by current device readiness and matching grants. AT-SPI nodes do not establish trusted screen coordinates or foreground window ownership. Never infer a pixel-to-application mapping from their bounds. Report the supplied screenshot age/freshness honestly. latest_observed may support best-effort output input while the original local receipt is at most 30 seconds old; it is never proof of a new or unchanged frame and grants no input authority. execute_wayland_output_input, when available, requires the exact device-issued output and original frame under a separate one-shot whole-output grant; application-scoped permission cannot substitute for it. Remote tools cannot create or restore Portal sessions or enable accessibility settings; explain a concrete missing local prerequisite instead of trying another input backend. Never use X11, uinput, evdev, synthetic cursor movement to obtain a frame, or shell commands to bypass unavailable desktop providers.\n";

pub(super) fn supports_adapter(id: &str) -> bool {
    matches!(
        id,
        LINUX_ATSPI_ADAPTER_ID
            | TEXT_ADAPTER_ID
            | DESKTOP_SESSION_ADAPTER_ID
            | FILE_WORKSPACE_ADAPTER_ID
            | SPREADSHEET_FILE_ADAPTER_ID
            | FILE_ARTIFACT_ADAPTER_ID
            | TERMINAL_OUTPUT_ADAPTER_ID
            | CURRENT_SCREEN_ADAPTER_ID
            | SYSTEM_DIAGNOSTICS_ADAPTER_ID
            | SYSTEM_COMMAND_ADAPTER_ID
            | BROWSER_EXTENSION_ADAPTER_ID
            | GMAIL_WEB_ADAPTER_ID
            | SLACK_WEB_ADAPTER_ID
    )
}

pub(super) fn output_tool() -> RegisteredTool {
    let mut tool = send_raw_input_tool();
    tool.spec.name = OUTPUT_TOOL.into();
    tool.required_capability = Capability::DesktopOutputInputConfirmed;
    tool.spec.description = "Execute one bounded input step on an entire authorized GNOME Wayland output. This is whole-screen authority, not application authority: it can affect any application shown on that output. Requires a device-issued output reference, its exact original frame and geometry, and a separate one-shot exact grant. latest_observed is allowed as best-effort evidence, never relabeled fresh. The original local receipt must be at most 30 seconds old at each submission; if expired, observe again and obtain a new exact grant. The device rechecks stream/session identity, geometry, continuity and the locally authorized best-effort input control period. The screen may change despite these checks. A Portal reply does not prove the intended UI effect; never replay an unknown outcome.".into();
    let schema = &mut tool.spec.parameters_schema;
    schema["properties"]["target"]["properties"]["object_kind"]["const"] = json!("desktop_output");
    schema["properties"]["target"]["properties"]["expires_at"] = json!({"type":"string"});
    schema["properties"]["action"]["properties"]["frame"] = json!({
        "type":"object",
        "properties": {
            "stream_generation":{"type":"integer","minimum":1},
            "observation_id":{"type":"string","minLength":1,"maxLength":128},
            "received_at_unix_ms":{"type":"integer","minimum":1},
            "freshness":{"type":"string","enum":["fresh","unchanged_verified","latest_observed"]}
        },
        "required":["stream_generation","observation_id","received_at_unix_ms","freshness"],
        "additionalProperties":false
    });
    schema["properties"]["action"]["required"] = json!(["screen", "frame", "step"]);
    tool
}

pub(super) fn output_provider() -> ProviderDescriptor {
    let mut provider = provider_for_tool(
        crate::tool_exposure::ExposureRequirement::NoAttachment,
        OUTPUT_PROVIDER_ID,
        OUTPUT_CAPABILITY_ID,
        "assistant.capability.waylandOutputInput",
        vec![OUTPUT_ADAPTER_ID.into()],
        ExecutionLocality::Edge,
        CapabilityEffect::InputFallback,
        1,
        Vec::new(),
        vec![
            CapabilityDataCategory::ScreenPixels,
            CapabilityDataCategory::UserRequest,
        ],
        vec![AuthorizationResourceKind::FreshObjectReference],
        output_tool(),
    );
    provider.wire.capabilities[0].prerequisites.platforms = vec![CapabilityPlatform::Linux];
    provider.capabilities[0].wire.prerequisites.platforms = vec![CapabilityPlatform::Linux];
    provider
}

pub(super) fn text_adapter(providers: &ProviderRegistry) -> EdgeAdapterDescriptor {
    EdgeAdapterDescriptor {
        adapter_id: TEXT_ADAPTER_ID.into(),
        adapter_version: TEXT_FILE_ADAPTER_VERSION.into(),
        capability_ids: vec![
            TEXT_FILE_UPDATE_CAPABILITY_ID.into(),
            TEXT_FILE_DELETE_CAPABILITY_ID.into(),
        ],
        limits: providers
            .capability(TEXT_FILE_UPDATE_CAPABILITY_ID)
            .expect("text mutation capability")
            .wire
            .limits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_center_prompt_entrypoints_include_linux_authority_boundaries() {
        for locale in [None, Some("zh-CN"), Some("en-US")] {
            for message in [
                build_ai_assistant_system_message(locale),
                build_ai_assistant_system_message_with_catalog(locale, "fixture catalog"),
            ] {
                assert!(!message.text.contains("one Windows or macOS desktop"));
                assert!(message.text.contains(PROMPT_GUIDANCE));
                assert!(
                    message
                        .text
                        .contains(crate::wait_tools::BACKGROUND_TASK_GUIDANCE)
                );
            }
        }
    }

    #[test]
    fn linux_declarations_preserve_platform_and_authority_boundaries() {
        let registry = ai_assistant_provider_registry();
        for id in [
            DESKTOP_UI_CAPABILITY_ID,
            TEXT_FILE_UPDATE_CAPABILITY_ID,
            CURRENT_SCREEN_CAPABILITY_ID,
            BROWSER_OPEN_CAPABILITY_ID,
        ] {
            assert!(
                registry
                    .capability(id)
                    .unwrap()
                    .wire
                    .prerequisites
                    .platforms
                    .contains(&CapabilityPlatform::Linux)
            );
        }
        let output = registry.capability(OUTPUT_CAPABILITY_ID).unwrap();
        assert_eq!(
            output.required_capability,
            Capability::DesktopOutputInputConfirmed
        );
        assert_eq!(
            output.wire.prerequisites.platforms,
            vec![CapabilityPlatform::Linux]
        );
        assert_eq!(output.adapter_ids, vec![OUTPUT_ADAPTER_ID.to_owned()]);
        assert!(
            !registry
                .capability(DESKTOP_RAW_INPUT_CAPABILITY_ID)
                .unwrap()
                .wire
                .prerequisites
                .platforms
                .contains(&CapabilityPlatform::Linux)
        );
        let adapters = ai_assistant_edge_adapter_registry();
        assert_eq!(
            adapters
                .adapter(LINUX_ATSPI_ADAPTER_ID)
                .unwrap()
                .adapter_version,
            LINUX_ATSPI_ADAPTER_VERSION
        );
        assert!(valid_ui_adapter(
            &desk_agent_protocol::computer_use::ComputerUseAdapterRef {
                kind: desk_agent_protocol::computer_use::ComputerUseAdapterKind::LinuxAtspi,
                version: LINUX_ATSPI_ADAPTER_VERSION.into(),
            }
        ));
    }
}
