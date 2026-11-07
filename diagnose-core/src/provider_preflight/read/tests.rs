use super::*;
use crate::{
    ai_assistant::ai_assistant_provider_registry,
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
fn output_digest_accepts_equivalent_json_and_defaults_but_binds_business_content() {
    let session = crate::ai_assistant::test_support::personal_session("7", "run", "11");
    let output = crate::seam::ToolRunOutput {
        format: crate::seam::ToolOutputFormat::Text,
        content: "original result".into(),
        image_data_url: None,
        document_preview: None,
    };
    for (name, input) in [
        ("read_system_info", "{ \n }"),
        (
            "read_process_list",
            r#"{ "queries": ["worker"], "limit": 3 }"#,
        ),
        ("search_public_web", r#"{ "query": "Rust language" }"#),
    ] {
        let original = ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments_json: input.into(),
        };
        let resolved =
            crate::ui_model_ids::resolve_session_call_with_context(&original, &session, 0, None)
                .unwrap();
        assert_ne!(original.arguments_json, resolved.arguments_json);
        assert_eq!(
            output_digest(&original, &output).unwrap(),
            output_digest(&resolved, &output).unwrap()
        );
    }
    let original = ToolCall {
        id: "call".into(),
        name: "search_public_web".into(),
        arguments_json: r#"{"query":"Rust language"}"#.into(),
    };
    let expected = output_digest(&original, &output).unwrap();
    let defaults = ToolCall {
        arguments_json: r#"{ "max_results": 5, "query": "Rust language" }"#.into(),
        ..original.clone()
    };
    assert_eq!(expected, output_digest(&defaults, &output).unwrap());
    for input in [
        r#"{"query":"different"}"#,
        r#"{"query":"Rust language","max_results":2}"#,
    ] {
        let changed = ToolCall {
            arguments_json: input.into(),
            ..original.clone()
        };
        assert_ne!(expected, output_digest(&changed, &output).unwrap());
    }
    for changed in [
        crate::seam::ToolRunOutput {
            content: "different".into(),
            ..output.clone()
        },
        crate::seam::ToolRunOutput {
            image_data_url: Some("data:image/png;base64,eA==".into()),
            ..output.clone()
        },
    ] {
        assert_ne!(expected, output_digest(&original, &changed).unwrap());
    }
    let other_tool = ToolCall {
        name: "read_system_info".into(),
        ..original.clone()
    };
    assert_ne!(expected, output_digest(&other_tool, &output).unwrap());
    let invalid = ToolCall {
        arguments_json: "invalid JSON".into(),
        ..original
    };
    assert!(output_digest(&invalid, &output).is_err());
    let error_digest = error_output_digest(&invalid, &output);
    assert_eq!(error_digest, error_output_digest(&invalid, &output));
    let changed_invalid = ToolCall {
        arguments_json: "different invalid JSON".into(),
        ..invalid
    };
    assert_ne!(error_digest, error_output_digest(&changed_invalid, &output));
    assert_ne!(expected, error_output_digest(&defaults, &output));
}

#[test]
fn live_read_authority_uses_original_targets_on_both_surfaces() {
    let registry = ai_assistant_provider_registry();
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
            external_issues: Vec::new(),
            external_issue_sources: Vec::new(),
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
            device_id: "42",
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
    let registry = ai_assistant_provider_registry();
    let destination = destination();
    let mut original = ReadContextSelection {
        external_issues: Vec::new(),
        external_issue_sources: Vec::new(),
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
            device_id: "42",
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
        "execute_ui_actions",
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
                    device_id: "42",
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
                device_id: "42",
                original: &original,
                destination: &destination,
                now_unix_ms: 1000
            }
        )
        .is_err()
    );
}

#[test]
fn web_fetch_preflight_reuses_host_scope_on_both_surfaces_and_freezes_each_input() {
    let registry = ai_assistant_provider_registry();
    let destination = destination();
    let original = ReadContextSelection {
        external_issues: Vec::new(),
        external_issue_sources: Vec::new(),
        tool_names: vec![crate::web_research::WEB_FETCH_TOOL_NAME.into()],
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![],
    };
    let binding = ObjectReadBinding {
        device_id: "42",
        original: &original,
        destination: &destination,
        now_unix_ms: 1_000,
    };
    let subject = ProviderCallSubject {
        actor_id: "7",
        run_id: "run",
        input_revision: 1,
        target_device_id: "42",
        policy_revision: 1,
        readiness_revision: 1,
        now_unix_ms: 1_000,
    };
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        let mut inputs = std::collections::HashSet::new();
        for url in [
            "https://EXAMPLE.com./docs",
            "http://example.com:8080/news?q=1",
        ] {
            let call = ToolCall {
                id: "fetch".into(),
                name: crate::web_research::WEB_FETCH_TOOL_NAME.into(),
                arguments_json: serde_json::json!({"url":url}).to_string(),
            };
            let preflight = ReadCallPreflight::build_central_web(
                &registry,
                surface,
                &call,
                &binding,
                "Read the official documentation",
            )
            .unwrap();
            let authority = preflight.grant_call(&subject).unwrap();
            assert_eq!(authority.resource_scope, ["external_web_host:example.com"]);
            assert_eq!(authority.operation_scope, ["fetch_public_web"]);
            assert!(inputs.insert(authority.canonical_input_digest_sha256.to_string()));
            let unselected = ReadContextSelection {
                external_issues: Vec::new(),
                external_issue_sources: Vec::new(),
                tool_names: vec![],
                ..original.clone()
            };
            assert!(
                ReadCallPreflight::build_central_web(
                    &registry,
                    surface,
                    &call,
                    &ObjectReadBinding {
                        original: &unselected,
                        ..binding
                    },
                    ""
                )
                .is_err()
            );
            let expired = ReadContextSelection {
                external_issues: Vec::new(),
                external_issue_sources: Vec::new(),
                expires_at: Some("1970-01-01T00:00:01Z".into()),
                ..original.clone()
            };
            assert!(
                ReadCallPreflight::build_central_web(
                    &registry,
                    surface,
                    &call,
                    &ObjectReadBinding {
                        original: &expired,
                        ..binding
                    },
                    ""
                )
                .is_err()
            );
        }
    }
}

#[test]
fn central_web_authority_is_exact_and_fixes_the_search_destination() {
    let registry = ai_assistant_provider_registry().with_web_search_binding(Some(
        crate::web_research::SearchBinding {
            connector_id: "brave_web_v1".into(),
            revision: 3,
        },
    ));
    let destination = destination();
    let original = ReadContextSelection {
        external_issues: Vec::new(),
        external_issue_sources: Vec::new(),
        tool_names: vec!["search_public_web".into()],
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![],
    };
    let binding = ObjectReadBinding {
        device_id: "42",
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
fn terminal_authority_uses_only_original_attachment_refs() {
    use crate::object_context::{
        ObjectContextBuild, ObjectContextMutation, build_object_context_mutation,
    };
    use desk_agent_protocol::ai_assistant::{
        AiAssistantObjectContextOperation as Op, AiAssistantObjectContextUpdate,
    };
    let registry = ai_assistant_provider_registry();
    let destination = destination();
    for name in ["read_terminal_output"] {
        let reference = ObjectRef {
            token: format!("original-{name}"),
            snapshot_id: "snapshot".into(),
            object_kind: ObjectKind::TerminalOutput,
            expires_at: "2030-01-01T00:00:00Z".into(),
        };
        let operation = Op::AttachTerminalOutput {
            object_ref: reference.clone(),
            display_summary: "fixture".into(),
        };
        let ObjectContextMutation::Attach(attachment) = build_object_context_mutation(
            &AiAssistantObjectContextUpdate {
                conversation_target:
                    desk_agent_protocol::ai_assistant::AiAssistantConversationTarget::FixedDevice,
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
            external_issues: Vec::new(),
            external_issue_sources: Vec::new(),
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
            device_id: "42",
            original: &original,
            destination: &destination,
            now_unix_ms: 1000,
        };
        if name == "inspect_powerpoint_file" {
            let (_, mut input) = crate::read_tools::build_read_operation(&call).unwrap();
            binding.bind(&call, &mut input).unwrap();
            let desk_agent_protocol::OperationInput::ReadContext(
                desk_agent_protocol::ReadContextInput {
                    kind: desk_agent_protocol::ContextKind::PresentationLiveInspect(params),
                },
            ) = input
            else {
                panic!("expected file-bound presentation read")
            };
            assert_eq!(params.batch_file.as_ref(), Some(&reference));
            assert!(params.target.is_none());
        }
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let preflight = ReadCallPreflight::build(&registry, surface, &call, &binding)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let selected = ReadCallPreflight::build_selected_device(
                &registry, surface, &call, &binding,
            ).unwrap();
            assert_eq!(selected.resource_scope(), preflight.resource_scope());
            assert!(ReadCallPreflight::build_selected_device(
                &registry, surface, &call, &ObjectReadBinding { device_id: "other", ..binding },
            ).is_err());
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

#[test]
fn unselected_desktop_preflight_requires_explicit_r1_authority_on_both_surfaces() {
    let registry = ai_assistant_provider_registry();
    let destination = destination();
    let original = ReadContextSelection {
        external_issues: Vec::new(),
        external_issue_sources: Vec::new(),
        tool_names: vec![],
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![],
    };
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        for name in [
            "inspect_desktop_session",
            "inspect_desktop_ui",
            "read_current_screen",
        ] {
            let call = ToolCall {
                id: "read".into(),
                name: name.into(),
                arguments_json: if name == "inspect_desktop_ui" {
                    r#"{"allow_unfiltered":true}"#.into()
                } else {
                    "{}".into()
                },
            };
            let preflight = ReadCallPreflight::build(
                &registry,
                surface,
                &call,
                &ObjectReadBinding {
                    device_id: "42",
                    original: &original,
                    destination: &destination,
                    now_unix_ms: 1000,
                },
            )
            .unwrap();
            let subject = ProviderCallSubject {
                actor_id: "owner",
                run_id: "run",
                input_revision: 1,
                target_device_id: "device",
                policy_revision: 1,
                readiness_revision: 1,
                now_unix_ms: 1000,
            };
            assert_eq!(
                preflight.grant_call(&subject).unwrap().risk_tier,
                CapabilityRiskTier::R1
            );
        }
        for name in [
            "inspect_office_selection",
            "inspect_files",
            "read_terminal_output",
        ] {
            let call = ToolCall {
                id: "read".into(),
                name: name.into(),
                arguments_json: "{}".into(),
            };
            assert!(
                ReadCallPreflight::build(
                    &registry,
                    surface,
                    &call,
                    &ObjectReadBinding {
                        device_id: "42",
                        original: &original,
                        destination: &destination,
                        now_unix_ms: 1000
                    }
                )
                .is_err()
            );
        }
    }
}
