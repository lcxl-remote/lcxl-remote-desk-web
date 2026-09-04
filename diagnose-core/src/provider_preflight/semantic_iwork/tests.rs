use super::*;
use crate::{
    context_attachment::{
        AttachmentBounds, AttachmentObjectRef, AttachmentState, CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        ContextAttachment, ContextAttachmentKind,
    },
    device_assistant::device_assistant_provider_registry,
    input_read_context::live_read::LiveReadTarget,
    session::AgentSessionSurface,
};
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};

const NOW: u64 = 1_788_134_401_000;

fn reference(kind: ObjectKind) -> ObjectRef {
    ObjectRef {
        token: "opaque-original".into(),
        snapshot_id: "snapshot-original".into(),
        object_kind: kind,
        expires_at: "2026-08-31T00:02:00Z".into(),
    }
}

fn original(read_tool: &str, object_ref: ObjectRef) -> ReadContextSelection {
    ReadContextSelection {
        tool_names: vec![read_tool.into()],
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![LiveReadTarget {
            tool_name: read_tool.into(),
            object_ref,
            interactive_session_incarnation: "501:worker-1:7".into(),
            readiness_expires_at_unix_ms: 1_788_134_460_000,
        }],
    }
}

fn derived_reference(kind: ObjectKind) -> ObjectRef {
    ObjectRef {
        token: "opaque-derived".into(),
        snapshot_id: "worker-1:7:272".into(),
        object_kind: kind,
        expires_at: "2026-08-31T00:05:00Z".into(),
    }
}

fn attachment(id: &str, kind: ContextAttachmentKind, reference: &ObjectRef) -> ContextAttachment {
    let expires_at_unix_ms = chrono::DateTime::parse_from_rfc3339(&reference.expires_at)
        .unwrap()
        .timestamp_millis() as u64;
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    ContextAttachment {
        schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        attachment_id: id.into(),
        client_request_id: format!("request-{id}"),
        actor_id: "actor".into(),
        device_id: "device".into(),
        surface: AgentSessionSurface::DeviceAssistant,
        kind,
        object_ref: AttachmentObjectRef {
            opaque_token: serde_json::to_string(reference).unwrap(),
            object_incarnation: format!("{}:{}", reference.snapshot_id, reference.token),
            source_provider_id: "file.workspace".into(),
            source_capability_id: "file.workspace.inspect".into(),
        },
        bounds: AttachmentBounds {
            max_bytes: 1024,
            max_objects: 1,
        },
        display_summary: id.into(),
        created_at_unix_ms: NOW - 1_000,
        expires_at_unix_ms,
        envelope: DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{id}"),
            content: ContentRef::EphemeralObservation {
                observation_id: format!("observation-{id}"),
                size_bytes: 1,
                expires_at_unix_ms,
            },
            provenance: DataProvenance {
                source_provider_id: "file.workspace".into(),
                source_tool_name: "inspect_selected_file_metadata".into(),
                source_object_id: Some(id.into()),
                source_envelope_ids: vec![],
            },
            digest_sha256: "a".repeat(64),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![destination],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(expires_at_unix_ms),
                delete_with_run: false,
            },
        },
        state: AttachmentState::Active,
    }
}

#[test]
fn live_iwork_calls_bind_exact_original_objects_and_authority() {
    let cases = [
        (
            "patch_live_spreadsheet_cell",
            "inspect_live_spreadsheet",
            ObjectKind::Range,
            serde_json::json!({"kind":"set_cell_value","params":{"value":"42"}}),
            Capability::SpreadsheetLivePatchConfirmed,
            ComputerUseAdapterKind::IworkNumbers,
        ),
        (
            "replace_live_document_body",
            "inspect_live_document",
            ObjectKind::Document,
            serde_json::json!("replacement"),
            Capability::DocumentLivePatchConfirmed,
            ComputerUseAdapterKind::IworkPages,
        ),
        (
            "patch_live_presentation_slide",
            "inspect_live_presentation",
            ObjectKind::Slide,
            serde_json::json!({"kind":"replace_slide_title","params":{"text":"Title"}}),
            Capability::PresentationLivePatchConfirmed,
            ComputerUseAdapterKind::IworkKeynote,
        ),
    ];
    for (tool, read_tool, kind, value, capability, adapter) in cases {
        let target = reference(kind);
        let arguments = if tool == "replace_live_document_body" {
            serde_json::json!({"target":target,"text":value})
        } else {
            serde_json::json!({"target":target,"action":value})
        };
        let call = ToolCall {
            id: format!("call-{tool}"),
            name: tool.into(),
            arguments_json: arguments.to_string(),
        };
        let preflight = IworkCallPreflight::build(
            &device_assistant_provider_registry(),
            ProductSurface::ManagerPersonalOwner,
            &call,
            &original(read_tool, target.clone()),
            NOW,
        )
        .unwrap();
        assert_eq!(preflight.target(), &target);
        assert_eq!(preflight.required_capability(), capability);
        assert_eq!(preflight.adapter_kind(), adapter);
        assert_eq!(preflight.action().required_capability(), capability);
        assert!(
            preflight
                .grant_call(&ProviderCallSubject {
                    actor_id: "actor",
                    run_id: "run",
                    input_revision: 1,
                    target_device_id: "device",
                    policy_revision: crate::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                    readiness_revision: 1,
                    now_unix_ms: NOW,
                })
                .is_ok()
        );
    }
}

#[test]
fn changed_or_missing_original_live_target_fails_closed() {
    let target = reference(ObjectKind::Document);
    let call = ToolCall {
        id: "call-pages".into(),
        name: "replace_live_document_body".into(),
        arguments_json: serde_json::json!({"target":target,"text":"replacement"}).to_string(),
    };
    let mut changed = target.clone();
    changed.token = "other".into();
    assert!(
        IworkCallPreflight::build(
            &device_assistant_provider_registry(),
            ProductSurface::ManagerPersonalOwner,
            &call,
            &original("inspect_live_document", changed),
            NOW,
        )
        .is_err()
    );
    assert!(
        IworkCallPreflight::build(
            &device_assistant_provider_registry(),
            ProductSurface::ManagerPersonalOwner,
            &call,
            &ReadContextSelection {
                tool_names: vec!["inspect_live_document".into()],
                expires_at: None,
                object_attachments: vec![],
                live_targets: vec![],
            },
            NOW,
        )
        .is_err()
    );
}

#[test]
fn live_iwork_calls_accept_fresh_read_target_from_same_worker_incarnation() {
    let frozen = reference(ObjectKind::Slide);
    let derived = derived_reference(ObjectKind::Slide);
    let call = ToolCall {
        id: "call-keynote-derived".into(),
        name: "patch_live_presentation_slide".into(),
        arguments_json: serde_json::json!({
            "target": derived,
            "action": {"kind":"replace_slide_title","params":{"text":"E2E"}}
        })
        .to_string(),
    };
    let preflight = IworkCallPreflight::build(
        &device_assistant_provider_registry(),
        ProductSurface::ManagerPersonalOwner,
        &call,
        &original("inspect_live_presentation", frozen),
        NOW,
    )
    .unwrap();
    assert_eq!(preflight.target(), &derived);
    assert_eq!(preflight.valid_until_unix_ms(), 1_788_134_700_000);
    assert_eq!(
        preflight.resource_scope(),
        fresh_object_resource_scope(&[derived])
    );
}

#[test]
fn fresh_read_target_outlives_original_short_readiness_lease() {
    let frozen = reference(ObjectKind::Slide);
    let derived = derived_reference(ObjectKind::Slide);
    let mut selection = original("inspect_live_presentation", frozen);
    selection.live_targets[0].readiness_expires_at_unix_ms = NOW - 1;
    let call = ToolCall {
        id: "call-keynote-after-approval-delay".into(),
        name: "patch_live_presentation_slide".into(),
        arguments_json: serde_json::json!({
            "target": derived,
            "action": {"kind":"replace_slide_title","params":{"text":"E2E"}}
        })
        .to_string(),
    };

    let preflight = IworkCallPreflight::build(
        &device_assistant_provider_registry(),
        ProductSurface::ManagerPersonalOwner,
        &call,
        &selection,
        NOW,
    )
    .unwrap();

    assert_eq!(preflight.target(), &derived);
    assert_eq!(preflight.valid_until_unix_ms(), 1_788_134_700_000);
}

#[test]
fn original_live_target_still_expires_with_original_readiness_lease() {
    let frozen = reference(ObjectKind::Slide);
    let mut selection = original("inspect_live_presentation", frozen.clone());
    selection.live_targets[0].readiness_expires_at_unix_ms = NOW - 1;
    let call = ToolCall {
        id: "call-keynote-stale-original".into(),
        name: "patch_live_presentation_slide".into(),
        arguments_json: serde_json::json!({
            "target": frozen,
            "action": {"kind":"replace_slide_title","params":{"text":"E2E"}}
        })
        .to_string(),
    };

    assert!(
        IworkCallPreflight::build(
            &device_assistant_provider_registry(),
            ProductSurface::ManagerPersonalOwner,
            &call,
            &selection,
            NOW,
        )
        .is_err()
    );
}

#[test]
fn live_iwork_calls_reject_untrusted_derived_targets() {
    let frozen = reference(ObjectKind::Slide);
    let base = derived_reference(ObjectKind::Slide);
    for changed in 0..6 {
        let mut target = base.clone();
        match changed {
            0 => target.snapshot_id = "other-worker:7:272".into(),
            1 => target.snapshot_id = "worker-1:7:not-a-sequence".into(),
            2 => target.object_kind = ObjectKind::Document,
            3 => target.token.clear(),
            4 => target.token = "x".repeat(4097),
            5 => target.expires_at = "2026-08-31T00:05:02Z".into(),
            _ => unreachable!(),
        }
        let call = ToolCall {
            id: format!("call-keynote-invalid-{changed}"),
            name: "patch_live_presentation_slide".into(),
            arguments_json: serde_json::json!({
                "target": target,
                "action": {"kind":"replace_slide_title","params":{"text":"E2E"}}
            })
            .to_string(),
        };
        assert!(
            IworkCallPreflight::build(
                &device_assistant_provider_registry(),
                ProductSurface::ManagerPersonalOwner,
                &call,
                &original("inspect_live_presentation", frozen.clone()),
                NOW,
            )
            .is_err(),
            "invalid derived target case {changed} was accepted"
        );
    }
}

#[test]
fn batch_iwork_calls_require_the_exact_selected_file_and_directory() {
    let mut file = reference(ObjectKind::File);
    file.token = "selected-file".into();
    let mut directory = reference(ObjectKind::Directory);
    directory.token = "selected-directory".into();
    let cases = [
        (
            "patch_selected_numbers_copy",
            "inspect_selected_numbers_with_iwork",
            "copy.numbers",
            serde_json::json!({"action":{"kind":"set_cell_value","params":{"value":"42"}}}),
            Capability::SpreadsheetLivePatchConfirmed,
        ),
        (
            "replace_selected_pages_copy_body",
            "inspect_selected_pages_with_iwork",
            "copy.pages",
            serde_json::json!({"text":"replacement"}),
            Capability::DocumentLivePatchConfirmed,
        ),
        (
            "patch_selected_keynote_copy",
            "inspect_selected_keynote_with_iwork",
            "copy.key",
            serde_json::json!({"action":{"kind":"replace_slide_title","params":{"text":"Title"}}}),
            Capability::PresentationLivePatchConfirmed,
        ),
    ];
    for (tool, read_tool, file_name, extra, capability) in cases {
        let mut arguments = serde_json::json!({
            "target":file,
            "output":{"destination_parent":directory,"native_file_name":file_name}
        });
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let call = ToolCall {
            id: format!("call-{tool}"),
            name: tool.into(),
            arguments_json: arguments.to_string(),
        };
        let original = ReadContextSelection {
            tool_names: vec![read_tool.into()],
            expires_at: None,
            object_attachments: vec![
                attachment(
                    "directory",
                    ContextAttachmentKind::DirectorySelection,
                    &directory,
                ),
                attachment("file", ContextAttachmentKind::File, &file),
            ],
            live_targets: vec![],
        };
        let preflight = IworkCallPreflight::build(
            &device_assistant_provider_registry(),
            ProductSurface::ManagerPersonalOwner,
            &call,
            &original,
            NOW,
        )
        .unwrap();
        assert_eq!(preflight.required_capability(), capability);
        assert_eq!(preflight.action().required_capability(), capability);
        assert_eq!(preflight.resource_scope().len(), 2);

        let mut changed = call.clone();
        let mut changed_json: serde_json::Value =
            serde_json::from_str(&changed.arguments_json).unwrap();
        changed_json["target"]["token"] = "not-selected".into();
        changed.arguments_json = changed_json.to_string();
        assert!(
            IworkCallPreflight::build(
                &device_assistant_provider_registry(),
                ProductSurface::ManagerPersonalOwner,
                &changed,
                &original,
                NOW,
            )
            .is_err()
        );
    }
}
