use super::*;
use crate::{
    file_scope::{DirectoryConsentSource, DirectoryProposal},
    input_read_context::ReadContextSelection,
    session::AgentSessionSurface,
};
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    capability_provider::ProductSurface,
    computer_use::{DirectoryEntryProjection, FileMetadataProjection},
    data_lineage::DestinationIdentity,
};

const NOW: u64 = 1000;

pub(crate) fn approve_directory(session: &mut PersistedAgentSession, id: &str) -> ObjectRef {
    let root = ObjectRef {
        token: format!("root-{id}"),
        snapshot_id: format!("snapshot-{id}"),
        object_kind: ObjectKind::Directory,
        expires_at: String::new(),
    };
    let subject = session
        .file_scope_subject(
            &session.actor_id,
            &session.device_id,
            &session.conversation_id,
        )
        .unwrap();
    let revision = session.file_scope.revision();
    session
        .file_scope
        .propose(
            &subject,
            revision,
            DirectoryProposal {
                request_id: id.into(),
                requested_path: format!("/private/tmp/{id}"),
                canonical_path: format!("/private/tmp/{id}"),
                purpose: "Requested files".into(),
                source: DirectoryConsentSource::ModelProposal,
                directory: root.clone(),
            },
            1,
        )
        .unwrap();
    session
        .file_scope
        .decide(&subject, revision + 1, id, true, 1)
        .unwrap();
    root
}

fn session() -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "conversation",
        "owner",
        "device",
        1,
        AgentScope {
            granted: vec![],
            expires_at: None,
            mode: ExecutionMode::ReadOnly,
            policy_name: None,
        },
        "2026-10-15T00:00:00Z",
    );
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::AiAssistant);
    session.input_revision = 1;
    session.focus_epoch.input_revision = 1;
    session.conversation.push(
        crate::model_message_labels::model_bound_user_message(
            "owner".into(),
            "Read or create the requested file".into(),
            DestinationIdentity::Model {
                connection_id: "test".into(),
                connection_revision: 1,
                model_id: "test".into(),
                profile_revision: 1,
            },
        )
        .unwrap(),
    );
    approve_directory(&mut session, "directory");
    session
}

fn call(name: &str, value: Value) -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: name.into(),
        arguments_json: value.to_string(),
    }
}

fn metadata_session(files: &[&str], truncated: bool) -> PersistedAgentSession {
    let mut session = session();
    let root = session.file_scope.records()[0].proposal.directory.clone();
    let output = FileMetadataInspectOutput {
        snapshot_id: "listing".into(),
        entries: vec![FileMetadataProjection {
            object_ref: root.clone(),
            display_name: "directory".into(),
            is_directory: true,
            byte_len: None,
            modified_at: None,
        }],
        directory_entries: files
            .iter()
            .map(|name| DirectoryEntryProjection {
                object_ref: Some(ObjectRef {
                    token: format!("file-{name}"),
                    snapshot_id: "file-version".into(),
                    object_kind: ObjectKind::File,
                    expires_at: "2099-01-01T00:00:00Z".into(),
                }),
                parent_snapshot_id: root.snapshot_id.clone(),
                display_name: (*name).into(),
                is_directory: false,
                byte_len: Some(3),
                modified_at: None,
            })
            .collect(),
        truncated,
        pagination_id: None,
        pagination_consistency: None,
        next_cursor: None,
    };
    // The original model omitted the directory; the authenticated output retains it.
    session.conversation = crate::result_model_ids::tests::evidence(
        "inspect_files",
        "metadata-call",
        json!({}),
        json!(OperationOutput::ReadContext(
            ReadContextOutput::FileMetadataInspect(output)
        )),
    );
    session
}

fn read_context(
    session: &mut PersistedAgentSession,
    tools: &[&str],
    files: usize,
) -> ReadContextSelection {
    use crate::context_attachment::*;
    for index in 0..files {
        let reference = ObjectRef {
            token: format!("file-{index}"),
            snapshot_id: "selected".into(),
            object_kind: ObjectKind::File,
            expires_at: "1970-01-01T00:01:00Z".into(),
        };
        let attachment = ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: format!("attachment-{index}"),
            client_request_id: format!("select-{index}"),
            actor_id: session.actor_id.clone(),
            device_id: session.device_id.clone(),
            surface: session.surface,
            kind: ContextAttachmentKind::File,
            object_ref: AttachmentObjectRef {
                opaque_token: json!(reference).to_string(),
                object_incarnation: format!("{}:{}", reference.snapshot_id, reference.token),
                source_provider_id: "file.workspace".into(),
                source_capability_id: "file.read".into(),
            },
            bounds: AttachmentBounds {
                max_bytes: 32768,
                max_objects: 8,
            },
            display_summary: format!("file-{index}.txt"),
            created_at_unix_ms: 1,
            expires_at_unix_ms: 60000,
            envelope: session.conversation[0].data_envelope.clone().unwrap(),
            state: AttachmentState::Active,
        };
        session.context_attachments.push(attachment);
    }
    let mut tool_names: Vec<_> = tools.iter().map(|name| (*name).to_string()).collect();
    tool_names.sort();
    ReadContextSelection {
        tool_names,
        expires_at: None,
        object_attachments: session.context_attachments.clone(),
        live_targets: vec![],
    }
}

#[test]
fn source_types_are_restored_from_verified_results_and_hidden_in_all_views() {
    let session = metadata_session(&["notes.txt"], false);
    for tool in [
        "inspect_word_file",
        "inspect_spreadsheets",
        "preview_document",
        "convert_document",
    ] {
        let mut args = match tool {
            "convert_document" => {
                json!({"output_name":"notes.pdf","conversion":{"kind":"text_to_pdf"}})
            }
            _ => json!({}),
        };
        if crate::provider_preflight::text_file::selection::multiple(tool) {
            args["sources"] = json!([{"file_result_call_id":"receipt"}]);
        } else {
            args["source"] = json!({"file_result_call_id":"receipt"});
        }
        let original = call(tool, args.clone());
        let resolved = crate::ui_model_ids::resolve_session_call(&original, &session, NOW).unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        let source = if args.get("sources").is_some() {
            &value["sources"][0]
        } else {
            &value["source"]
        };
        assert_eq!(source["kind"], "directory_entry");
        assert_eq!(source["entry_name"], "notes.txt");
        assert_eq!(source["file_result_call_id"], "metadata-call");
        assert!(
            crate::ui_model_ids::same_call_input_with_references(
                tool,
                &original.arguments_json,
                &resolved.arguments_json,
                None,
                &crate::result_model_ids::locators(&session.conversation)
            ),
            "{tool}: original={} resolved={}",
            original.arguments_json,
            resolved.arguments_json
        );
        let mut projected = value.clone();
        crate::result_model_ids::project_arguments(
            tool,
            &mut projected,
            &crate::result_model_ids::locators(&session.conversation),
        );
        assert!(!projected.to_string().contains("directory_entry"));
        assert!(!projected.to_string().contains("artifact_result"));
        let field = if args.get("sources").is_some() {
            &mut args["sources"][0]
        } else {
            &mut args["source"]
        };
        field["kind"] = json!("directory_entry");
        assert!(
            crate::ui_model_ids::resolve_call(&call(tool, args), &session.conversation, NOW)
                .is_err()
        );
    }
}

#[test]
fn omitted_child_never_guesses_in_incomplete_or_ambiguous_directory_results() {
    for (files, truncated) in [
        (vec!["a.txt", "b.txt"], false),
        (vec!["a.txt"], true),
        (vec![], false),
    ] {
        let session = metadata_session(&files, truncated);
        let error = crate::ui_model_ids::resolve_session_call(
            &call("read_text_file", json!({"file_result_call_id":"receipt"})),
            &session,
            NOW,
        )
        .unwrap_err();
        assert!(error.message.contains("entry_name is required"));
    }
    let session = metadata_session(&["a.txt", "b.txt"], false);
    assert!(
        crate::ui_model_ids::resolve_session_call(
            &call(
                "read_text_file",
                json!({"file_result_call_id":"receipt","entry_name":"b.txt"})
            ),
            &session,
            NOW
        )
        .is_ok()
    );
    assert!(
        crate::ui_model_ids::resolve_session_call(
            &call("read_text_file", json!({"file_result_call_id":"unknown"})),
            &session,
            NOW
        )
        .is_err()
    );
}

#[test]
fn directory_and_created_content_freeze_before_human_or_ai_review() {
    let mut session = session();
    let original = call(
        "create_text_file",
        json!({"file_name":"notes.txt","content_utf8":""}),
    );
    let frozen = crate::ui_model_ids::resolve_session_call(&original, &session, NOW).unwrap();
    let value: Value = serde_json::from_str(&frozen.arguments_json).unwrap();
    assert_eq!(value["directory_request_id"], "directory");
    assert_eq!(value["content_utf8"], "");
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        let before = crate::provider_preflight::ArtifactCallPreflight::from_session(
            &registry, surface, &session, &frozen, NOW,
        )
        .unwrap();
        approve_directory(
            &mut session,
            if surface == ProductSurface::OssPersonalOwner {
                "second"
            } else {
                "third"
            },
        );
        let after = crate::provider_preflight::ArtifactCallPreflight::from_session(
            &registry, surface, &session, &frozen, NOW,
        )
        .unwrap();
        assert_eq!(before.canonical_input_json(), after.canonical_input_json());
        assert_eq!(before.target(), after.target());
        assert!(crate::ui_model_ids::resolve_session_call(&original, &session, NOW).is_err());
    }
    let mut changed = value;
    changed["content_utf8"] = json!("different");
    assert!(!crate::ui_model_ids::same_call_input(
        "create_text_file",
        &original.arguments_json,
        &changed.to_string()
    ));
    let subject = session
        .file_scope_subject("owner", "device", "conversation")
        .unwrap();
    session
        .file_scope
        .revoke(&subject, session.file_scope.revision(), "directory")
        .unwrap();
    assert!(
        crate::provider_preflight::ArtifactCallPreflight::from_session(
            &registry,
            ProductSurface::OssPersonalOwner,
            &session,
            &frozen,
            NOW
        )
        .is_err()
    );
}

#[test]
fn missing_creation_business_fields_are_explained_before_permission_creation() {
    let session = session();
    let nested = call(
        "request_permissions",
        json!({"items":[{"tool_name":"request_permissions","reason":"invalid nested request","exact_input":{"items":[]}}]}),
    );
    assert!(crate::ui_model_ids::resolve_session_call(&nested, &session, NOW).is_err());
    for input in [
        json!({"file_name":"notes.txt"}),
        json!({"content_utf8":"text"}),
    ] {
        let error = crate::ui_model_ids::resolve_session_call(&call("request_permissions", json!({"items":[{"tool_name":"create_text_file","reason":"create","exact_input":input}]})), &session, NOW).unwrap_err();
        assert!(error.message.contains(if input.get("file_name").is_some() {
            "content_utf8"
        } else {
            "file_name"
        }));
    }
}

#[test]
fn metadata_root_is_recovered_from_original_result_after_more_directories_are_approved() {
    let mut session = metadata_session(&["notes.txt"], false);
    let original = call("read_text_file", json!({"file_result_call_id":"receipt"}));
    let frozen = crate::ui_model_ids::resolve_session_call(&original, &session, NOW).unwrap();
    approve_directory(&mut session, "second");
    let conversion = call(
        "convert_document",
        json!({"source":{"file_result_call_id":"receipt"},"output_name":"notes.pdf","conversion":{"kind":"text_to_pdf"}}),
    );
    let converted = crate::ui_model_ids::resolve_session_call(&conversion, &session, NOW).unwrap();
    let converted: Value = serde_json::from_str(&converted.arguments_json).unwrap();
    assert_eq!(converted["directory_request_id"], "directory");
    let [destination] = session.conversation[0]
        .data_envelope
        .as_ref()
        .unwrap()
        .allowed_destinations
        .as_slice()
    else {
        panic!();
    };
    let read = crate::provider_preflight::text_file::ResultFileRead::build(
        &session,
        &frozen,
        destination,
        NOW,
    )
    .unwrap();
    assert_eq!(read.reference().token, "file-notes.txt");
    let subject = session
        .file_scope_subject("owner", "device", "conversation")
        .unwrap();
    session
        .file_scope
        .revoke(&subject, session.file_scope.revision(), "directory")
        .unwrap();
    assert!(
        crate::provider_preflight::text_file::ResultFileRead::build(
            &session,
            &frozen,
            destination,
            NOW
        )
        .is_err()
    );
    assert!(crate::ui_model_ids::resolve_session_call(&conversion, &session, NOW).is_err());
}

#[test]
fn available_attachments_never_replace_the_immutable_user_selection() {
    let mut session = session();
    let original = read_context(&mut session, &["read_text_file"], 1);
    let validate = |session: &PersistedAgentSession, context| {
        crate::provider_preflight::text_file::validate_read_permission_input_with_context(
            session,
            "read_text_file",
            Some("{}"),
            NOW,
            context,
        )
    };
    assert!(validate(&session, Some(&original)).is_ok());
    assert!(validate(&session, None).is_err());
    let mut empty = original.clone();
    empty.object_attachments.clear();
    assert!(validate(&session, Some(&empty)).is_err());
    let explicit = call("read_text_file", json!({"file_result_call_id":"unknown"}));
    assert!(
        crate::ui_model_ids::resolve_session_call_with_context(
            &explicit,
            &session,
            NOW,
            Some(&original)
        )
        .is_err()
    );
    session.context_attachments.clear();
    assert!(validate(&session, Some(&original)).is_err());
}

#[test]
fn contextual_file_permissions_preserve_business_fields_and_exact_source_cardinality() {
    let mut session = session();
    let tools = [
        "inspect_excel_cell",
        "inspect_files",
        "inspect_keynote_file",
        "inspect_numbers_file",
        "inspect_pages_file",
        "inspect_powerpoint_file",
        "inspect_spreadsheets",
        "inspect_word_file",
        "preview_document",
        "preview_spreadsheet_merge",
        "read_text_file",
    ];
    let original = read_context(&mut session, &tools, 1);
    for tool in tools {
        let args = match tool {
            "inspect_excel_cell" => json!({"sheet_name":"Sheet1","address":"A1"}),
            "preview_spreadsheet_merge" => {
                json!({"columns":[{"output_header":"name","source_headers":["name"]}]})
            }
            _ => json!({}),
        };
        let request = call(
            "request_permissions",
            json!({"items":[{"tool_name":tool,"reason":"read","exact_input":args}]}),
        );
        let resolved = crate::ui_model_ids::resolve_session_call_with_context(
            &request,
            &session,
            NOW,
            Some(&original),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        let exact = value["items"][0]["exact_input"].to_string();
        crate::provider_preflight::text_file::validate_read_permission_input_with_context(
            &session,
            tool,
            Some(&exact),
            NOW,
            Some(&original),
        )
        .unwrap();
        assert_eq!(
            crate::permission_tools::permission_input_mode(
                crate::ai_assistant::ai_assistant_provider_registry()
                    .capability_for_tool(tool)
                    .unwrap()
            ),
            "contextual_exact_input"
        );
    }
    for tool in ["inspect_excel_cell", "preview_spreadsheet_merge"] {
        assert!(
            crate::ui_model_ids::resolve_session_call_with_context(
                &call(
                    "request_permissions",
                    json!({"items":[{"tool_name":tool,"reason":"read"}]})
                ),
                &session,
                NOW,
                Some(&original)
            )
            .is_err()
        );
    }
    let permission = call(
        "request_permissions",
        json!({"items":[{"tool_name":"read_text_file","reason":"read"}]}),
    );
    let resolved = crate::ui_model_ids::resolve_session_call_with_context(
        &permission,
        &session,
        NOW,
        Some(&original),
    )
    .unwrap();
    assert!(crate::ui_model_ids::same_call_input(
        "request_permissions",
        &permission.arguments_json,
        &resolved.arguments_json
    ));
    let mut multiple_session = self::session();
    let multiple = read_context(&mut multiple_session, &tools, 2);
    multiple.validate().unwrap();
    assert!(
        crate::provider_preflight::text_file::validate_read_permission_input_with_context(
            &multiple_session,
            "read_text_file",
            Some("{}"),
            NOW,
            Some(&multiple)
        )
        .is_err()
    );
    assert!(
        crate::provider_preflight::text_file::validate_read_permission_input_with_context(
            &multiple_session,
            "inspect_spreadsheets",
            Some("{}"),
            NOW,
            Some(&multiple)
        )
        .is_ok()
    );
    let mut mixed = multiple;
    mixed.object_attachments[1].kind =
        crate::context_attachment::ContextAttachmentKind::DirectorySelection;
    let destination = &multiple_session.conversation[0]
        .data_envelope
        .as_ref()
        .unwrap()
        .allowed_destinations[0];
    let binding = crate::input_read_context::object_read::ObjectReadBinding {
        original: &mixed,
        destination,
        now_unix_ms: NOW,
    };
    assert!(
        binding
            .selected(&call("inspect_spreadsheets", json!({})))
            .is_err()
    );
}

#[test]
fn model_schemas_and_prompt_do_not_expose_source_type_metadata() {
    let prompt = crate::ai_assistant::build_ai_assistant_system_message(Some("zh-CN"));
    assert!(!prompt.text.contains("kind=directory_entry"));
    assert!(!prompt.text.contains("kind=artifact_result"));
    for mut tool in crate::ai_assistant::ai_assistant_provider_registry().registered_tools() {
        crate::ui_model_ids::project_tool(&mut tool.spec);
        if crate::provider_preflight::text_file::selection::supports(&tool.spec.name) {
            let projected = tool.spec.clone();
            crate::ui_model_ids::project_tool(&mut tool.spec);
            assert_eq!(
                tool.spec, projected,
                "{} projection must be stable",
                tool.spec.name
            );
            let schema = &tool.spec.parameters_schema;
            let selector =
                if crate::provider_preflight::text_file::selection::multiple(&tool.spec.name) {
                    &schema["properties"]["sources"]["items"]
                } else {
                    &schema["properties"]["source"]
                };
            assert!(selector["properties"].get("kind").is_none());
            assert_eq!(selector["required"], json!(["file_result_call_id"]));
            assert!(!tool.spec.description.contains("kind="));
        }
    }
}
