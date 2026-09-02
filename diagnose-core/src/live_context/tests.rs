use super::*;
use crate::context_attachment::AttachmentStaleReason;
use desk_agent_protocol::{
    AgentScope, Capability, ExecutionMode,
    computer_use::{ComputerUseContextReference, ObjectKind, ObjectRef},
};

fn readiness() -> ComputerUseReadiness {
    ComputerUseReadiness {
        schema_version: 1,
        revision: 1,
        observed_at: "2026-08-31T00:00:00Z".into(),
        expires_at: "2026-08-31T00:01:00Z".into(),
        server_api_version: 1,
        os: "macos".into(),
        interactive_session_incarnation: "worker-1".into(),
        local_ceiling_revision: 1,
        capabilities: vec![],
        context_references: vec![],
    }
}

fn now() -> u64 {
    chrono::DateTime::parse_from_rfc3339("2026-08-31T00:00:01Z")
        .unwrap()
        .timestamp_millis() as u64
}

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "model-connection".into(),
        connection_revision: 1,
        model_id: "model-1".into(),
        profile_revision: 1,
    }
}

fn build(
    ids: &[String],
    readiness: Option<&ComputerUseReadiness>,
    ready: bool,
) -> Result<ContextSelectionClaim, AgentError> {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let inventory = ids
        .iter()
        .map(|id| CapabilityAvailability {
            provider_id: registry
                .provider_for_capability(id)
                .map_or("unknown", |p| p.wire.provider_id.as_str())
                .into(),
            capability_id: id.clone(),
            tool_name: "inspect".into(),
            compiled: true,
            enabled: true,
            connected: ready,
            ready,
            reason: None,
        })
        .collect::<Vec<_>>();
    let mut nonce = 0;
    build_live_context(
        LiveContextBuild {
            registry: &registry,
            inventory: &inventory,
            readiness,
            selected_capability_ids: ids,
            request_id: "request-1",
            actor_id: "owner-1",
            device_id: "device-1",
            destination: &destination(),
            now_unix_ms: now(),
        },
        || {
            nonce += 1;
            format!("opaque-{nonce}")
        },
    )
}

fn selection() -> ContextSelectionClaim {
    build(&["desktop.ui.inspect".into()], Some(&readiness()), true).unwrap()
}

fn session() -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "run-1",
        "owner-1",
        "device-1",
        0,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-08-31T00:00:01Z",
    );
    session.adopt_client_metadata(Some("client-1"), AgentSessionSurface::DeviceAssistant);
    session
}

#[test]
fn live_metadata_keeps_original_destination_expiry_and_does_not_grant_tools() {
    let selection = selection();
    let mut session = session();
    assert!(reconcile_live_context(&mut session, &selection).unwrap());
    let original = session.clone();
    assert!(!reconcile_live_context(&mut session, &selection).unwrap());
    assert_eq!(session, original);
    assert_eq!(session.context_attachments.len(), 1);
    let attachment = &session.context_attachments[0];
    assert_eq!(
        attachment.envelope.allowed_destinations,
        vec![destination()]
    );
    assert_eq!(
        attachment.envelope.retention.expires_at_unix_ms,
        Some(attachment.expires_at_unix_ms)
    );
    assert!(matches!(
        attachment.envelope.content,
        ContentRef::EphemeralObservation { .. }
    ));
    assert!(session.scope_snapshot.granted.is_empty());
    assert_eq!(session.input_revision, 0);
    assert!(session.conversation.is_empty());
}

#[test]
fn live_metadata_uses_matching_edge_reference_expiry_beyond_readiness_heartbeat() {
    let mut readiness = readiness();
    let reference_expiry = "2026-08-31T00:05:00Z";
    readiness.context_references = vec![ComputerUseContextReference {
        capability: Capability::PresentationLiveInspect,
        object_ref: ObjectRef {
            token: "keynote-token".into(),
            snapshot_id: "keynote-snapshot".into(),
            object_kind: ObjectKind::Slide,
            expires_at: reference_expiry.into(),
        },
    }];
    let selection = build(
        &[crate::device_assistant::PRESENTATION_BATCH_INSPECT_CAPABILITY_ID.into()],
        Some(&readiness),
        true,
    )
    .unwrap();

    assert_eq!(
        selection.candidates[0].expires_at_unix_ms,
        chrono::DateTime::parse_from_rfc3339(reference_expiry)
            .unwrap()
            .timestamp_millis() as u64
    );
    assert!(
        selection.candidates[0].expires_at_unix_ms
            > chrono::DateTime::parse_from_rfc3339(&readiness.expires_at)
                .unwrap()
                .timestamp_millis() as u64
    );
}

#[test]
fn duplicate_candidate_cannot_bypass_subject_surface_kind_or_expiry_validation() {
    let mut session = session();
    let selection = selection();
    reconcile_live_context(&mut session, &selection).unwrap();
    let original = session.clone();
    for change in ["actor", "device", "surface", "kind", "expiry", "selection"] {
        let mut changed = selection.clone();
        match change {
            "actor" => changed.candidates[0].actor_id = "another".into(),
            "device" => changed.candidates[0].device_id = "another".into(),
            "surface" => changed.candidates[0].surface = AgentSessionSurface::Unknown,
            "kind" => changed.candidates[0].kind = ContextAttachmentKind::File,
            "expiry" => changed.now_unix_ms = changed.candidates[0].expires_at_unix_ms,
            "selection" => changed.selected_capability_ids.clear(),
            _ => unreachable!(),
        }
        assert!(
            reconcile_live_context(&mut session, &changed).is_err(),
            "{change}"
        );
        assert_eq!(session, original, "{change}");
    }
}

#[test]
fn empty_selection_removes_live_context_without_touching_object_attachments() {
    let mut session = session();
    let selection = selection();
    reconcile_live_context(&mut session, &selection).unwrap();
    let mut file = session.context_attachments[0].clone();
    file.attachment_id = "file-1".into();
    file.kind = ContextAttachmentKind::File;
    session.context_attachments.push(file.clone());
    let cleared = build(&[], None, false).unwrap();
    assert!(reconcile_live_context(&mut session, &cleared).unwrap());
    assert!(matches!(
        session.context_attachments[0].state,
        AttachmentState::Stale { .. }
    ));
    assert_eq!(session.context_attachments[1], file);
}

#[test]
fn worker_change_replaces_live_metadata_without_rebinding_the_old_reference() {
    let mut session = session();
    reconcile_live_context(&mut session, &selection()).unwrap();
    let original = session.context_attachments[0].clone();
    let mut current = readiness();
    current.interactive_session_incarnation = "worker-2".into();
    let mut next = build(&["desktop.ui.inspect".into()], Some(&current), true).unwrap();
    next.candidates[0].attachment_id = "replacement".into();
    next.candidates[0].client_request_id = "replacement-request".into();
    reconcile_live_context(&mut session, &next).unwrap();
    assert_eq!(
        session.context_attachments[0].object_ref,
        original.object_ref
    );
    assert_eq!(
        session.context_attachments[0].state,
        AttachmentState::Stale {
            reason: AttachmentStaleReason::WorkerRespawned
        }
    );
    assert_eq!(
        session.context_attachments[1].object_ref.object_incarnation,
        "worker-2"
    );
}

#[test]
fn screen_is_ephemeral_and_invalid_or_unready_context_is_rejected() {
    let screen = build(
        &[crate::device_assistant::CURRENT_SCREEN_CAPABILITY_ID.into()],
        Some(&readiness()),
        true,
    )
    .unwrap();
    assert!(screen.candidates.is_empty());
    let ids = ["desktop.ui.inspect".into()];
    assert!(build(&ids, None, true).is_err());
    assert!(build(&ids, Some(&readiness()), false).is_err());
    let mut expired = readiness();
    expired.expires_at = "2026-08-31T00:00:00Z".into();
    assert!(build(&ids, Some(&expired), true).is_err());
    assert!(build(&["unrecognized".into()], Some(&readiness()), true).is_err());
}

#[test]
fn durable_selection_validation_rejects_screen_unknown_and_duplicate_ids() {
    use desk_agent_protocol::device_assistant::DeviceAssistantContextUpdate;
    let mut update = DeviceAssistantContextUpdate {
        conversation_id: "client-1".into(),
        client_request_id: "request-1".into(),
        selected_capability_ids: vec![],
    };
    assert!(validate_durable_update(&update).is_ok());
    for ids in [
        vec![crate::device_assistant::CURRENT_SCREEN_CAPABILITY_ID.into()],
        vec!["unknown".into()],
        vec!["desktop.ui.inspect".into(), "desktop.ui.inspect".into()],
    ] {
        update.selected_capability_ids = ids;
        assert!(validate_durable_update(&update).is_err());
    }
}

#[test]
fn explicit_selection_for_a_changed_model_keeps_old_metadata_immutable() {
    let mut session = session();
    let mut next = selection();
    reconcile_live_context(&mut session, &next).unwrap();
    let original = session.context_attachments[0].clone();
    next.candidates[0].attachment_id = "changed-model-context".into();
    next.candidates[0].client_request_id = "changed-model-request".into();
    next.candidates[0].envelope.allowed_destinations = vec![DestinationIdentity::Model {
        connection_id: "model-connection".into(),
        connection_revision: 2,
        model_id: "model-2".into(),
        profile_revision: 1,
    }];
    assert!(reconcile_live_context(&mut session, &next).unwrap());
    assert_eq!(session.context_attachments.len(), 2);
    assert_eq!(
        session.context_attachments[0].object_ref,
        original.object_ref
    );
    assert_eq!(session.context_attachments[0].envelope, original.envelope);
    assert_eq!(
        session.context_attachments[0].state,
        AttachmentState::Stale {
            reason: AttachmentStaleReason::PolicyNarrowed
        }
    );
    assert_eq!(
        session.context_attachments[1].envelope.allowed_destinations,
        next.candidates[0].envelope.allowed_destinations
    );
}
