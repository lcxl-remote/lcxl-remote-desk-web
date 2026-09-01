use super::*;
use crate::{
    assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
    device_assistant::device_assistant_provider_registry,
};
use serde_json::json;

const NOW: u64 = 1_800_000_000_000;

fn directory() -> ObjectRef {
    ObjectRef {
        token: "opaque-directory".into(),
        snapshot_id: "directory-snapshot".into(),
        object_kind: ObjectKind::Directory,
        expires_at: chrono::DateTime::from_timestamp_millis((NOW + 60_000) as i64)
            .unwrap()
            .to_rfc3339(),
    }
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments_json: arguments.to_string(),
    }
}

#[test]
fn both_orchestrators_derive_closed_create_new_artifacts() {
    let cases = [
        (
            call(
                "create_text_artifact_in_selected_directory",
                json!({"file_name":"notes.txt","content_utf8":"hello"}),
            ),
            Capability::FileArtifactCreateConfirmed,
        ),
        (
            call(
                "create_workbook_from_merge_preview",
                json!({"preview_id":"preview-1","file_name":"merged.xlsx"}),
            ),
            Capability::SpreadsheetWorkbookCreateConfirmed,
        ),
        (
            call(
                "create_formula_workbook_from_merge_preview",
                json!({
                    "preview_id":"preview-1",
                    "file_name":"formula.xlsx",
                    "target_cell":"Merged!C2",
                    "formula":"=SUM(Merged!A2:B2)",
                    "locale":"en-US-a1"
                }),
            ),
            Capability::SpreadsheetFormulaWorkbookCreateConfirmed,
        ),
        (
            call(
                "create_word_report_from_merge_preview",
                json!({
                    "preview_id":"preview-1",
                    "file_name":"report.docx",
                    "title":"Report",
                    "web_search_call_id":"search-1",
                    "web_sources":[{"title":"Source","url":"https://example.com/source"}]
                }),
            ),
            Capability::WordDocumentCreateConfirmed,
        ),
        (
            call(
                "create_local_communication_draft",
                json!({
                    "file_name":"message.draft.txt",
                    "draft":{
                        "schema_version":3,
                        "recipients":[{"role":"to","address":"person@example.com","display_name":null}],
                        "subject":"Subject",
                        "body_plain_text":"Body",
                        "attachment_labels":[]
                    }
                }),
            ),
            Capability::CommunicationLocalDraftCreateConfirmed,
        ),
    ];
    let registry = device_assistant_provider_registry();
    for (call, expected_capability) in cases {
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let preflight =
                ArtifactCallPreflight::build(&registry, surface, &call, &[directory()], NOW)
                    .unwrap();
            assert_eq!(preflight.target(), &directory());
            assert_eq!(preflight.required_capability(), expected_capability);
            let authority = preflight
                .grant_call(&ProviderCallSubject {
                    actor_id: "owner",
                    run_id: "run",
                    target_device_id: "device",
                    policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
                    readiness_revision: 9,
                    now_unix_ms: NOW,
                })
                .unwrap();
            assert_eq!(authority.surface, surface);
            assert_eq!(authority.resource_scope, preflight.resource_scope());
            assert_eq!(authority.operation_scope, ["create_new_artifact"]);
            assert!(authority.export_destinations.is_empty());
            assert_eq!(authority.item_count, 1);
        }
    }
}

#[test]
fn artifact_preflight_rejects_ambiguous_directory_and_unbounded_inputs() {
    let registry = device_assistant_provider_registry();
    let valid = call(
        "create_text_artifact_in_selected_directory",
        json!({"file_name":"notes.txt","content_utf8":"hello"}),
    );
    assert!(
        ArtifactCallPreflight::build(
            &registry,
            ProductSurface::ManagerPersonalOwner,
            &valid,
            &[],
            NOW,
        )
        .is_err()
    );
    assert!(
        ArtifactCallPreflight::build(
            &registry,
            ProductSurface::ManagerPersonalOwner,
            &valid,
            &[directory(), directory()],
            NOW,
        )
        .is_err()
    );
    for invalid in [
        call(
            "create_text_artifact_in_selected_directory",
            json!({"file_name":"notes.txt","content_utf8":""}),
        ),
        call(
            "create_text_artifact_in_selected_directory",
            json!({"file_name":"../notes.txt","content_utf8":"hello"}),
        ),
        call(
            "create_workbook_from_merge_preview",
            json!({"preview_id":"preview-1","file_name":"merged.txt"}),
        ),
        call(
            "create_word_report_from_merge_preview",
            json!({
                "preview_id":"preview-1",
                "file_name":"report.docx",
                "title":"Report",
                "web_sources":[{"title":"invented","url":"https://example.com"}]
            }),
        ),
    ] {
        assert!(
            ArtifactCallPreflight::build(
                &registry,
                ProductSurface::ManagerPersonalOwner,
                &invalid,
                &[directory()],
                NOW,
            )
            .is_err()
        );
    }
}

#[test]
fn formula_policy_digest_is_computed_by_the_shared_validator() {
    let call = call(
        "create_formula_workbook_from_merge_preview",
        json!({
            "preview_id":"preview-1",
            "file_name":"formula.xlsx",
            "target_cell":"Merged!C2",
            "formula":"=SUM(Merged!A2:B2)",
            "locale":"en-US-a1"
        }),
    );
    let FilePatchAction::CreateSpreadsheetFormulaArtifact {
        formula_policy_digest_sha256,
        ..
    } = artifact_action_from_call(&call).unwrap()
    else {
        panic!("expected formula artifact")
    };
    assert_eq!(formula_policy_digest_sha256.len(), 64);
}
