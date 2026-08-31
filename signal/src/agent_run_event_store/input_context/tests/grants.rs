//! Both host surfaces issue object-read authority from the same original input.
use super::*;
use desk_agent_protocol::capability_provider::ProductSurface;
use desk_diagnose_core::{
    capability_availability::CapabilityAvailability,
    dynamic_run::{PermissionDecisionItem, PermissionItemDecision},
    permission_grant::{PermissionGrantIssuanceContext, build_permission_grants},
};

#[tokio::test]
async fn all_object_read_grants_ignore_later_objects_display_names_and_ambient_references() {
    let store = setup("sqlite::memory:").await;
    let file = attach(&store, "original-file", ObjectKind::File).await;
    let directory = attach(&store, "original-directory", ObjectKind::Directory).await;
    let terminal = attach(&store, "original-terminal", ObjectKind::TerminalOutput).await;
    let later = attach(&store, "later", ObjectKind::File).await;
    let mut session = state(&store).await;
    session.input_revision = 1;
    session.policy_revision =
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
    let now = Utc::now().timestamp_millis() as u64;
    let deadline = now + 60_000;
    let ambient = [serde_json::from_str::<ObjectRef>(&later.object_ref.opaque_token).unwrap()];
    let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        for (name, selected) in [
            ("inspect_selected_file_metadata", &file),
            ("read_selected_text_file", &file),
            ("inspect_selected_spreadsheets", &file),
            ("preview_spreadsheet_merge", &file),
            ("inspect_selected_terminal_output", &terminal),
            ("inspect_selected_numbers_with_iwork", &file),
            ("inspect_selected_pages_with_iwork", &file),
            ("inspect_selected_keynote_with_iwork", &file),
            ("inspect_selected_file_metadata", &directory),
            ("inspect_selected_spreadsheets", &directory),
            ("preview_spreadsheet_merge", &directory),
        ] {
            let capability = registry.capability_for_tool(name).unwrap();
            let provider = registry
                .provider_for_capability(&capability.wire.capability_id)
                .unwrap();
            let call = ToolCall { id: "request".into(), name: "request_capability_grants".into(),
                arguments_json: serde_json::json!({"items":[{
                    "item_id":"read", "provider_id":provider.wire.provider_id,
                    "tool_name":name, "expected_effect":capability.wire.effect,
                    "suggested_ttl_seconds":120,"suggested_max_uses":1,"reason":"Read original object"
                }]}).to_string(),
            };
            let request = desk_diagnose_core::permission_tools::build_permission_request(
                &call,
                &registry,
                "request".into(),
                1,
                Utc::now().to_rfc3339(),
            )
            .unwrap();
            let decisions = [PermissionDecisionItem {
                item_id: "read".into(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: request.items[0].resource_scope.clone(),
                    operation_scope: request.items[0].operation_scope.clone(),
                    export_destinations: vec![],
                    ttl_seconds: 120,
                    max_uses: 1,
                },
            }];
            let inventory = [CapabilityAvailability {
                provider_id: provider.wire.provider_id.clone(),
                capability_id: capability.wire.capability_id.clone(),
                tool_name: name.into(),
                compiled: true,
                enabled: true,
                connected: true,
                ready: true,
                reason: None,
            }];
            let context = PermissionGrantIssuanceContext {
                surface,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms: now,
                implicit_fresh_object_refs: &ambient,
            };
            let original = ReadContextSelection {
                tool_names: vec![name.into()],
                expires_at: Some(
                    chrono::DateTime::from_timestamp_millis(deadline as i64)
                        .unwrap()
                        .to_rfc3339(),
                ),
                object_attachments: vec![selected.clone()],
                live_targets: Vec::new(),
            };
            let grants =
                build_permission_grants(&session, &request, &decisions, &context, Some(&original))
                    .unwrap();
            assert_eq!(grants.len(), 1);
            let reference: ObjectRef =
                serde_json::from_str(&selected.object_ref.opaque_token).unwrap();
            assert_eq!(
                grants[0].resource_scope,
                desk_diagnose_core::capability_grant::fresh_object_resource_scope(&[reference]),
                "{surface:?}/{name}"
            );
            assert_eq!(grants[0].expires_at_unix_ms, deadline);
            assert!(
                build_permission_grants(&session, &request, &decisions, &context, None).is_err()
            );
            let denied = [PermissionDecisionItem {
                item_id: "read".into(),
                decision: PermissionItemDecision::Deny,
            }];
            assert!(
                !desk_diagnose_core::permission_grant::requires_original_read_context(
                    &request, &denied
                )
            );
            assert!(
                build_permission_grants(&session, &request, &denied, &context, None)
                    .unwrap()
                    .is_empty()
            );
            let mut expired = original;
            expired.expires_at = Some(
                chrono::DateTime::from_timestamp_millis(now as i64)
                    .unwrap()
                    .to_rfc3339(),
            );
            assert!(
                build_permission_grants(&session, &request, &decisions, &context, Some(&expired))
                    .is_err()
            );
        }
    }
}
