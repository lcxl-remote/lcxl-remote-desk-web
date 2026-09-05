use super::*;
use crate::context_attachment::{
    AttachmentBounds, AttachmentObjectRef, CONTEXT_ATTACHMENT_SCHEMA_VERSION,
};
use crate::dynamic_run::{
    GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionDecisionItem,
    PermissionItemDecision, PermissionRequest, PermissionRequestState,
};
use crate::session::AgentSessionSurface;
use desk_agent_protocol::capability_grant::CapabilityRiskTier;
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
            command_confirmation: None,
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

#[test]
fn owner_freeform_approval_is_exact_one_shot_and_rechecks_the_frozen_policy() {
    let (mut session, mut request, mut decisions) = decision_fixture();
    session.actor_id = "1".into();
    session.scope_snapshot.mode = ExecutionMode::ConfirmEachAction;
    session.turn_start_scope.mode = ExecutionMode::ConfirmEachAction;
    let policy = crate::command_confirmation::test_policy();
    let canonical = serde_json::json!({"schema_version":1,"shell":"bash",
        "command":"du -d 1 \"a directory\" | sort -n\ndf -h", "timeout_ms":20000})
    .to_string();
    let snapshot = policy.prepare(&canonical, 1).unwrap();
    let registry = crate::device_assistant::device_assistant_provider_registry()
        .with_command_policy(policy.clone());
    let capability = registry
        .capability_for_tool(crate::command_confirmation::COMMAND_TOOL)
        .unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let item = &mut request.items[0];
    item.provider_id = provider.wire.provider_id.clone();
    item.tool_name = capability.wire.tool_name.clone();
    item.expected_effect = capability.wire.effect;
    item.resource_scope = snapshot.resource_scope().unwrap();
    item.operation_scope = vec![crate::command_confirmation::COMMAND_TOOL.into()];
    item.canonical_input_json = Some(canonical);
    item.canonical_input_digest_sha256 = Some(snapshot.canonical_input_digest_sha256.clone());
    item.command_confirmation = Some(snapshot);
    item.suggested_max_uses = 1;
    decisions[0].decision = PermissionItemDecision::Approve {
        resource_scope: item.resource_scope.clone(),
        operation_scope: item.operation_scope.clone(),
        export_destinations: vec![],
        ttl_seconds: 60,
        max_uses: 1,
    };
    let inventory = vec![CapabilityAvailability {
        provider_id: provider.wire.provider_id.clone(),
        capability_id: capability.wire.capability_id.clone(),
        tool_name: capability.wire.tool_name.clone(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        let build = |registry: &ProviderRegistry, request: &PermissionRequest| {
            build_permission_grants(
                &session,
                request,
                &decisions,
                &PermissionGrantIssuanceContext {
                    surface,
                    registry,
                    inventory: &inventory,
                    readiness_revision: 7,
                    now_unix_ms: 1000,
                    implicit_fresh_object_refs: &[],
                },
                None,
            )
        };
        let grants = build(&registry, &request).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].remaining_uses, 1);
        assert_eq!(grants[0].risk_tier, CapabilityRiskTier::R3);
        assert_eq!(grants[0].resource_scope, request.items[0].resource_scope);
        let mut changed = policy.clone();
        changed.target_session_id.push_str("-new");
        assert!(build(&registry.clone().with_command_policy(changed), &request).is_err());
        let mut missing = request.clone();
        missing.items[0].command_confirmation = None;
        assert!(build(&registry, &missing).is_err());
        let mut changed = policy.clone();
        changed.admission_policy = desk_agent_protocol::authz::ExecAdmissionPolicy::TemplateOnly;
        assert!(build(&registry.clone().with_command_policy(changed), &request).is_err());
    }
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
        None,
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
fn compiler_rejects_stale_policy_for_approval_and_denial_on_both_servers() {
    let (mut session, request, decisions) = decision_fixture();
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        for revision in [0, 2, i64::MAX] {
            session.policy_revision = revision;
            let before = session.clone();
            assert!(compile(surface, &session, &request, &decisions, true).is_err());
            let mut denied = decisions.clone();
            denied[0].decision = PermissionItemDecision::Deny;
            assert!(compile(surface, &session, &request, &denied, false).is_err());
            assert_eq!(session, before);
        }
        session.policy_revision = crate::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
        let grants = compile(surface, &session, &request, &decisions, true).unwrap();
        assert_eq!(grants[0].policy_revision, session.policy_revision);
        grants[0].validate().unwrap();
    }
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
        connector_id: crate::device_assistant::BRAVE_WEB_SEARCH_CONNECTOR_ID.into(),
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
        None,
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
fn directory_selection_can_back_local_draft_grants() {
    let mut directory = context_attachment("directory", "attach-directory", "worker-1", 1);
    directory.kind = ContextAttachmentKind::DirectorySelection;
    directory.display_summary = "selected draft output directory".into();
    assert!(attachment_matches_fresh_object_capability(
        desk_agent_protocol::Capability::CommunicationLocalDraftCreateConfirmed,
        &directory,
    ));

    let mut unsupported_file = directory;
    unsupported_file.kind = ContextAttachmentKind::File;
    unsupported_file.display_summary = "notes.txt".into();
    assert!(!attachment_matches_fresh_object_capability(
        desk_agent_protocol::Capability::CommunicationLocalDraftCreateConfirmed,
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
