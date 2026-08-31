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
        truncated: false,
    }));
    assert!(validate_output(&registry, &ui_call, &ui_output, &limit(4096, 1)).is_err());
    validate_output(&registry, &ui_call, &ui_output, &limit(4096, 2)).unwrap();
}
