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

struct OriginalReads(Option<crate::input_read_context::ReadContextSelection>);

#[async_trait::async_trait(?Send)]
impl ToolSeam for OriginalReads {
    fn original_read_context(&self) -> Option<&crate::input_read_context::ReadContextSelection> {
        self.0.as_ref()
    }

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
        read_target: Some(SelectedDeviceTarget {
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
async fn selected_file_reads_project_and_plan_only_on_the_directory_device() {
    use crate::{
        conversation_key::{AssistantConversationScope, derive_assistant_conversation_key},
        file_scope::*,
    };
    use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
    let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
    let mut session = session();
    session.conversation_id = derive_assistant_conversation_key(
        "owner",
        AssistantConversationScope::Personal,
        "file-input",
    );
    session.adopt_client_metadata(
        Some("file-input"),
        crate::session::AgentSessionSurface::AiAssistant,
    );
    let subject = session
        .file_scope_subject("owner", "a", &session.conversation_id)
        .unwrap();
    let input = crate::model_message_labels::model_bound_user_message(
        "owner-input".into(),
        "Read the chosen directory".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "connection".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    session.conversation.push(input);
    let roots = vec!["inspect_files", "read_text_file"]
        .into_iter()
        .map(|name| {
            registry
                .capability_for_tool(name)
                .unwrap()
                .wire
                .capability_id
                .clone()
        })
        .collect::<Vec<_>>();
    let devices = [device("a", roots.clone()), device("b", roots)];
    assert!(read_tools(&devices, &session, registry, true, None).is_empty());
    session
        .file_scope
        .propose(
            &subject,
            0,
            DirectoryProposal {
                request_id: "root".into(),
                requested_path: "/work".into(),
                canonical_path: "/work".into(),
                purpose: "Read the chosen directory".into(),
                source: DirectoryConsentSource::OwnerSelection,
                directory: ObjectRef {
                    token: "dir-token".into(),
                    snapshot_id: "dir-snapshot".into(),
                    object_kind: ObjectKind::Directory,
                    expires_at: String::new(),
                },
            },
            1,
        )
        .unwrap();
    session
        .file_scope
        .decide(&subject, 1, "root", true, 1)
        .unwrap();
    let reads = read_tools(&devices, &session, registry, true, None);
    assert_eq!(reads.len(), 2);
    assert!(
        reads
            .iter()
            .all(|(tool, callable)| has_target_selector(&tool.spec) && !callable)
    );
    assert!(read_tools(&devices[1..], &session, registry, true, None).is_empty());
    for (target_ref, allowed) in [("a", true), ("b", false)] {
        let request = plan_device_permission(
            &NoDeviceCommands,
            Some(registry),
            &session,
            &devices,
            &reads,
            &proposal(
                "inspect_files",
                target_ref,
                json!({"directory_request_id":"root"}),
            ),
            "file-read",
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(request.is_ok(), allowed, "{request:?}");
        if let Ok(request) = request {
            request.validate().unwrap();
            assert_eq!(
                request.selected_device_target(),
                devices[0].read_target.as_ref()
            );
        }
    }
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
        if matches!(
            name,
            crate::command_confirmation::COMMAND_TOOL | "read_terminal_output"
        ) || crate::file_model_input::is_read(name)
            || selected_action_descriptor(descriptor)
        {
            continue;
        }
        let args = match name {
            "read_system_info"
            | "read_network_ports"
            | "read_recent_logs"
            | "read_container_list"
            | "inspect_desktop_session"
            | "inspect_office_selection"
            | "inspect_live_document"
            | "inspect_live_spreadsheet"
            | "inspect_live_presentation"
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
        let original = crate::input_read_context::live_read::target_kind(name).map(|(_, kind)| {
            use crate::input_read_context::{ReadContextSelection, live_read::LiveReadTarget};
            let expires_at = "2026-11-06T00:02:00Z";
            ReadContextSelection {
                tool_names: vec![name.into()],
                live_targets: vec![LiveReadTarget {
                    device_id: "a".into(),
                    tool_name: name.into(),
                    object_ref: ObjectRef {
                        token: "observed-live-resource".into(),
                        snapshot_id: "snapshot".into(),
                        object_kind: kind,
                        expires_at: expires_at.into(),
                    },
                    interactive_session_incarnation: "worker".into(),
                    readiness_expires_at_unix_ms: chrono::DateTime::parse_from_rfc3339(expires_at)
                        .unwrap()
                        .timestamp_millis()
                        as u64,
                }],
                ..ReadContextSelection::default()
            }
        });
        if original.is_some() {
            assert!(
                read_tools(&devices, &session, registry, true, None).is_empty(),
                "{name}"
            );
        }
        let reads = read_tools(&devices, &session, registry, true, original.as_ref());
        let seam = OriginalReads(original);
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
        let mut fixed_spec = descriptor.tool_spec.clone();
        crate::ui_model_ids::project_tool(&mut fixed_spec);
        let mut selected_schema = model_spec.parameters_schema["properties"].clone();
        selected_schema
            .as_object_mut()
            .unwrap()
            .remove("target_ref");
        assert_eq!(
            selected_schema,
            fixed_spec
                .parameters_schema
                .get("properties")
                .cloned()
                .unwrap_or_else(|| json!({})),
            "{name}"
        );
        assert!(
            read_tools(&devices[1..], &session, registry, true, None).is_empty(),
            "{name}"
        );
        if name == "read_current_screen" {
            assert!(read_tools(&devices, &session, registry, false, None).is_empty());
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
            &seam,
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
            &seam,
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
        assert_eq!(approved.selected_device_target(), Some(target), "{name}");
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
                &SelectedDeviceTarget {
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
                &SelectedDeviceTarget {
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
    let ui = registry.capability_for_tool("inspect_desktop_ui").unwrap();
    let mut session = session();
    let ready = vec![
        screen.wire.capability_id.clone(),
        ui.wire.capability_id.clone(),
    ];
    let devices = [device("a", ready.clone()), device("b", ready)];
    let window = |snapshot: &str| {
        json!({"token":"same-window-token","snapshot_id":snapshot,
            "object_kind":"window","expires_at":""})
    };
    for (device, snapshot) in [("a", "snapshot-a"), ("b", "snapshot-b")] {
        let mut call = tool("inspect_desktop_ui", json!({}));
        call.id = format!("observe-{device}");
        let element = format!("native-element-{device}");
        let content = json!({"ReadContext":{"DesktopUiInspect":{
            "owner_selectable_windows":[{"title":"Identical title","object_ref":window(snapshot)}],
            "nodes":[{"element_id":element,"object_ref":{"token":element,"snapshot_id":snapshot,
                "object_kind":"ui_element","expires_at":""}}]
        }}})
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
    let reads = read_tools(&devices, &session, registry, true, None);
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
    for (target, element, allowed) in [("a", "e1", true), ("b", "e1", false), ("b", "e2", true)] {
        let call = resolve(
            &tool(
                REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                json!({
                    "target_ref":target,
                    "items":[{"tool_name":"inspect_desktop_ui",
                        "exact_input":{"element_id":element,"element_only":true},
                        "reason":"Read a specific control"}]
                }),
            ),
            &session,
        );
        let permission = plan_device_permission(
            &NoDeviceCommands,
            Some(registry),
            &session,
            &devices,
            &reads,
            &call,
            "control",
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(
            permission.is_ok(),
            allowed,
            "{target}/{element}: {permission:?}"
        );
    }
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
    let reads = read_tools(&devices, &session, registry, true, None);
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

#[test]
fn sole_target_uses_its_frozen_public_reference_after_reopen() {
    use crate::dynamic_run::AssistantInputTarget;

    let mut session = session();
    let read = tool("read_system_info", json!({}));
    assert!(infer_single_target(&read, &session).unwrap().is_none());
    session.input_revision += 1;
    session
        .begin_focus_epoch(session.input_revision, Vec::new())
        .unwrap();
    let only = AssistantInputTarget {
        reference: "public-mac".into(),
        device_id: "42".into(),
        member_generation: Some(2),
    };
    session
        .bind_assistant_input_targets(std::slice::from_ref(&only))
        .unwrap();
    assert!(
        session
            .bind_assistant_input_targets(&[AssistantInputTarget {
                reference: "renamed".into(),
                ..only.clone()
            }])
            .is_err()
    );
    let restored =
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    assert_eq!(restored.assistant_request_device("public-mac"), Some(&only));
    assert!(restored.assistant_request_device("42").is_none());
    let inferred = infer_single_target(&read, &restored).unwrap().unwrap();
    assert_eq!(selected_read(&inferred).unwrap().0, "public-mac");
    let explicit = tool("read_system_info", json!({"target_ref":"unknown"}));
    assert!(infer_single_target(&explicit, &restored).unwrap().is_none());

    let ask = tool(
        REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
        json!({"items":[{"tool_name":"inspect_desktop_ui","exact_input":{}}]}),
    );
    let inferred = infer_single_target(&ask, &restored).unwrap().unwrap();
    let args: Value = serde_json::from_str(&inferred.arguments_json).unwrap();
    assert_eq!(args["target_ref"], "public-mac");
    assert!(
        args["items"][0]["exact_input"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    let web = tool(
        REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
        json!({"items":[{"tool_name":"fetch_public_web_page","exact_input":{"url":"https://example.com/"}}]}),
    );
    assert!(infer_single_target(&web, &restored).unwrap().is_none());
}

#[tokio::test]
async fn browser_open_permission_is_exact_and_bound_to_the_original_device() {
    let mut session = session();
    let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
    let name = "browser_open_page";
    let descriptor = registry.capability_for_tool(name).unwrap();
    assert!(selected_action_descriptor(descriptor));
    let mut devices = [
        device("a", vec![descriptor.wire.capability_id.clone()]),
        device("b", vec![]),
    ];
    devices[0].ready_capabilities = vec![Capability::BrowserPageObserve];
    let reads = read_tools(&devices, &session, registry, true, None);
    let [(registered, callable)] = reads.as_slice() else {
        panic!("expected the browser operation")
    };
    assert_eq!(registered.name(), name);
    assert!(!callable);
    let exact = json!({"target":{"url":"https://example.com/","origin":{"kind":"https","host_ascii":"example.com","port":443}}});
    let propose = |target| proposal(name, target, exact.clone());
    let plan = plan_device_permission(
        &NoDeviceCommands,
        Some(registry),
        &session,
        &devices,
        &reads,
        &propose("a"),
        "browser-permission",
        NOW,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(plan.selected_device_id(), Some("a"));
    assert_eq!(
        plan.items[0].canonical_input_json.as_deref(),
        Some(
            canonical_selected_read_input(name, exact.clone())
                .unwrap()
                .as_str()
        )
    );
    assert!(
        plan_device_permission(
            &NoDeviceCommands,
            Some(registry),
            &session,
            &devices,
            &reads,
            &propose("b"),
            "wrong-device",
            NOW
        )
        .await
        .unwrap()
        .is_err()
    );
    session.permission_requests.push(plan);
    let native = selected_action_device(
        &session,
        &tool(name, json!({"target_ref":"a","target":exact["target"]})),
    )
    .unwrap();
    assert_eq!(native.as_deref(), Some("a"));
    assert!(
        selected_action_device(
            &session,
            &tool(
                name,
                json!({"target_ref":"elsewhere","target":exact["target"]})
            )
        )
        .is_err()
    );
    session.agent_role = crate::subagent::AgentRole::Coordinator {
        workbench_id: "workbench".into(),
        request_id: None,
        issue_source: None,
    };
    assert!(read_tools(&devices, &session, registry, true, None).is_empty());
    assert!(
        selected_action_device(
            &session,
            &tool(name, json!({"target_ref":"a","target":exact["target"]}))
        )
        .is_err()
    );
}
