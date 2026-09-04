use super::*;
use crate::{
    device_assistant::device_assistant_provider_registry,
    input_read_context::{ReadContextSelection, live_read::LiveReadTarget},
};

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "fixture".into(),
        connection_revision: 1,
        model_id: "fixture".into(),
        profile_revision: 1,
    }
}

#[test]
fn live_read_authority_uses_original_targets_on_both_surfaces() {
    let registry = device_assistant_provider_registry();
    let now = 1_788_134_401_000;
    let destination = destination();
    for name in [
        "inspect_office_selection",
        "inspect_live_spreadsheet",
        "inspect_live_document",
        "inspect_live_presentation",
    ] {
        let reference = ObjectRef {
            token: "original-token".into(),
            snapshot_id: "snapshot".into(),
            object_kind: crate::input_read_context::live_read::target_kind(name)
                .unwrap()
                .1,
            expires_at: chrono::DateTime::from_timestamp_millis((now + 60_000) as i64)
                .unwrap()
                .to_rfc3339(),
        };
        let original = ReadContextSelection {
            tool_names: vec![name.into()],
            expires_at: None,
            object_attachments: vec![],
            live_targets: vec![LiveReadTarget {
                tool_name: name.into(),
                object_ref: reference.clone(),
                interactive_session_incarnation: "worker".into(),
                readiness_expires_at_unix_ms: now + 30_000,
            }],
        };
        let binding = ObjectReadBinding {
            original: &original,
            destination: &destination,
            now_unix_ms: now,
        };
        let call = ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments_json: "{}".into(),
        };
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let preflight = ReadCallPreflight::build(&registry, surface, &call, &binding).unwrap();
            assert_eq!(preflight.valid_until_unix_ms(), now + 30_000);
            let subject = ProviderCallSubject {
                actor_id: "7",
                run_id: "run",
                input_revision: 1,
                target_device_id: "42",
                policy_revision: 1,
                readiness_revision: 7,
                now_unix_ms: now,
            };
            let authority = preflight.grant_call(&subject).unwrap();
            assert_eq!(
                authority.byte_count, 0,
                "output size is checked after the read"
            );
            assert_eq!(authority.item_count, 1);
            assert!(preflight.output_limits().max_bytes_per_call > 2);
            assert_eq!(
                authority.resource_scope,
                fresh_object_resource_scope(std::slice::from_ref(&reference))
            );
            assert_eq!(authority.operation_scope, ["use_selected_object"]);
            assert_ne!(authority.risk_tier, CapabilityRiskTier::R0);
            assert!(!authority.resource_scope.join("").contains("original-token"));
            let mut old = original.clone();
            old.live_targets.clear();
            assert!(
                ReadCallPreflight::build(
                    &registry,
                    surface,
                    &call,
                    &ObjectReadBinding {
                        original: &old,
                        ..binding
                    }
                )
                .is_err()
            );
        }
    }
}

#[test]
fn selected_device_read_is_bounded_and_mutations_or_unselected_calls_are_refused() {
    let registry = device_assistant_provider_registry();
    let destination = destination();
    let mut original = ReadContextSelection {
        tool_names: vec!["read_system_info".into()],
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![],
    };
    let call = ToolCall {
        id: "call".into(),
        name: "read_system_info".into(),
        arguments_json: "{}".into(),
    };
    let preflight = ReadCallPreflight::build(
        &registry,
        ProductSurface::ManagerPersonalOwner,
        &call,
        &ObjectReadBinding {
            original: &original,
            destination: &destination,
            now_unix_ms: 1000,
        },
    )
    .unwrap();
    assert_eq!(preflight.valid_until_unix_ms(), 121000);
    let subject = ProviderCallSubject {
        actor_id: "7",
        run_id: "run",
        input_revision: 1,
        target_device_id: "42",
        policy_revision: 1,
        readiness_revision: 7,
        now_unix_ms: 1000,
    };
    assert_eq!(
        preflight.grant_call(&subject).unwrap().risk_tier,
        CapabilityRiskTier::R0
    );
    let mut stale = subject;
    stale.readiness_revision = 0;
    assert!(preflight.grant_call(&stale).is_err());
    for name in [
        "read_process_list",
        "execute_confirmed_ui_action",
        "fetch_public_web_page",
    ] {
        let other = ToolCall {
            name: name.into(),
            ..call.clone()
        };
        assert!(
            ReadCallPreflight::build(
                &registry,
                ProductSurface::ManagerPersonalOwner,
                &other,
                &ObjectReadBinding {
                    original: &original,
                    destination: &destination,
                    now_unix_ms: 1000
                }
            )
            .is_err()
        );
    }
    original.expires_at = Some("1970-01-01T00:00:01Z".into());
    assert!(
        ReadCallPreflight::build(
            &registry,
            ProductSurface::ManagerPersonalOwner,
            &call,
            &ObjectReadBinding {
                original: &original,
                destination: &destination,
                now_unix_ms: 1000
            }
        )
        .is_err()
    );
}

#[test]
fn central_web_authority_is_exact_and_fixes_the_search_destination() {
    let registry = device_assistant_provider_registry();
    let destination = destination();
    let original = ReadContextSelection {
        tool_names: vec!["search_public_web".into()],
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![],
    };
    let binding = ObjectReadBinding {
        original: &original,
        destination: &destination,
        now_unix_ms: 1_000,
    };
    let call = ToolCall {
        id: "search-call".into(),
        name: "search_public_web".into(),
        arguments_json: serde_json::json!({"query":"Rust language","max_results":5}).to_string(),
    };
    let preflight = ReadCallPreflight::build_central_web(
        &registry,
        ProductSurface::ManagerPersonalOwner,
        &call,
        &binding,
        "请搜索 Rust language",
    )
    .unwrap();
    let authority = preflight
        .grant_call(&ProviderCallSubject {
            actor_id: "7",
            run_id: "run",
            input_revision: 1,
            target_device_id: "42",
            policy_revision: 1,
            readiness_revision: 1,
            now_unix_ms: 1_000,
        })
        .unwrap();
    assert_eq!(authority.effect, CapabilityEffect::ExportData);
    assert_eq!(authority.item_count, 5);
    assert!(authority.resource_scope[0].starts_with("external_query_input:sha256:"));
    assert_eq!(authority.operation_scope, ["search_public_web"]);
    assert_eq!(
        authority.export_destinations,
        [DestinationIdentity::WebResearch {
            connector_id: crate::web_research::BRAVE_WEB_SEARCH_CONNECTOR_ID.into(),
        }]
    );
    assert!(
        ReadCallPreflight::build_central_web(
            &registry,
            ProductSurface::ManagerPersonalOwner,
            &call,
            &binding,
            "请搜索刚才的关键词",
        )
        .is_err()
    );
    assert!(
        ReadCallPreflight::build(
            &registry,
            ProductSurface::ManagerPersonalOwner,
            &call,
            &binding
        )
        .is_err()
    );
}

#[test]
fn file_terminal_and_iwork_batch_authority_uses_only_original_attachment_refs() {
    use crate::object_context::{
        ObjectContextBuild, ObjectContextMutation, build_object_context_mutation,
    };
    use desk_agent_protocol::device_assistant::{
        DeviceAssistantObjectContextOperation as Op, DeviceAssistantObjectContextUpdate,
    };
    let registry = device_assistant_provider_registry();
    let destination = destination();
    for name in [
        "inspect_selected_file_metadata",
        "read_selected_text_file",
        "inspect_selected_spreadsheets",
        "preview_spreadsheet_merge",
        "inspect_selected_terminal_output",
        "inspect_selected_numbers_with_iwork",
        "inspect_selected_pages_with_iwork",
        "inspect_selected_keynote_with_iwork",
    ] {
        let reference = ObjectRef {
            token: format!("original-{name}"),
            snapshot_id: "snapshot".into(),
            object_kind: if name == "inspect_selected_terminal_output" {
                ObjectKind::TerminalOutput
            } else {
                ObjectKind::File
            },
            expires_at: "2030-01-01T00:00:00Z".into(),
        };
        let operation = if name == "inspect_selected_terminal_output" {
            Op::AttachTerminalOutput {
                object_ref: reference.clone(),
                display_summary: "fixture".into(),
            }
        } else {
            Op::AttachFile {
                object_ref: reference.clone(),
                display_summary: "fixture".into(),
            }
        };
        let ObjectContextMutation::Attach(attachment) = build_object_context_mutation(
            &DeviceAssistantObjectContextUpdate {
                conversation_id: "client".into(),
                client_request_id: "selection".into(),
                operation,
            },
            ObjectContextBuild {
                actor_id: "7",
                device_id: "42",
                destination: &destination,
                now_unix_ms: 1000,
                attachment_id: "attachment",
                observation_id: "observation",
            },
        )
        .unwrap() else {
            panic!("attachment");
        };
        let original = ReadContextSelection {
            tool_names: vec![name.into()],
            expires_at: None,
            object_attachments: vec![attachment],
            live_targets: vec![],
        };
        let call = ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments_json: if name == "preview_spreadsheet_merge" {
                r#"{"columns":[{"output_header":"Region","source_headers":["Region"]}]}"#
            } else {
                "{}"
            }
            .into(),
        };
        let binding = ObjectReadBinding {
            original: &original,
            destination: &destination,
            now_unix_ms: 1000,
        };
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let preflight = ReadCallPreflight::build(&registry, surface, &call, &binding)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(
                preflight.resource_scope(),
                fresh_object_resource_scope(std::slice::from_ref(&reference))
            );
        }
        let mut missing = original.clone();
        missing.object_attachments.clear();
        assert!(
            ReadCallPreflight::build(
                &registry,
                ProductSurface::ManagerPersonalOwner,
                &call,
                &ObjectReadBinding {
                    original: &missing,
                    ..binding
                }
            )
            .is_err()
        );
    }
}
