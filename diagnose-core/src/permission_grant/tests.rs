use super::*;
use crate::context_attachment::{
    AttachmentBounds, AttachmentObjectRef, CONTEXT_ATTACHMENT_SCHEMA_VERSION,
};
use crate::dynamic_run::{
    GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionDecisionItem,
    PermissionItemDecision, PermissionRequest, PermissionRequestState,
};
use crate::session::AgentSessionSurface;
use desk_agent_protocol::capability_provider::CapabilityEffect;
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
    Sensitivity,
};
use desk_agent_protocol::{AgentScope, ExecutionMode};

fn decision_fixture() -> (
    PersistedAgentSession,
    PermissionRequest,
    Vec<PermissionDecisionItem>,
) {
    let mut session = PersistedAgentSession::new(
        "run-1",
        "owner-1",
        "device-1",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-08-30T00:00:00Z",
    );
    session.input_revision = 1;
    let request = PermissionRequest {
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id: "permission-1".into(),
        input_revision: 1,
        state: PermissionRequestState::Pending,
        items: vec![GrantRequestItem {
            item_id: "inspect".into(),
            provider_id: "desktop.session".into(),
            tool_name: "inspect_desktop_session".into(),
            expected_effect: CapabilityEffect::ReadDevice,
            resource_scope: vec!["target:current_device".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: vec![],
            canonical_input_json: None,
            canonical_input_digest_sha256: None,
            suggested_ttl_seconds: 120,
            suggested_max_uses: 2,
            reason: "Inspect the current device".into(),
        }],
        created_at: "2026-08-30T00:00:00Z".into(),
    };
    let decisions = vec![PermissionDecisionItem {
        item_id: "inspect".into(),
        decision: PermissionItemDecision::Approve {
            resource_scope: request.items[0].resource_scope.clone(),
            operation_scope: request.items[0].operation_scope.clone(),
            export_destinations: vec![],
            ttl_seconds: 60,
            max_uses: 1,
        },
    }];
    (session, request, decisions)
}

fn compile(
    surface: ProductSurface,
    session: &PersistedAgentSession,
    request: &PermissionRequest,
    decisions: &[PermissionDecisionItem],
    ready: bool,
) -> Result<Vec<CapabilityGrant>, AgentError> {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let inventory = vec![CapabilityAvailability {
        provider_id: "desktop.session".into(),
        capability_id: "desktop.session.inspect".into(),
        tool_name: "inspect_desktop_session".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready,
        reason: None,
    }];
    build_permission_grants(
        session,
        request,
        decisions,
        &PermissionGrantIssuanceContext {
            surface,
            registry: &registry,
            inventory: &inventory,
            readiness_revision: 7,
            now_unix_ms: 1000,
            implicit_fresh_object_refs: &[],
        },
    )
}

#[test]
fn oss_and_manager_compile_identical_narrowed_authority_except_surface() {
    let (session, request, decisions) = decision_fixture();
    let original = session.clone();
    let mut oss = compile(
        ProductSurface::OssPersonalOwner,
        &session,
        &request,
        &decisions,
        true,
    )
    .unwrap();
    let manager = compile(
        ProductSurface::ManagerPersonalOwner,
        &session,
        &request,
        &decisions,
        true,
    )
    .unwrap();
    assert_eq!(manager.len(), 1);
    oss[0].surface = ProductSurface::ManagerPersonalOwner;
    assert_eq!(oss, manager);
    assert_eq!(manager[0].actor_id, "owner-1");
    assert_eq!(manager[0].target_device_id, "device-1");
    assert_eq!(manager[0].remaining_uses, 1);
    assert_eq!(manager[0].expires_at_unix_ms, 61_000);
    assert_eq!(
        session.encode_json_for_storage().unwrap(),
        original.encode_json_for_storage().unwrap()
    );
    assert_eq!(request.state, PermissionRequestState::Pending);
}

#[test]
fn compiler_rejects_incomplete_duplicate_widened_stale_and_unready_decisions() {
    let (mut session, request, mut decisions) = decision_fixture();
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        assert!(compile(surface, &session, &request, &[], true).is_err());
        let duplicate = vec![decisions[0].clone(), decisions[0].clone()];
        assert!(compile(surface, &session, &request, &duplicate, true).is_err());
        assert!(compile(surface, &session, &request, &decisions, false).is_err());
        session.input_revision = 2;
        assert!(compile(surface, &session, &request, &decisions, true).is_err());
        session.input_revision = 1;
    }
    if let PermissionItemDecision::Approve { resource_scope, .. } = &mut decisions[0].decision {
        resource_scope.push("target:other_device".into());
    }
    assert!(
        compile(
            ProductSurface::ManagerPersonalOwner,
            &session,
            &request,
            &decisions,
            true
        )
        .is_err()
    );
}

#[test]
fn denial_creates_no_grant_even_when_provider_is_unready() {
    let (session, request, mut decisions) = decision_fixture();
    decisions[0].decision = PermissionItemDecision::Deny;
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        assert!(
            compile(surface, &session, &request, &decisions, false)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn exact_external_grant_never_restores_removed_scopes_or_destinations() {
    let (session, mut request, mut decisions) = decision_fixture();
    let canonical = r#"{"query":"test","max_results":1}"#.to_string();
    let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
    let item = &mut request.items[0];
    item.provider_id = "web.search".into();
    item.tool_name = "search_public_web".into();
    item.expected_effect = CapabilityEffect::ExportData;
    item.resource_scope = crate::capability_grant::exact_external_query_resource_scope(&digest);
    item.operation_scope = vec!["search_public_web".into()];
    item.export_destinations = vec![DestinationIdentity::WebResearch {
        connector_id: crate::device_assistant::DUCKDUCKGO_HTML_CONNECTOR_ID.into(),
    }];
    item.canonical_input_json = Some(canonical);
    item.canonical_input_digest_sha256 = Some(digest);
    decisions[0].decision = PermissionItemDecision::Approve {
        resource_scope: Vec::new(),
        operation_scope: Vec::new(),
        export_destinations: Vec::new(),
        ttl_seconds: 30,
        max_uses: 1,
    };
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let capability = registry
        .provider("web.search")
        .unwrap()
        .capabilities
        .iter()
        .find(|capability| capability.tool_spec.name == "search_public_web")
        .unwrap();
    let inventory = vec![CapabilityAvailability {
        provider_id: item.provider_id.clone(),
        capability_id: capability.wire.capability_id.clone(),
        tool_name: item.tool_name.clone(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let grants = build_permission_grants(
        &session,
        &request,
        &decisions,
        &PermissionGrantIssuanceContext {
            surface: ProductSurface::OssPersonalOwner,
            registry: &registry,
            inventory: &inventory,
            readiness_revision: 7,
            now_unix_ms: 1000,
            implicit_fresh_object_refs: &[],
        },
    )
    .unwrap();
    assert_eq!(grants.len(), 1);
    assert!(grants[0].resource_scope.is_empty());
    assert!(grants[0].operation_scope.is_empty());
    assert!(grants[0].export_destinations.is_empty());
}

fn context_attachment(
    id: &str,
    client_request_id: &str,
    incarnation: &str,
    now_unix_ms: u64,
) -> ContextAttachment {
    ContextAttachment {
        schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        attachment_id: id.into(),
        client_request_id: client_request_id.into(),
        actor_id: "1".into(),
        device_id: "device-1".into(),
        surface: AgentSessionSurface::DeviceAssistant,
        kind: ContextAttachmentKind::InteractiveSession,
        object_ref: AttachmentObjectRef {
            opaque_token: format!("opaque-{id}"),
            object_incarnation: incarnation.into(),
            source_provider_id: "desktop.ui".into(),
            source_capability_id: "desktop.ui.inspect".into(),
        },
        bounds: AttachmentBounds {
            max_bytes: 1024,
            max_objects: 16,
        },
        display_summary: "desktop.ui.inspect on the current interactive session".into(),
        created_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms + 60_000,
        envelope: DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{id}"),
            content: ContentRef::EphemeralObservation {
                observation_id: format!("observation-{id}"),
                size_bytes: 1,
                expires_at_unix_ms: now_unix_ms + 60_000,
            },
            provenance: DataProvenance {
                source_provider_id: "desktop.ui".into(),
                source_tool_name: "inspect_desktop_ui".into(),
                source_object_id: Some(format!("opaque-{id}")),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: "a".repeat(64),
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(now_unix_ms + 60_000),
                delete_with_run: false,
            },
        },
        state: AttachmentState::Active,
    }
}

#[test]
fn directory_selection_can_back_spreadsheet_and_local_draft_grants() {
    let mut directory = context_attachment("directory", "attach-directory", "worker-1", 1);
    directory.kind = ContextAttachmentKind::DirectorySelection;
    directory.display_summary = "selected spreadsheet input directory".into();

    assert!(attachment_matches_fresh_object_capability(
        desk_agent_protocol::Capability::SpreadsheetFileInspect,
        &directory,
    ));
    assert!(attachment_matches_fresh_object_capability(
        desk_agent_protocol::Capability::SpreadsheetMergePreview,
        &directory,
    ));
    assert!(!attachment_matches_fresh_object_capability(
        desk_agent_protocol::Capability::FileContentRead,
        &directory,
    ));
    assert!(attachment_matches_fresh_object_capability(
        desk_agent_protocol::Capability::CommunicationLocalDraftCreateConfirmed,
        &directory,
    ));

    let mut unsupported_file = directory;
    unsupported_file.kind = ContextAttachmentKind::File;
    unsupported_file.display_summary = "notes.txt".into();
    assert!(!attachment_matches_fresh_object_capability(
        desk_agent_protocol::Capability::SpreadsheetFileInspect,
        &unsupported_file,
    ));
}

#[test]
fn readiness_browser_surface_can_back_only_browser_grants() {
    let surface = ObjectRef {
        token: "browser-surface".into(),
        snapshot_id: "browser-connection-1".into(),
        object_kind: ObjectKind::BrowserSurface,
        expires_at: "2026-08-27T12:00:00Z".into(),
    };

    assert!(object_ref_matches_fresh_object_capability(
        desk_agent_protocol::Capability::BrowserPageNavigateConfirmed,
        &surface,
    ));
    assert!(object_ref_matches_fresh_object_capability(
        desk_agent_protocol::Capability::BrowserInputFallbackConfirmed,
        &surface,
    ));
    assert!(!object_ref_matches_fresh_object_capability(
        desk_agent_protocol::Capability::FileMetadataRead,
        &surface,
    ));
}
