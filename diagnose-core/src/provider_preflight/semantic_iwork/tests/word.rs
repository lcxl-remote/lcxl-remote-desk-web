use super::*;
use crate::{
    chat::{ChatMessage, ChatRole, ToolCallRef},
    device_assistant::windows_word,
    file_scope::{DirectoryConsentSource, DirectoryProposal},
};
use desk_agent_protocol::computer_use::{
    BatchDocumentSourceProjection, ComputerUseAdapterRef, LiveDocumentInspectOutput,
    LiveDocumentProjection, office_batch,
};

#[test]
fn word_approval_requires_authenticated_read_selected_source_and_current_directory() {
    let mut file = reference(ObjectKind::File);
    file.token = "source".into();
    let mut directory = reference(ObjectKind::Directory);
    directory.token = "directory".into();
    let document = derived_reference(ObjectKind::Document);
    let original = ReadContextSelection {
        tool_names: vec![windows_word::INSPECT_TOOL.into()],
        expires_at: None,
        object_attachments: vec![attachment("file", ContextAttachmentKind::File, &file)],
        live_targets: vec![],
    };
    let call = ToolCall { id: "copy".into(), name: windows_word::PATCH_TOOL.into(), arguments_json: serde_json::json!({
        "target": document, "output": {"destination_parent": directory, "native_file_name":"copy.docx"}, "text":"approved body"
    }).to_string() };
    let registry = device_assistant_provider_registry();
    assert!(
        IworkCallPreflight::build(
            &registry,
            ProductSurface::OssPersonalOwner,
            &call,
            &original,
            std::slice::from_ref(&directory),
            NOW
        )
        .is_err()
    );
    let mut session = crate::session::PersistedAgentSession::new(
        "conversation",
        "owner",
        "device",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            expires_at: None,
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            policy_name: None,
        },
        "2026-08-31T00:00:01Z",
    );
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
    let subject = session
        .file_scope_subject("owner", "device", "conversation")
        .unwrap();
    session
        .file_scope
        .propose(
            &subject,
            0,
            DirectoryProposal {
                request_id: "output".into(),
                requested_path: "D:\\输出".into(),
                canonical_path: "\\\\?\\D:\\输出".into(),
                purpose: "new copy".into(),
                source: DirectoryConsentSource::ModelProposal,
                directory: directory.clone(),
            },
            NOW,
        )
        .unwrap();
    session
        .file_scope
        .decide(&subject, 1, "output", true, NOW)
        .unwrap();
    let output = LiveDocumentInspectOutput {
        snapshot_id: document.snapshot_id.clone(),
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::OfficeWord,
            version: office_batch::DOCX_ADAPTER_VERSION.into(),
        },
        batch_source: Some(BatchDocumentSourceProjection {
            file: file.clone(),
            display_name: "source.docx".into(),
            byte_len: 100,
            sha256: "a".repeat(64),
        }),
        projection: LiveDocumentProjection::Document {
            document: document.clone(),
            body_text: "body".into(),
            body_sha256: format!("{:x}", Sha256::digest(b"body")),
        },
    };
    let text = serde_json::to_string(&desk_agent_protocol::OperationOutput::ReadContext(
        desk_agent_protocol::ReadContextOutput::DocumentLiveInspect(output),
    ))
    .unwrap();
    let mut proposal = ChatMessage::text("proposal", ChatRole::Assistant, "");
    proposal.tool_calls.push(ToolCallRef {
        id: "read".into(),
        name: windows_word::INSPECT_TOOL.into(),
        arguments_json: "{}".into(),
    });
    let mut receipt = ChatMessage::tool_result("receipt", "read", text.clone());
    let mut envelope = original.object_attachments[0].envelope.clone();
    envelope.provenance.source_provider_id = windows_word::PROVIDER_ID.into();
    envelope.provenance.source_tool_name = windows_word::INSPECT_TOOL.into();
    envelope.digest_sha256 = format!("{:x}", Sha256::digest(text.as_bytes()));
    envelope.content = ContentRef::EphemeralObservation {
        observation_id: "read".into(),
        size_bytes: text.len() as u64,
        expires_at_unix_ms: NOW + 30000,
    };
    receipt.data_envelope = Some(envelope);
    session.conversation = vec![proposal, receipt];
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        let build = |session: &crate::session::PersistedAgentSession,
                     original: &ReadContextSelection,
                     worker: &str,
                     now: u64| {
            IworkCallPreflight::from_session(
                &registry, surface, &call, original, session, worker, now,
            )
        };
        let approved = build(&session, &original, "501:worker-1:7", NOW).unwrap();
        assert_eq!(approved.adapter_kind(), ComputerUseAdapterKind::OfficeWord);
        assert_eq!(approved.target(), &document);
        assert_eq!(
            approved.resource_scope(),
            fresh_object_resource_scope(&[document.clone(), directory.clone()])
        );
        assert_eq!(
            IworkCallPreflight::frozen_presentation_resources(
                &call,
                approved.target(),
                approved.action()
            )
            .unwrap(),
            approved.resource_scope()
        );
        assert!(build(&session, &original, "501:other-worker", NOW).is_err());
        assert!(build(&session, &original, "501:worker-1:7", NOW + 30000).is_err());
        let mut other_file = file.clone();
        other_file.token = "other".into();
        let mut wrong_selection = original.clone();
        wrong_selection.object_attachments[0] =
            attachment("other", ContextAttachmentKind::File, &other_file);
        assert!(build(&session, &wrong_selection, "501:worker-1:7", NOW).is_err());
        for case in 0..8 {
            let mut wrong = session.clone();
            match case {
                0 => wrong.conversation[1].role = ChatRole::User,
                1 => wrong.conversation[1].text.push(' '),
                2 => wrong.conversation[1].data_envelope = None,
                3 => {
                    wrong.conversation[1]
                        .data_envelope
                        .as_mut()
                        .unwrap()
                        .provenance
                        .source_provider_id = "other".into()
                }
                4 => wrong.conversation[0].tool_calls[0].name = "inspect_live_document".into(),
                5 => wrong.conversation.push(wrong.conversation[1].clone()),
                6 => wrong.conversation.push(wrong.conversation[0].clone()),
                _ => {
                    wrong.file_scope.revoke(&subject, 2, "output").unwrap();
                }
            }
            assert!(
                build(&wrong, &original, "501:worker-1:7", NOW).is_err(),
                "case {case}"
            );
        }
    }
}
