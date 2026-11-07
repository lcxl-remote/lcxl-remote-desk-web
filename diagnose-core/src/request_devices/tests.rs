use super::*;
use crate::dynamic_run::PermissionRequestState;
use crate::provider_registry::TargetBinding;
use crate::seam::ToolRunOutput;
use desk_agent_protocol::capability_grant::CapabilityRiskTier;
use desk_agent_protocol::{AgentScope, ExecutionMode};
use serde_json::{Value, json};

const NOW: &str = "2026-11-06T00:00:00Z";

struct NoDeviceCommands;

#[async_trait::async_trait(?Send)]
impl ToolSeam for NoDeviceCommands {
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        unreachable!("planning a permission cannot run a tool")
    }
}

fn session() -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "run",
        "owner",
        "",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        NOW,
    );
    session.surface = crate::session::AgentSessionSurface::AiAssistant;
    session.input_revision = 1;
    session
        .bind_assistant_request_targets(vec!["a".into(), "b".into()])
        .unwrap();
    session
}

fn device(reference: &str, ready_provider_ids: Vec<String>) -> RequestDevice {
    RequestDevice {
        read_target: Some(SelectedReadTarget {
            device_id: reference.into(),
            target_ref: reference.into(),
            connection_id: format!("connection-{reference}"),
            readiness_revision: 1,
        }),
        ready_provider_ids,
        approved_read_tools: vec![],
        target_ref: reference.into(),
        name: None,
        platform: None,
        online: true,
        ready_capabilities: vec![],
    }
}

fn tool(name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: name.into(),
        arguments_json: input.to_string(),
    }
}

fn proposal(name: &str, reference: &str, input: Value) -> ToolCall {
    tool(
        REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
        json!({
            "target_ref": reference,
            "items": [{
                "item_id": "item", "tool_name": name, "exact_input": input,
                "reason": "Read the selected device for the owner's task",
                "suggested_ttl_seconds": 120, "suggested_max_uses": 1
            }]
        }),
    )
}

#[tokio::test]
async fn every_request_device_read_projects_parses_and_keeps_its_exact_target() {
    let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
    for descriptor in registry
        .providers()
        .flat_map(|p| &p.capabilities)
        .filter(|d| d.target_binding == TargetBinding::RequestDevice)
    {
        let name = descriptor.wire.tool_name.as_str();
        if name == crate::command_confirmation::COMMAND_TOOL {
            continue;
        }
        let args = match name {
            "read_system_info"
            | "read_network_ports"
            | "read_recent_logs"
            | "read_container_list"
            | "inspect_desktop_session"
            | "read_current_screen" => json!({}),
            "read_process_list" | "read_service_status" | "list_applications" => {
                json!({"allow_unfiltered":true})
            }
            "inspect_desktop_ui" => json!({"queries":["Calendar"]}),
            _ => panic!("{name}: provide a native call fixture for the new selected tool"),
        };
        assert!(selected_read_descriptor(descriptor), "{name}");
        assert!(
            !has_target_selector(&descriptor.tool_spec),
            "{name}: selector conflicts with native schema"
        );
        let mut session = session();
        let devices = [
            device("a", vec![descriptor.wire.capability_id.clone()]),
            device("b", vec![]),
        ];
        let reads = read_tools(&devices, registry, true);
        let [(registered, callable)] = reads.as_slice() else {
            panic!("{name}: missing projected tool");
        };
        assert_eq!(registered.name(), name);
        let mut model_spec = registered.spec.clone();
        crate::ui_model_ids::project_tool(&mut model_spec);
        assert!(
            model_spec.parameters_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("target_ref")),
            "{name}"
        );
        if matches!(name, "read_current_screen" | "inspect_desktop_ui") {
            for field in NATIVE_OBJECT_FIELDS {
                assert!(
                    model_spec.parameters_schema["properties"]
                        .get(field)
                        .is_none(),
                    "{name}: leaked {field}"
                );
            }
        }
        assert!(
            read_tools(&devices[1..], registry, true).is_empty(),
            "{name}"
        );
        if name == "read_current_screen" {
            assert!(read_tools(&devices, registry, false).is_empty());
        }
        let mut wrapped = args.clone();
        wrapped["target_ref"] = json!("a");
        let (selector, native) =
            selected_read(&tool(name, wrapped)).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(selector, "a");
        assert_eq!(
            serde_json::from_str::<Value>(&native.arguments_json).unwrap(),
            args
        );
        let (capability, _) = crate::read_tools::build_read_operation(&native)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(capability, descriptor.required_capability, "{name}");
        let risk = crate::provider_preflight::classify_provider_call(descriptor, &native).unwrap();
        let target = devices[0].read_target.as_ref().unwrap();
        if risk == CapabilityRiskTier::R0 {
            assert!(*callable, "{name}");
            require_current_read_target(&session, &native, target).unwrap();
            continue;
        }
        assert!(
            !callable,
            "{name}: sensitive read was advertised before approval"
        );
        let denied = plan_device_permission(
            &NoDeviceCommands,
            Some(registry),
            &session,
            &devices,
            &reads,
            &proposal(name, "b", args.clone()),
            "p",
            NOW,
        )
        .await;
        assert!(
            denied.unwrap().is_err(),
            "{name}: device B does not report readiness"
        );
        let mut approved = plan_device_permission(
            &NoDeviceCommands,
            Some(registry),
            &session,
            &devices,
            &reads,
            &proposal(name, "a", args),
            "p",
            NOW,
        )
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(approved.selected_read_target(), Some(target), "{name}");
        assert!(
            require_current_read_target(&session, &native, target).is_err(),
            "{name}"
        );
        approved.state = PermissionRequestState::Approved;
        session.permission_requests.push(approved);
        require_current_read_target(&session, &native, target)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(
            require_current_read_target(
                &session,
                &native,
                devices[1].read_target.as_ref().unwrap()
            )
            .is_err(),
            "{name}: A approval authorized B"
        );
        assert!(
            require_current_read_target(
                &session,
                &native,
                &SelectedReadTarget {
                    connection_id: "reconnected".into(),
                    ..target.clone()
                }
            )
            .is_err(),
            "{name}: approval survived a changed connection"
        );
        assert!(
            require_current_read_target(
                &session,
                &native,
                &SelectedReadTarget {
                    readiness_revision: 2,
                    ..target.clone()
                }
            )
            .is_err(),
            "{name}: approval survived changed readiness"
        );
    }
}

#[test]
fn process_arguments_cannot_escalate_an_unapproved_read() {
    let target = device("a", vec![]).read_target.unwrap();
    let session = session();
    let basic = tool("read_process_list", json!({"allow_unfiltered":true}));
    require_current_read_target(&session, &basic, &target).unwrap();
    let sensitive = tool(
        "read_process_list",
        json!({
            "allow_unfiltered":true, "include_command_line":true
        }),
    );
    assert!(require_current_read_target(&session, &sensitive, &target).is_err());
    assert!(
        require_observed_desktop_target(
            &session,
            "a",
            "read_current_screen",
            &json!({"window_id":"w1"})
        )
        .is_err()
    );
}

#[tokio::test]
async fn desktop_object_approval_and_dispatch_keep_the_observed_device_and_incarnation() {
    use crate::chat::{ChatMessage, ChatRole};
    use crate::model_message_labels::{ReadResultLabel, read_result_envelope};
    use crate::seam::ToolOutputFormat;

    let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
    let screen = registry.capability_for_tool("read_current_screen").unwrap();
    let mut session = session();
    let devices = [
        device("a", vec![screen.wire.capability_id.clone()]),
        device("b", vec![screen.wire.capability_id.clone()]),
    ];
    let window = |snapshot: &str| {
        json!({"token":"same-window-token","snapshot_id":snapshot,
            "object_kind":"window","expires_at":""})
    };
    for (device, snapshot) in [("a", "snapshot-a"), ("b", "snapshot-b")] {
        let mut call = tool("inspect_desktop_ui", json!({}));
        call.id = format!("observe-{device}");
        let content = json!({"ReadContext":{"DesktopUiInspect":{"owner_selectable_windows":[
            {"title":"Identical title","object_ref":window(snapshot)}
        ]}}})
        .to_string();
        let output = ToolRunOutput {
            format: ToolOutputFormat::Json,
            content: content.clone(),
            image_data_url: None,
            document_preview: None,
        };
        let mut observation =
            ChatMessage::text(format!("result-{device}"), ChatRole::Tool, content);
        observation.tool_call_id = Some(call.id.clone());
        observation.tool_ok = Some(true);
        observation.data_envelope = Some(
            read_result_envelope(
                registry,
                &call,
                &output,
                ReadResultLabel {
                    envelope_id: format!("envelope-{device}"),
                    observation_id: format!("observation-{device}"),
                    source_object_id: Some(format!("{device}:{}", call.id)),
                    observed_at_unix_ms: 1,
                },
            )
            .unwrap(),
        );
        let mut assistant = ChatMessage::text(format!("inspect-{device}"), ChatRole::Assistant, "");
        assistant.tool_calls.push(call.to_ref());
        session.conversation.extend([assistant, observation]);
    }
    session
        .ui_references
        .observe(&session.conversation)
        .unwrap();
    let resolve = |call: &ToolCall, session: &PersistedAgentSession| {
        crate::ui_model_ids::resolve_session_call_with_context(call, session, 1, None).unwrap()
    };
    let reads = read_tools(&devices, registry, true);
    let input_a = json!({"window":window("snapshot-a")});
    let input_b = json!({"window":window("snapshot-b")});
    let propose_window = |alias: &str| {
        tool(
            REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
            json!({
                "target_ref":"a",
                "items":[{"tool_name":"read_current_screen","exact_input":{"window_id":alias},
                    "reason":"Capture the selected window"}]
            }),
        )
    };
    let proposal_a = resolve(&propose_window("w1"), &session);
    let proposal_b = resolve(&propose_window("w2"), &session);
    let mut request = plan_device_permission(
        &NoDeviceCommands,
        Some(registry),
        &session,
        &devices,
        &reads,
        &proposal_a,
        "permission-a",
        NOW,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        plan_device_permission(
            &NoDeviceCommands,
            Some(registry),
            &session,
            &devices,
            &reads,
            &proposal_b,
            "permission-b",
            NOW,
        )
        .await
        .unwrap()
        .is_err()
    );
    request.state = PermissionRequestState::Approved;
    session.permission_requests.push(request);
    let a = devices[0].read_target.as_ref().unwrap();
    let b = devices[1].read_target.as_ref().unwrap();
    let (reference, native) = selected_read(&resolve(
        &tool(
            "read_current_screen",
            json!({"target_ref":"a","window_id":"w1"}),
        ),
        &session,
    ))
    .unwrap();
    assert_eq!(reference, "a");
    assert_eq!(
        serde_json::from_str::<Value>(&native.arguments_json).unwrap(),
        input_a
    );
    require_current_read_target(&session, &native, a).unwrap();
    assert!(
        require_current_read_target(&session, &tool("read_current_screen", input_b.clone()), a)
            .is_err()
    );
    let mut reused_token = session.conversation.last().unwrap().clone();
    reused_token
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_object_id = Some("a:observe-b".into());
    session.conversation.push(reused_token);
    assert!(
        require_observed_desktop_target(&session, "a", "read_current_screen", &input_b).is_ok()
    );
    assert!(
        require_current_read_target(&session, &tool("read_current_screen", input_b.clone()), a)
            .is_err(),
        "a newly observed incarnation on the same device needs a new approval"
    );
    assert!(
        require_current_read_target(&session, &tool("read_current_screen", input_b), b).is_err()
    );
}

#[tokio::test]
async fn explicit_display_must_have_been_observed_on_the_selected_device() {
    use crate::chat::{ChatMessage, ChatRole};
    use crate::model_message_labels::{ReadResultLabel, read_result_envelope};
    use crate::seam::ToolOutputFormat;

    let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
    let desktop = registry
        .capability_for_tool("inspect_desktop_session")
        .unwrap();
    let screen = registry.capability_for_tool("read_current_screen").unwrap();
    let mut session = session();
    let devices = [
        device(
            "a",
            vec![
                desktop.wire.capability_id.clone(),
                screen.wire.capability_id.clone(),
            ],
        ),
        device("b", vec![screen.wire.capability_id.clone()]),
    ];
    let reads = read_tools(&devices, registry, true);
    let content = json!({"ReadContext":{"DesktopSessionInspect":{"displays":[
        {"display":"opaque-one","name":"One"}, {"display":"opaque-two","name":"Two"}
    ]}}})
    .to_string();
    let output = ToolRunOutput {
        format: ToolOutputFormat::Json,
        content: content.clone(),
        image_data_url: None,
        document_preview: None,
    };
    let mut observation = ChatMessage::text("desktop-result", ChatRole::Tool, content);
    observation.tool_call_id = Some("call".into());
    observation.data_envelope = Some(
        read_result_envelope(
            registry,
            &tool("inspect_desktop_session", json!({})),
            &output,
            ReadResultLabel {
                envelope_id: "envelope".into(),
                observation_id: "observation".into(),
                source_object_id: Some("a:call".into()),
                observed_at_unix_ms: 1,
            },
        )
        .unwrap(),
    );
    let resolved = observation.clone();
    observation.text = r#"{"result_externalized":true}"#.into();
    observation.tool_ok = Some(true);
    observation.resolved_result = Some(Box::new(resolved));
    session.conversation.push(observation);
    for (target, display, permitted) in [
        ("a", "opaque-two", true),
        ("b", "opaque-two", false),
        ("a", "invented", false),
    ] {
        let request = plan_device_permission(
            &NoDeviceCommands,
            Some(registry),
            &session,
            &devices,
            &reads,
            &proposal("read_current_screen", target, json!({"display":display})),
            "p",
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(request.is_ok(), permitted, "{target}/{display}");
    }
}
