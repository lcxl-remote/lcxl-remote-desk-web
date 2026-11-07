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
            expires_at: String::new(),
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
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::AiAssistant);
    let tool = "create_text_file";
    let resources = crate::capability_grant::fresh_object_resource_scope(&[proposal().directory]);
    assert!(validate_artifact_scope(&session, tool, &resources, 1).is_err());
    session.file_scope = approved();
    assert!(validate_artifact_scope(&session, tool, &resources, 1).is_ok());
    assert!(session.scope_snapshot.granted.is_empty());
    assert!(validate_artifact_scope(&session, tool, &[], 1).is_err());
    assert!(
        validate_artifact_scope(&session, tool, &["selected:server_resolved".into()], 1).is_err()
    );
    assert!(validate_artifact_scope(&session, tool, &resources, u64::MAX).is_ok());
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
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::AiAssistant);
    session.file_scope = approved();
    let root = crate::capability_grant::fresh_object_resource_scope(&[proposal().directory]);
    let resources = vec!["object:exact-source".into(), root[0].clone()];
    for tool in [
        "update_text_file",
        "delete_text_file",
        "patch_numbers_copy",
        "replace_pages_copy_body",
        "patch_keynote_copy",
        "patch_powerpoint_copy",
        "replace_word_copy_body",
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
        "patch_numbers_copy",
        "replace_pages_copy_body",
        "patch_keynote_copy",
        "patch_powerpoint_copy",
        "replace_word_copy_body",
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
fn selectable_scope_round_trips_same_path_with_independent_device_authority() {
    use crate::conversation_key::{AssistantConversationScope, derive_assistant_conversation_key};
    let client_id = "selectable";
    let run =
        derive_assistant_conversation_key("owner", AssistantConversationScope::Personal, client_id);
    let mut session = PersistedAgentSession::new(
        &run,
        "owner",
        "",
        1,
        AgentScope {
            granted: vec![],
            expires_at: None,
            mode: ExecutionMode::ReadOnly,
            policy_name: None,
        },
        "2026-09-05T00:00:00Z",
    );
    session.adopt_client_metadata(Some(client_id), AgentSessionSurface::AiAssistant);
    let a = session
        .file_scope_subject("owner", "device-a", &run)
        .unwrap();
    let b = session
        .file_scope_subject("owner", "device-b", &run)
        .unwrap();
    for (index, device) in [&a, &b].into_iter().enumerate() {
        let mut proposal = proposal();
        proposal.request_id = format!("directory-{index}");
        proposal.source = DirectoryConsentSource::OwnerSelection;
        let update = transaction::FileScopeUpdate {
            subject: device.clone(),
            client_conversation_id: client_id.into(),
            client_request_id: proposal.request_id.clone(),
            expected_revision: session.file_scope.revision(),
            mutation: transaction::FileScopeMutation::Select { proposal },
        };
        (session, _) = transaction::prepare(&session, &update, 1).unwrap();
    }
    let json = session.encode_json_for_storage().unwrap();
    let mut session = PersistedAgentSession::decode_json(&json).unwrap();
    session.input_revision = 1;
    session
        .bind_assistant_request_targets(vec!["device-a".into(), "device-b".into()])
        .unwrap();
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
    for (device, selected, other) in [
        (&a, "directory-0", "directory-1"),
        (&b, "directory-1", "directory-0"),
    ] {
        for surface in [
            desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
            desk_agent_protocol::capability_provider::ProductSurface::ManagerPersonalOwner,
        ] {
            let call = crate::chat::ToolCall { id: "write".into(), name: "create_text_file".into(),
                arguments_json: serde_json::json!({"directory_request_id":selected,"file_name":"notes.txt","content_utf8":"owner content"}).to_string() };
            let input = crate::provider_preflight::ArtifactCallPreflight::from_device_session(
                &registry,
                surface,
                &session,
                &device.device_id,
                &call,
                1,
            )
            .unwrap();
            validate_device_artifact_scope(
                &session,
                &device.device_id,
                &call.name,
                input.resource_scope(),
                1,
            )
            .unwrap();
            let model_call = crate::chat::ToolCall { id:"write-model".into(), name:"create_text_file".into(),
                arguments_json:serde_json::json!({"target_ref":device.device_id,"file_name":"notes.txt","content_utf8":"owner content"}).to_string() };
            let resolved = crate::ui_model_ids::resolve_session_call_with_context(
                &model_call,
                &session,
                1,
                None,
            )
            .unwrap();
            let (target, native) = crate::request_devices::split_device_call(&resolved).unwrap();
            assert_eq!(target, device.device_id);
            let native: serde_json::Value = serde_json::from_str(&native.arguments_json).unwrap();
            assert_eq!(native["directory_request_id"], selected);
            assert!(native.get("target_ref").is_none());
            let wrong = crate::chat::ToolCall { arguments_json: serde_json::json!({"directory_request_id":other,"file_name":"notes.txt","content_utf8":"owner content"}).to_string(), ..call };
            assert!(
                crate::provider_preflight::ArtifactCallPreflight::from_device_session(
                    &registry,
                    surface,
                    &session,
                    &device.device_id,
                    &wrong,
                    1,
                )
                .is_err()
            );
        }
    }
    let scope = &mut session.file_scope;
    let revision = scope.revision();
    assert!(
        scope
            .approved_directory(&a, revision, "directory-0", 1)
            .is_ok()
    );
    assert!(
        scope
            .approved_directory(&b, revision, "directory-1", 1)
            .is_ok()
    );
    assert_eq!(
        scope.approved_directory(&b, revision, "directory-0", 1),
        Err(FileScopeError::WrongSubject)
    );
    assert_eq!(
        scope.revoke(&a, revision, "directory-1"),
        Err(FileScopeError::WrongSubject)
    );
    assert_eq!(scope.revoke(&a, revision, "directory-0"), Ok(true));
    assert!(
        scope
            .approved_directory(&b, scope.revision(), "directory-1", 1)
            .is_ok()
    );
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
fn directory_consent_has_no_time_limit_but_still_checks_revision_and_revocation() {
    let mut scope = SessionFileScope::default();
    let mut selected = proposal();
    selected.directory.expires_at.clear();
    scope.propose(&subject(), 0, selected, 1).unwrap();
    let before = scope.clone();
    assert_eq!(
        scope.decide(&subject(), 0, "directory-request", true, 1),
        Err(FileScopeError::StaleRevision)
    );
    assert_eq!(scope, before);
    scope
        .decide(&subject(), 1, "directory-request", true, u64::MAX)
        .unwrap();
    assert!(
        scope
            .approved_directory(&subject(), 2, "directory-request", u64::MAX)
            .is_ok()
    );
    scope.revoke(&subject(), 2, "directory-request").unwrap();
    assert_eq!(
        scope.approved_directory(&subject(), 3, "directory-request", u64::MAX),
        Err(FileScopeError::NotApproved)
    );
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
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::AiAssistant);
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
    use desk_agent_protocol::ai_assistant::{
        AiAssistantObjectContextOperation, AiAssistantObjectContextUpdate,
    };
    use desk_agent_protocol::computer_use::FileDirectoryResolveOutput;
    let wire = AiAssistantObjectContextUpdate {
        conversation_target:
            desk_agent_protocol::ai_assistant::AiAssistantConversationTarget::FixedDevice,
        conversation_id: "browser-intent".into(),
        client_request_id: "directory-request".into(),
        operation: AiAssistantObjectContextOperation::SelectDirectory {
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
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::AiAssistant);
    let (mut stored, receipt) = prepare(&session, &selection, 1).unwrap();
    assert_eq!(
        stored.file_scope.records()[0].state,
        DirectoryConsentState::Pending
    );
    assert!(match_owner_selection(&receipt, &wire, &subject()).is_ok());
    let mut changed = wire.clone();
    if let AiAssistantObjectContextOperation::SelectDirectory { path, .. } = &mut changed.operation
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
fn windows_prefix_conversion_keeps_owner_selection_but_not_target_changes() {
    use super::transaction::*;
    use desk_agent_protocol::ai_assistant::{
        AiAssistantObjectContextOperation, AiAssistantObjectContextUpdate,
    };
    use desk_agent_protocol::computer_use::FileDirectoryResolveOutput;
    let wire = AiAssistantObjectContextUpdate {
        conversation_target:
            desk_agent_protocol::ai_assistant::AiAssistantConversationTarget::FixedDevice,
        conversation_id: "browser-intent".into(),
        client_request_id: "directory-request".into(),
        operation: AiAssistantObjectContextOperation::SelectDirectory {
            path: r"D:\测试 输入".into(),
            purpose: "test".into(),
            expected_revision: 0,
        },
    };
    for (canonical, approved) in [(r"\\?\D:\测试 输入", true), (r"\\?\D:\other", false)] {
        let selection = owner_selection(
            &wire,
            subject(),
            FileDirectoryResolveOutput {
                canonical_path: canonical.into(),
                directory: proposal().directory,
            },
        )
        .unwrap();
        assert_eq!(
            matches!(selection.mutation, FileScopeMutation::Select { .. }),
            approved
        );
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
        session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::AiAssistant);
        let (stored, receipt) = prepare(&session, &selection, 1).unwrap();
        assert_eq!(
            stored.file_scope.records()[0].state == DirectoryConsentState::Approved,
            approved
        );
        assert!(stored.scope_snapshot.granted.is_empty());
        match_owner_selection(&receipt, &wire, &subject()).unwrap();
        let mut changed = wire.clone();
        let AiAssistantObjectContextOperation::SelectDirectory { path, .. } =
            &mut changed.operation
        else {
            panic!()
        };
        *path = canonical.into();
        assert!(match_owner_selection(&receipt, &changed, &subject()).is_err());
    }
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
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::AiAssistant);
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

#[test]
fn task_directory_consent_keeps_distinct_source_and_requires_active_scheduled_run() {
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
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::AiAssistant);
    let mut directory = proposal();
    directory.source = DirectoryConsentSource::TaskContract;
    let mut update = FileScopeUpdate {
        subject: subject(),
        client_conversation_id: "browser-intent".into(),
        client_request_id: directory.request_id.clone(),
        expected_revision: 0,
        mutation: FileScopeMutation::Select {
            proposal: directory.clone(),
        },
    };
    assert!(prepare(&session, &update, 1).is_err());
    session.trigger_origin = crate::session::TriggerOrigin::ScheduledTask;
    session.turn_state = crate::session::TurnState::Running;
    let (stored, receipt) = prepare(&session, &update, 1).unwrap();
    assert_eq!(
        stored.file_scope.records()[0].proposal.source,
        DirectoryConsentSource::TaskContract
    );
    assert_eq!(
        stored.file_scope.records()[0].state,
        DirectoryConsentState::Approved
    );
    assert!(stored.scope_snapshot.granted.is_empty());
    assert_eq!(receipt.update, update);
    assert_eq!(session.file_scope.revision(), 0);
    update.mutation = FileScopeMutation::Propose {
        proposal: directory,
    };
    assert!(update.validate().is_err());
}

#[test]
fn conversion_directory_boundary_accepts_two_exact_resources_and_still_rejects_wrong_directories() {
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
        "now",
    );
    session.adopt_client_metadata(Some("browser-intent"), AgentSessionSurface::AiAssistant);
    session.file_scope = approved();
    let directory = proposal().directory;
    let mut source = directory.clone();
    source.object_kind = ObjectKind::File;
    source.token = "source".into();
    let resources =
        crate::capability_grant::fresh_object_resource_scope(&[source.clone(), directory.clone()]);
    validate_artifact_scope(&session, "convert_document", &resources, 1).unwrap();
    assert!(validate_artifact_scope(&session, "convert_document", &resources[..1], 1).is_err());
    assert!(validate_artifact_scope(&session, "create_text_file", &resources, 1).is_err());
    let mut wrong_directory = directory;
    wrong_directory.token = "unapproved-directory".into();
    let wrong = crate::capability_grant::fresh_object_resource_scope(&[source, wrong_directory]);
    assert!(validate_artifact_scope(&session, "convert_document", &wrong, 1).is_err());
    session
        .file_scope
        .revoke(&subject(), 2, "directory-request")
        .unwrap();
    assert!(validate_artifact_scope(&session, "convert_document", &resources, 1).is_err());
}
