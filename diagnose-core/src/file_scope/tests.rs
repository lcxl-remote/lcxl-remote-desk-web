use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};

fn subject() -> FileScopeSubject {
    FileScopeSubject {
        actor_id: "owner".into(),
        device_id: "device".into(),
        conversation_id: "conversation".into(),
    }
}

fn proposal() -> DirectoryProposal {
    DirectoryProposal {
        request_id: "directory-request".into(),
        requested_path: "/private/tmp/assistant-test".into(),
        canonical_path: "/private/tmp/assistant-test".into(),
        directory: ObjectRef {
            token: "device-reference".into(),
            snapshot_id: "device-generation".into(),
            object_kind: ObjectKind::Directory,
            expires_at: "2030-01-01T00:00:00Z".into(),
        },
        purpose: "Create the requested text document".into(),
        source: DirectoryConsentSource::ModelProposal,
    }
}

fn approved() -> SessionFileScope {
    let mut scope = SessionFileScope::default();
    scope.propose(&subject(), 0, proposal(), 1).unwrap();
    scope
        .decide(&subject(), 1, "directory-request", true, 1)
        .unwrap();
    scope
}

#[test]
fn empty_and_pending_scopes_never_authorize_a_directory() {
    let mut scope = SessionFileScope::default();
    assert_eq!(scope.revision(), 0);
    assert!(scope.records().is_empty());
    assert_eq!(
        scope.approved_directory(&subject(), 0, "directory-request", 1),
        Err(FileScopeError::NotApproved)
    );
    scope.propose(&subject(), 0, proposal(), 1).unwrap();
    assert_eq!(
        scope.approved_directory(&subject(), 1, "directory-request", 1),
        Err(FileScopeError::NotApproved)
    );
    scope
        .decide(&subject(), 1, "directory-request", true, 1)
        .unwrap();
    assert_eq!(
        scope.approved_directory(&subject(), 2, "directory-request", 1),
        Ok(&proposal().directory)
    );
}

#[test]
fn artifact_boundary_requires_exact_current_directory_without_granting_tools() {
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
        "2026-09-05T00:00:00Z",
    );
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::DeviceAssistant);
    let tool = "create_text_artifact_in_selected_directory";
    let resources = crate::capability_grant::fresh_object_resource_scope(&[proposal().directory]);
    assert!(validate_artifact_scope(&session, tool, &resources, 1).is_err());
    session.file_scope = approved();
    assert!(validate_artifact_scope(&session, tool, &resources, 1).is_ok());
    assert!(session.scope_snapshot.granted.is_empty());
    assert!(validate_artifact_scope(&session, tool, &[], 1).is_err());
    assert!(
        validate_artifact_scope(&session, tool, &["selected:server_resolved".into()], 1).is_err()
    );
    assert!(validate_artifact_scope(&session, tool, &resources, u64::MAX).is_err());
    session
        .file_scope
        .revoke(&subject(), 2, "directory-request")
        .unwrap();
    assert!(validate_artifact_scope(&session, tool, &resources, 1).is_err());
    assert!(validate_artifact_scope(&session, "read_current_application", &[], 1).is_ok());
}

#[test]
fn manual_selection_and_model_proposals_share_confirmation_rules() {
    for source in [
        DirectoryConsentSource::OwnerSelection,
        DirectoryConsentSource::ModelProposal,
    ] {
        let mut scope = SessionFileScope::default();
        let mut request = proposal();
        request.source = source;
        scope.propose(&subject(), 0, request, 1).unwrap();
        assert_eq!(scope.records()[0].state, DirectoryConsentState::Pending);
        scope
            .decide(&subject(), 1, "directory-request", true, 1)
            .unwrap();
        assert!(
            scope
                .approved_directory(&subject(), 2, "directory-request", 1)
                .is_ok()
        );
    }
}

#[test]
fn all_native_file_writes_require_a_current_directory_but_live_edits_do_not() {
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
        "2026-09-05T00:00:00Z",
    );
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
    session.file_scope = approved();
    let root = crate::capability_grant::fresh_object_resource_scope(&[proposal().directory]);
    let resources = vec!["object:exact-source".into(), root[0].clone()];
    for tool in [
        "update_text_file",
        "delete_text_file",
        "patch_selected_numbers_copy",
        "replace_selected_pages_copy_body",
        "patch_selected_keynote_copy",
    ] {
        assert!(
            validate_artifact_scope(&session, tool, &resources, 1).is_ok(),
            "{tool}"
        );
        assert!(
            validate_artifact_scope(&session, tool, &root, 1).is_err(),
            "{tool}"
        );
        assert!(
            validate_artifact_scope(&session, tool, &[root[0].clone(), root[0].clone()], 1)
                .is_err(),
            "{tool}"
        );
    }
    session
        .file_scope
        .revoke(&subject(), 2, "directory-request")
        .unwrap();
    for tool in [
        "update_text_file",
        "delete_text_file",
        "patch_selected_numbers_copy",
        "replace_selected_pages_copy_body",
        "patch_selected_keynote_copy",
    ] {
        assert!(
            validate_artifact_scope(&session, tool, &resources, 1).is_err(),
            "{tool}"
        );
    }
    for tool in [
        "patch_live_spreadsheet_cell",
        "replace_live_document_body",
        "patch_live_presentation_slide",
    ] {
        assert!(
            validate_artifact_scope(&session, tool, &[], 1).is_ok(),
            "{tool}"
        );
    }
}

#[test]
fn cross_actor_device_and_conversation_cannot_read_or_mutate_scope() {
    let mut scope = approved();
    let before = scope.clone();
    for foreign in [
        FileScopeSubject {
            actor_id: "other".into(),
            ..subject()
        },
        FileScopeSubject {
            device_id: "other".into(),
            ..subject()
        },
        FileScopeSubject {
            conversation_id: "other".into(),
            ..subject()
        },
    ] {
        assert_eq!(
            scope.approved_directory(&foreign, 2, "directory-request", 1),
            Err(FileScopeError::WrongSubject)
        );
        assert_eq!(
            scope.revoke(&foreign, 2, "directory-request"),
            Err(FileScopeError::WrongSubject)
        );
        assert_eq!(
            scope.propose(&foreign, 2, proposal(), 1),
            Err(FileScopeError::WrongSubject)
        );
    }
    assert_eq!(scope, before);
}

#[test]
fn retries_are_idempotent_but_changed_paths_and_refs_conflict() {
    let mut scope = approved();
    let before = scope.clone();
    assert_eq!(scope.propose(&subject(), 0, proposal(), 1), Ok(false));
    assert_eq!(
        scope.decide(&subject(), 1, "directory-request", true, 1),
        Ok(false)
    );
    for change_path in [true, false] {
        let mut changed = proposal();
        if change_path {
            changed.canonical_path = "/private/tmp/other".into();
        } else {
            changed.directory.snapshot_id = "replaced-directory".into();
        }
        assert_eq!(
            scope.propose(&subject(), 2, changed, 1),
            Err(FileScopeError::RequestConflict)
        );
    }
    assert_eq!(scope, before);
}

#[test]
fn revoke_fences_old_dispatch_and_cannot_be_undone_by_delayed_approval() {
    let mut scope = approved();
    scope.revoke(&subject(), 2, "directory-request").unwrap();
    assert_eq!(
        scope.approved_directory(&subject(), 2, "directory-request", 1),
        Err(FileScopeError::StaleRevision)
    );
    assert_eq!(
        scope.approved_directory(&subject(), 3, "directory-request", 1),
        Err(FileScopeError::NotApproved)
    );
    assert_eq!(
        scope.decide(&subject(), 1, "directory-request", true, 1),
        Err(FileScopeError::InvalidTransition)
    );
    assert_eq!(scope.propose(&subject(), 0, proposal(), 1), Ok(false));
    assert_eq!(scope.revoke(&subject(), 2, "directory-request"), Ok(false));
    assert_eq!(scope.revision(), 3);
    assert_eq!(scope.records()[0].state, DirectoryConsentState::Revoked);
}

#[test]
fn stale_revision_and_expired_reference_fail_without_changing_state() {
    let mut scope = SessionFileScope::default();
    scope.propose(&subject(), 0, proposal(), 1).unwrap();
    let before = scope.clone();
    assert_eq!(
        scope.decide(&subject(), 0, "directory-request", true, 1),
        Err(FileScopeError::StaleRevision)
    );
    assert_eq!(
        scope.decide(&subject(), 1, "directory-request", true, u64::MAX),
        Err(FileScopeError::ExpiredReference)
    );
    assert_eq!(scope, before);
    scope
        .decide(&subject(), 1, "directory-request", true, 1)
        .unwrap();
    assert_eq!(
        scope.approved_directory(&subject(), 2, "directory-request", u64::MAX),
        Err(FileScopeError::ExpiredReference)
    );
    // Expiry must never prevent revocation.
    scope.revoke(&subject(), 2, "directory-request").unwrap();
}

#[test]
fn rejection_and_cancellation_are_terminal_even_for_expired_proposals() {
    for cancel in [true, false] {
        let mut scope = SessionFileScope::default();
        scope.propose(&subject(), 0, proposal(), 1).unwrap();
        if cancel {
            scope.revoke(&subject(), 1, "directory-request").unwrap();
        } else {
            scope
                .decide(&subject(), 1, "directory-request", false, u64::MAX)
                .unwrap();
        }
        assert_eq!(
            scope.decide(&subject(), 2, "directory-request", true, 1),
            Err(FileScopeError::InvalidTransition)
        );
    }
}

#[test]
fn rejects_bad_metadata_and_bounds_directory_records() {
    let mut scope = SessionFileScope::default();
    let mut wrong_kind = proposal();
    wrong_kind.directory.object_kind = ObjectKind::File;
    let mut empty_token = proposal();
    empty_token.directory.token.clear();
    let mut excessive_purpose = proposal();
    excessive_purpose.purpose = "x".repeat(2049);
    for bad in [wrong_kind, empty_token, excessive_purpose] {
        assert_eq!(
            scope.propose(&subject(), 0, bad, 1),
            Err(FileScopeError::InvalidProposal)
        );
    }
    let mut bad = proposal();
    bad.canonical_path.push('\n');
    assert_eq!(
        scope.propose(&subject(), 0, bad, 1),
        Err(FileScopeError::InvalidProposal)
    );
    let mut bad = proposal();
    bad.directory.expires_at = "not a timestamp".into();
    assert_eq!(
        scope.propose(&subject(), 0, bad, 1),
        Err(FileScopeError::InvalidProposal)
    );
    for index in 0..MAX_SESSION_DIRECTORY_RECORDS {
        let mut next = proposal();
        next.request_id = format!("request-{index}");
        scope.propose(&subject(), index as u64, next, 1).unwrap();
    }
    let before = scope.clone();
    assert_eq!(
        scope.propose(&subject(), scope.revision(), proposal(), 1),
        Err(FileScopeError::CapacityExceeded)
    );
    assert_eq!(scope, before);
    // Exhausted proposal capacity must not prevent revocation.
    scope
        .revoke(&subject(), scope.revision(), "request-0")
        .unwrap();
}

#[test]
fn conversation_storage_preserves_scope_without_putting_it_in_model_messages() {
    let mut session = PersistedAgentSession::new(
        "conversation",
        "owner",
        "device",
        0,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-05T00:00:00Z",
    );
    assert_eq!(
        session.file_scope_subject("owner", "device", "conversation"),
        Err(FileScopeError::WrongSubject)
    );
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::DeviceAssistant);
    assert_eq!(
        session.file_scope_subject("owner", "device", "browser-intent"),
        Err(FileScopeError::WrongSubject)
    );
    assert_eq!(
        session.file_scope_subject("owner", "device", "conversation"),
        Ok(subject())
    );
    session.file_scope = approved();
    let restored =
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    assert_eq!(restored.file_scope, session.file_scope);
    assert!(restored.conversation.is_empty());
    assert!(restored.scope_snapshot.granted.is_empty());
    assert!(
        restored
            .file_scope
            .approved_directory(&subject(), 2, "directory-request", 1)
            .is_ok()
    );
}

#[test]
fn canonical_path_changes_require_confirmation_and_retries_bind_original_intent() {
    use super::transaction::*;
    use desk_agent_protocol::computer_use::FileDirectoryResolveOutput;
    use desk_agent_protocol::device_assistant::{
        DeviceAssistantObjectContextOperation, DeviceAssistantObjectContextUpdate,
    };
    let wire = DeviceAssistantObjectContextUpdate {
        conversation_id: "browser-intent".into(),
        client_request_id: "directory-request".into(),
        operation: DeviceAssistantObjectContextOperation::SelectDirectory {
            path: "/tmp/assistant-test".into(),
            purpose: "test".into(),
            expected_revision: 0,
        },
    };
    let resolved = FileDirectoryResolveOutput {
        canonical_path: "/private/tmp/assistant-test".into(),
        directory: proposal().directory,
    };
    let selection = owner_selection(&wire, subject(), resolved).unwrap();
    assert!(matches!(
        selection.mutation,
        FileScopeMutation::Propose { .. }
    ));
    let mut session = PersistedAgentSession::new(
        "conversation",
        "owner",
        "device",
        0,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-05T00:00:00Z",
    );
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::DeviceAssistant);
    let (mut stored, receipt) = prepare(&session, &selection, 1).unwrap();
    assert_eq!(
        stored.file_scope.records()[0].state,
        DirectoryConsentState::Pending
    );
    assert!(match_owner_selection(&receipt, &wire, &subject()).is_ok());
    let mut changed = wire.clone();
    if let DeviceAssistantObjectContextOperation::SelectDirectory { path, .. } =
        &mut changed.operation
    {
        *path = "/other".into();
    }
    assert_eq!(
        match_owner_selection(&receipt, &changed, &subject()),
        Err(FileScopeError::RequestConflict)
    );
    stored
        .file_scope
        .revoke(&subject(), 1, "directory-request")
        .unwrap();
    stored.file_scope.archive_terminal_records();
    assert!(replay(&stored, &selection, &receipt).is_ok());
    assert!(stored.file_scope.records().is_empty());
}

#[test]
fn owner_selection_transaction_is_atomic_and_does_not_grant_tool_execution() {
    use super::transaction::*;
    let mut session = PersistedAgentSession::new(
        "conversation",
        "owner",
        "device",
        0,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-05T00:00:00Z",
    );
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::DeviceAssistant);
    let mut directory = proposal();
    directory.source = DirectoryConsentSource::OwnerSelection;
    let update = FileScopeUpdate {
        subject: subject(),
        client_conversation_id: "browser-intent".into(),
        client_request_id: directory.request_id.clone(),
        expected_revision: 0,
        mutation: FileScopeMutation::Select {
            proposal: directory,
        },
    };
    assert!(prepare(&session, &update, u64::MAX).is_err());
    assert_eq!(session.file_scope.revision(), 0);
    let (stored, receipt) = prepare(&session, &update, 1).unwrap();
    assert_eq!(stored.file_scope.revision(), 2);
    assert_eq!(
        stored.file_scope.records()[0].state,
        DirectoryConsentState::Approved
    );
    assert!(stored.scope_snapshot.granted.is_empty());
    assert!(stored.conversation.is_empty());
    assert!(replay(&stored, &update, &receipt).is_ok());
    assert!(prepare(&stored, &update, 1).is_err());
}
