use super::*;
use desk_agent_protocol::computer_use::*;

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "read".into(),
        name: name.into(),
        arguments_json: "{}".into(),
    }
}
fn reference() -> ObjectRef {
    ObjectRef {
        token: "original".into(),
        snapshot_id: "snapshot".into(),
        object_kind: ObjectKind::File,
        expires_at: "2026-09-01T00:00:00Z".into(),
    }
}
fn limit(bytes: u64, items: u32) -> CapabilityGrantLimits {
    CapabilityGrantLimits {
        max_bytes_per_call: bytes,
        max_items_per_call: items,
        max_calls: 1,
    }
}
fn output(value: ReadContextOutput) -> ToolRunOutput {
    ToolRunOutput {
        content: serde_json::to_string(&OperationOutput::ReadContext(value)).unwrap(),
        image_data_url: None,
    }
}

#[test]
fn read_limits_narrow_typed_bounds_without_dropping_selected_roots() {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let call = call("inspect_selected_file_metadata");
    let (_, mut input) = build_read_operation(&call).unwrap();
    let OperationInput::ReadContext(ref mut input_value) = input else {
        panic!()
    };
    let ContextKind::FileMetadataInspect(params) = &mut input_value.kind else {
        panic!()
    };
    params.roots = vec![reference(), reference()];
    assert!(bind(&registry, &call, &mut input, &limit(800, 1)).is_err());
    bind(&registry, &call, &mut input, &limit(800, 2)).unwrap();
    let OperationInput::ReadContext(input) = input else {
        panic!()
    };
    let ContextKind::FileMetadataInspect(params) = input.kind else {
        panic!()
    };
    assert_eq!(
        (params.max_bytes, params.max_entries, params.roots.len()),
        (800, 1, 2)
    );
}

#[test]
fn read_output_limits_count_wire_bytes_and_actual_projections() {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let text = output(ReadContextOutput::FileContentRead(FileContentReadOutput {
        file: reference(),
        display_name: "synthetic".into(),
        content_utf8: "中文".into(),
        byte_len: 6,
        sha256: "a".repeat(64),
    }));
    let text_call = call("read_selected_text_file");
    let bytes = text.content.len() as u64;
    validate_output(&registry, &text_call, &text, &limit(bytes, 1)).unwrap();
    assert!(validate_output(&registry, &text_call, &text, &limit(bytes - 1, 1)).is_err());
    let mut image = text.clone();
    image.image_data_url = Some("data:image/png;base64,YQ==".into());
    assert!(validate_output(&registry, &text_call, &image, &limit(1024, 1)).is_err());
    assert!(
        validate_output(
            &registry,
            &call("inspect_selected_terminal_output"),
            &text,
            &limit(1024, 1)
        )
        .is_err()
    );
    let projection = FileMetadataProjection {
        object_ref: reference(),
        display_name: "synthetic".into(),
        is_directory: false,
        byte_len: Some(1),
        modified_at: None,
    };
    let metadata = output(ReadContextOutput::FileMetadataInspect(
        FileMetadataInspectOutput {
            snapshot_id: "snapshot".into(),
            entries: vec![projection.clone(), projection],
            directory_entries: vec![],
            truncated: false,
        },
    ));
    let meta_call = call("inspect_selected_file_metadata");
    assert!(validate_output(&registry, &meta_call, &metadata, &limit(4096, 1)).is_err());
    validate_output(&registry, &meta_call, &metadata, &limit(4096, 2)).unwrap();
    let screen = ToolRunOutput {
        content: "summary".into(),
        image_data_url: Some("data:image/png;base64,YQ==".into()),
    };
    let screen_call = call("read_system_info");
    assert!(validate_output(&registry, &screen_call, &screen, &limit(7, 1)).is_err());
}

#[test]
fn desktop_ui_limits_narrow_request_and_count_returned_nodes() {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let ui_call = call("inspect_desktop_ui");
    let (_, mut input) = build_read_operation(&ui_call).unwrap();
    bind(&registry, &ui_call, &mut input, &limit(512, 1)).unwrap();
    let OperationInput::ReadContext(input) = input else {
        panic!()
    };
    let ContextKind::DesktopUiInspect(params) = input.kind else {
        panic!()
    };
    assert_eq!((params.max_bytes, params.max_nodes), (512, 1));

    let node = UiNodeProjection {
        object_ref: ObjectRef {
            object_kind: ObjectKind::UiElement,
            ..reference()
        },
        parent_index: None,
        role: "button".into(),
        name: Some("Apply".into()),
        value: None,
        is_protected: false,
        enabled: true,
        supported_actions: vec![],
    };
    let ui_output = output(ReadContextOutput::DesktopUiInspect(UiInspectOutput {
        snapshot_id: "snapshot".into(),
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::MacosAccessibility,
            version: "1".into(),
        },
        nodes: vec![node.clone(), node],
        owner_selectable_windows: vec![],
        truncated: false,
    }));
    assert!(validate_output(&registry, &ui_call, &ui_output, &limit(4096, 1)).is_err());
    validate_output(&registry, &ui_call, &ui_output, &limit(4096, 2)).unwrap();
}

#[test]
fn central_web_output_is_schema_checked_and_narrowed_by_the_grant() {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let search_call = ToolCall {
        id: "search-1".into(),
        name: crate::web_research::WEB_SEARCH_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"query":"Rust language","max_results":2}).to_string(),
    };
    let search = ToolRunOutput {
        content: serde_json::json!({
            "schema_version": 1,
            "configuration_revision": 3,
            "web_search_call_id": "search-1",
            "untrusted_external_content": true,
            "connector": {
                "connector_id": crate::web_research::BRAVE_WEB_SEARCH_CONNECTOR_ID,
                "display_name": "Brave Web Search",
                "requires_api_key": true,
                "experimental": false
            },
            "query_sha256": format!("{:x}", sha2::Sha256::digest(b"Rust language")),
            "searched_at": "2026-08-28T00:00:00Z",
            "response_sha256": "a".repeat(64),
            "response_bytes": 100,
            "result_count": 2,
            "results": [
                {"title":"Rust","url":"https://www.rust-lang.org/","snippet":"Language","published_at":null},
                {"title":"Book","url":"https://doc.rust-lang.org/book/","snippet":"Book","published_at":null}
            ]
        })
        .to_string(),
        image_data_url: None,
    };
    assert!(validate_output(&registry, &search_call, &search, &limit(4096, 1)).is_err());
    validate_output(&registry, &search_call, &search, &limit(4096, 2)).unwrap();
    for (connector, revision, valid) in [
        ("brave_web_v1", 3, true),
        ("tavily_search_v1", 3, false),
        ("brave_web_v1", 4, false),
    ] {
        let bound =
            registry
                .clone()
                .with_web_search_binding(Some(crate::web_research::SearchBinding {
                    connector_id: connector.into(),
                    revision,
                }));
        assert_eq!(
            validate_output(&bound, &search_call, &search, &limit(4096, 2)).is_ok(),
            valid
        );
    }
    let mut forged: serde_json::Value = serde_json::from_str(&search.content).unwrap();
    forged["connector"]["connector_id"] = serde_json::json!("model_selected");
    assert!(
        validate_output(
            &registry,
            &search_call,
            &ToolRunOutput {
                content: forged.to_string(),
                image_data_url: None
            },
            &limit(4096, 2)
        )
        .is_err()
    );

    let fetch_call = ToolCall {
        id: "fetch-1".into(),
        name: crate::web_research::WEB_FETCH_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"url":"https://example.com/"}).to_string(),
    };
    let fetch = ToolRunOutput {
        content: serde_json::json!({
            "schema_version": 1,
            "untrusted_external_content": true,
            "requested_url": "https://example.com/",
            "final_url": "https://example.com/page",
            "title": "Example",
            "published_at": null,
            "fetched_at": "2026-08-28T00:00:00Z",
            "content_type": "text/html",
            "body_bytes": 100,
            "sha256": "b".repeat(64),
            "excerpt": "Example body"
        })
        .to_string(),
        image_data_url: None,
    };
    validate_output(&registry, &fetch_call, &fetch, &limit(4096, 1)).unwrap();
}
