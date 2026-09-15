//! Real SQLite dispatch boundaries; action admission is tested in shared preflight.
use super::*;
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use desk_diagnose_core::{
    file_scope::{DirectoryConsentSource, DirectoryProposal},
    session::AgentSessionSurface,
};

async fn set_directory(db: &DatabaseConnection, directory: &ObjectRef, revoke: bool) {
    let row = agent_session::Entity::find()
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
    if !revoke {
        session
            .begin_focus_epoch(session.input_revision, std::iter::empty::<String>())
            .unwrap();
    }
    let subject = session
        .file_scope_subject("actor-1", "device-1", "run-1")
        .unwrap();
    if revoke {
        session.file_scope.revoke(&subject, 2, "output").unwrap();
    } else {
        session
            .file_scope
            .propose(
                &subject,
                0,
                DirectoryProposal {
                    request_id: "output".into(),
                    requested_path: "D:\\输出".into(),
                    canonical_path: "\\\\?\\D:\\输出".into(),
                    purpose: "approved copy".into(),
                    source: DirectoryConsentSource::ModelProposal,
                    directory: directory.clone(),
                },
                100,
            )
            .unwrap();
        session
            .file_scope
            .decide(&subject, 1, "output", true, 100)
            .unwrap();
    }
    let mut active: agent_session::ActiveModel = row.into();
    let encoded = session.encode_json_for_storage().unwrap();
    PersistedAgentSession::decode_json(&encoded).unwrap();
    active.state_json = Set(encoded);
    active.update(db).await.unwrap();
}

#[tokio::test]
async fn office_directory_revocation_blocks_both_dispatch_boundaries_and_restart_keeps_one_claim() {
    for (tool, provider, capability, extension, kind) in [
        (
            "replace_selected_word_copy_body",
            "office.docx.batch",
            "office.docx.batch.patch",
            "docx",
            ObjectKind::Document,
        ),
        (
            "patch_selected_powerpoint_copy",
            "office.pptx.batch",
            "office.pptx.batch.patch",
            "pptx",
            ObjectKind::Slide,
        ),
    ] {
        for phase in ["before-intent", "before-claim", "restart"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("office.db");
            let db = file_db(&path).await;
            insert_session(&db, 1, 1).await;
            let object = |kind, token: &str| ObjectRef {
                token: token.into(),
                snapshot_id: "worker:1".into(),
                object_kind: kind,
                expires_at: "2030-01-01T00:00:00Z".into(),
            };
            let directory = object(ObjectKind::Directory, "output");
            let target = object(kind, "target");
            set_directory(&db, &directory, false).await;
            let resources = desk_diagnose_core::capability_grant::fresh_object_resource_scope(&[
                target.clone(),
                directory.clone(),
            ]);
            let operations = vec!["use_selected_object".into()];
            let mut args = serde_json::json!({"target":target,"output":{"destination_parent":directory,"native_file_name":format!("copy.{extension}")}});
            if extension == "docx" {
                args["text"] = "approved body".into();
            } else {
                args["action"] = serde_json::json!({"kind":"replace_slide_title","params":{"text":"approved title"}});
            }
            let canonical =
                desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
                    tool, args,
                )
                .unwrap();
            let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
            let mut permission = grant(1);
            permission.provider_id = provider.into();
            permission.capability_id = capability.into();
            permission.tool_name = tool.into();
            permission.effect = CapabilityEffect::MutateApplication;
            permission.resource_scope = resources.clone();
            permission.operation_scope = operations.clone();
            let store = SignalCapabilityGrantStore::new(db.clone());
            store.issue(&permission).await.unwrap();
            let make_request = || {
                let mut value = request(
                    "office-call",
                    &canonical,
                    &digest,
                    &resources,
                    &operations,
                    1,
                );
                value.call.provider_id = provider;
                value.call.capability_id = capability;
                value.call.tool_name = tool;
                value.call.effect = CapabilityEffect::MutateApplication;
                value.call.byte_count = canonical.len() as u64;
                value
            };
            store.prepare(make_request()).await.unwrap();
            if phase == "before-intent" {
                set_directory(&db, &directory, true).await;
                assert!(
                    store.record_dispatch_intent(make_request()).await.is_err(),
                    "{tool}"
                );
                assert_eq!(
                    agent_capability_dispatch_outbox::Entity::find()
                        .count(&db)
                        .await
                        .unwrap(),
                    0
                );
                continue;
            }
            let DispatchIntentResult::Recorded {
                dispatch_id,
                outbox_id,
                ..
            } = store.record_dispatch_intent(make_request()).await.unwrap()
            else {
                panic!()
            };
            if phase == "before-claim" {
                set_directory(&db, &directory, true).await;
                assert!(
                    store.claim_dispatch(&dispatch_id, 600).await.is_err(),
                    "{tool}"
                );
                let outbox = agent_capability_dispatch_outbox::Entity::find_by_id(outbox_id)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(outbox.state, DISPATCH_OUTBOX_PENDING);
                continue;
            }
            drop(store);
            db.close().await.unwrap();
            let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
                .await
                .unwrap();
            let store = SignalCapabilityGrantStore::new(reopened.clone());
            let replay = store.record_dispatch_intent(make_request()).await.unwrap();
            assert_eq!(
                replay,
                DispatchIntentResult::Recorded {
                    dispatch_id: dispatch_id.clone(),
                    outbox_id,
                    idempotent_replay: true
                }
            );
            assert!(matches!(
                store.claim_dispatch(&dispatch_id, 600).await.unwrap(),
                DispatchClaimResult::Claimed(_)
            ));
            assert!(store.claim_dispatch(&dispatch_id, 601).await.is_err());
            let outbox = agent_capability_dispatch_outbox::Entity::find_by_id(outbox_id)
                .one(&reopened)
                .await
                .unwrap()
                .unwrap();
            let payload: CapabilityDispatchPayload =
                serde_json::from_str(&outbox.payload_json).unwrap();
            assert_eq!(payload.tool_name, tool);
            assert_eq!(payload.canonical_input_json, canonical);
            assert_eq!(payload.canonical_input_digest_sha256, digest);
            assert_eq!(
                agent_capability_dispatch_outbox::Entity::find()
                    .count(&reopened)
                    .await
                    .unwrap(),
                1
            );
            let permission = agent_capability_grant::Entity::find()
                .one(&reopened)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(permission.remaining_uses, 0);
        }
    }
}
