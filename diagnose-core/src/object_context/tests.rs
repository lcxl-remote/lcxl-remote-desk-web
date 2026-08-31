use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "model-connection".into(),
        connection_revision: 1,
        model_id: "configured-model".into(),
        profile_revision: 1,
    }
}

fn update(kind: ObjectKind) -> DeviceAssistantObjectContextUpdate {
    let object_ref = ObjectRef {
        token: "opaque-ref".into(),
        snapshot_id: "generation-1".into(),
        object_kind: kind,
        expires_at: "2030-01-01T00:00:00Z".into(),
    };
    let operation = if kind == ObjectKind::TerminalOutput {
        DeviceAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref,
            display_summary: "Selected output".into(),
        }
    } else {
        DeviceAssistantObjectContextOperation::AttachFile {
            object_ref,
            display_summary: "Selected file".into(),
        }
    };
    DeviceAssistantObjectContextUpdate {
        conversation_id: "client-conversation".into(),
        client_request_id: "client-request".into(),
        operation,
    }
}

fn build(
    update: &DeviceAssistantObjectContextUpdate,
    id: &str,
    now: u64,
) -> Result<ObjectContextMutation, AgentError> {
    build_object_context_mutation(
        update,
        ObjectContextBuild {
            actor_id: "7",
            device_id: "11",
            destination: &destination(),
            now_unix_ms: now,
            attachment_id: id,
            observation_id: id,
        },
    )
}

fn attachment(kind: ObjectKind) -> ContextAttachment {
    let ObjectContextMutation::Attach(attachment) =
        build(&update(kind), "attachment-1", 1).unwrap()
    else {
        panic!("expected attachment")
    };
    attachment
}

fn session() -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "run-1",
        "7",
        "11",
        0,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-08-31T00:00:00Z",
    );
    session.adopt_client_metadata(
        Some("client-conversation"),
        AgentSessionSurface::DeviceAssistant,
    );
    session
}

#[test]
fn selections_keep_original_refs_bounds_destination_and_metadata_digest() {
    for (kind, expected_kind, max_bytes, max_objects) in [
        (ObjectKind::File, ContextAttachmentKind::File, 65536, 32),
        (
            ObjectKind::Directory,
            ContextAttachmentKind::DirectorySelection,
            65536,
            32,
        ),
        (
            ObjectKind::TerminalOutput,
            ContextAttachmentKind::TerminalSessionRef,
            32768,
            8,
        ),
    ] {
        let value = attachment(kind);
        value.validate().unwrap();
        assert_eq!(value.kind, expected_kind);
        assert_eq!(
            value.bounds,
            AttachmentBounds {
                max_bytes,
                max_objects
            }
        );
        assert_eq!(value.envelope.allowed_destinations, vec![destination()]);
        assert_eq!(value.envelope.sensitivity, Sensitivity::Sensitive);
        assert_eq!(value.expires_at_unix_ms, 1893456000000);
        assert_eq!(
            value.envelope.retention.expires_at_unix_ms,
            Some(value.expires_at_unix_ms)
        );
        let original: ObjectRef = serde_json::from_str(&value.object_ref.opaque_token).unwrap();
        assert_eq!(original.object_kind, kind);
        assert_eq!(original.token, "opaque-ref");
        let metadata = serde_json::to_vec(&serde_json::json!({ "provider_id": value.object_ref.source_provider_id, "capability_id": value.object_ref.source_capability_id, "object_ref": original, "display_summary": value.display_summary })).unwrap();
        assert_eq!(
            value.envelope.digest_sha256,
            format!("{:x}", Sha256::digest(&metadata))
        );
        assert!(
            matches!(value.envelope.content, ContentRef::EphemeralObservation { size_bytes, .. } if size_bytes == metadata.len() as u64)
        );
    }
}

#[test]
fn invalid_selection_never_builds_authority() {
    assert!(build(&update(ObjectKind::File), "id", 1893456000000).is_err());
    assert!(build(&update(ObjectKind::File), "id", 0).is_err());
    assert!(build(&update(ObjectKind::TerminalOutput), "", 1).is_err());
    let mut request = update(ObjectKind::File);
    let DeviceAssistantObjectContextOperation::AttachFile {
        display_summary, ..
    } = &mut request.operation
    else {
        unreachable!()
    };
    *display_summary = "x".repeat(513);
    assert!(build(&request, "id", 1).is_err());
    let DeviceAssistantObjectContextOperation::AttachFile {
        object_ref,
        display_summary,
    } = &mut request.operation
    else {
        unreachable!()
    };
    *display_summary = "file".into();
    object_ref.object_kind = ObjectKind::TerminalOutput;
    assert!(build(&request, "id", 1).is_err());
    let request = update(ObjectKind::File);
    assert!(
        build_object_context_mutation(
            &request,
            ObjectContextBuild {
                actor_id: "7",
                device_id: "11",
                destination: &DestinationIdentity::LocalArtifact {
                    workspace_id: "workspace".into()
                },
                now_unix_ms: 1,
                attachment_id: "id",
                observation_id: "obs"
            }
        )
        .is_err()
    );
}

#[test]
fn replay_preserves_original_identity_and_never_reactivates_detached_selection() {
    let mut session = session();
    let request = update(ObjectKind::File);
    assert!(apply_object_mutation(&mut session, &build(&request, "first", 1).unwrap()).unwrap());
    let original = session.context_attachments.clone();
    assert!(!apply_object_mutation(&mut session, &build(&request, "second", 2).unwrap()).unwrap());
    assert_eq!(session.context_attachments, original);
    assert!(
        apply_object_mutation(
            &mut session,
            &ObjectContextMutation::Detach {
                attachment_id: "first".into()
            }
        )
        .unwrap()
    );
    let detached = session.context_attachments.clone();
    assert!(!apply_object_mutation(&mut session, &build(&request, "third", 3).unwrap()).unwrap());
    assert_eq!(session.context_attachments, detached);
}

#[test]
fn duplicate_object_does_not_bypass_subject_or_request_conflict_checks() {
    let mut session = session();
    let original = attachment(ObjectKind::File);
    apply_object_mutation(
        &mut session,
        &ObjectContextMutation::Attach(original.clone()),
    )
    .unwrap();
    for alter in 0..5 {
        let mut changed = original.clone();
        match alter {
            0 => changed.actor_id = "8".into(),
            1 => changed.device_id = "12".into(),
            2 => changed.surface = AgentSessionSurface::Unknown,
            3 => changed.display_summary = "another selection".into(),
            _ => {
                if let DestinationIdentity::Model {
                    connection_revision,
                    ..
                } = &mut changed.envelope.allowed_destinations[0]
                {
                    *connection_revision = 2;
                }
            }
        }
        assert!(
            apply_object_mutation(&mut session, &ObjectContextMutation::Attach(changed)).is_err()
        );
        assert_eq!(session.context_attachments, vec![original.clone()]);
    }
}

#[test]
fn refresh_is_atomic_and_replays_original_replacement() {
    let mut session = session();
    apply_object_mutation(
        &mut session,
        &ObjectContextMutation::Attach(attachment(ObjectKind::File)),
    )
    .unwrap();
    let mut request = update(ObjectKind::File);
    request.client_request_id = "refresh-1".into();
    let DeviceAssistantObjectContextOperation::AttachFile {
        mut object_ref,
        display_summary,
    } = request.operation
    else {
        unreachable!()
    };
    object_ref.token = "new-ref".into();
    request.operation = DeviceAssistantObjectContextOperation::RefreshFile {
        stale_attachment_id: "attachment-1".into(),
        object_ref,
        display_summary,
    };
    assert!(
        apply_object_mutation(&mut session, &build(&request, "replacement", 2).unwrap()).unwrap()
    );
    let refreshed = session.context_attachments.clone();
    assert!(
        !apply_object_mutation(&mut session, &build(&request, "replayed", 3).unwrap()).unwrap()
    );
    assert_eq!(session.context_attachments, refreshed);
    request.client_request_id = "refresh-2".into();
    if let DeviceAssistantObjectContextOperation::RefreshFile {
        stale_attachment_id,
        ..
    } = &mut request.operation
    {
        *stale_attachment_id = "missing".into();
    }
    assert!(apply_object_mutation(&mut session, &build(&request, "invalid", 4).unwrap()).is_err());
    assert_eq!(session.context_attachments, refreshed);
}
