use super::*;
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use desk_diagnose_core::file_scope::{DirectoryConsentSource, DirectoryProposal, FileScopeSubject};
use sea_orm::{ConnectionTrait, Database, Schema};

async fn setup() -> SignalAgentSessionStore {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    let schema = Schema::new(db.get_database_backend());
    for table in [
        schema.create_table_from_entity(agent_session::Entity),
        schema.create_table_from_entity(agent_run_event::Entity),
    ] {
        db.execute(&table).await.unwrap();
    }
    scoped(db)
}

fn scoped(db: DatabaseConnection) -> SignalAgentSessionStore {
    SignalAgentSessionStore::new(db)
        .with_client_metadata(Some("client".into()), AgentSessionSurface::AiAssistant)
}

fn update() -> FileScopeUpdate {
    FileScopeUpdate {
        subject: FileScopeSubject {
            actor_id: "7".into(),
            device_id: "device".into(),
            conversation_id: "run".into(),
        },
        client_conversation_id: "client".into(),
        client_request_id: "select".into(),
        expected_revision: 0,
        mutation: FileScopeMutation::Select {
            proposal: DirectoryProposal {
                request_id: "select".into(),
                requested_path: "/private/tmp/test".into(),
                canonical_path: "/private/tmp/test".into(),
                purpose: "Create a file".into(),
                source: DirectoryConsentSource::OwnerSelection,
                directory: ObjectRef {
                    token: "opaque".into(),
                    snapshot_id: "generation".into(),
                    object_kind: ObjectKind::Directory,
                    expires_at: "2030-01-01T00:00:00Z".into(),
                },
            },
        },
    }
}

fn now() -> DateTime<Utc> {
    "2026-09-05T00:00:00.123456Z".parse().unwrap()
}

async fn state(store: &SignalAgentSessionStore) -> PersistedAgentSession {
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[tokio::test]
async fn stale_directory_decisions_leave_no_receipt_or_partial_state() {
    for path in [r"C:\用户资料\季度 报告", "/Users/owner/季度 报告"] {
        let store = setup().await;
        let mut proposed = update();
        let FileScopeMutation::Select { mut proposal } = proposed.mutation else {
            panic!()
        };
        proposal.source = DirectoryConsentSource::ModelProposal;
        proposal.requested_path = path.into();
        proposal.canonical_path = path.into();
        proposal.directory.expires_at = (now() + chrono::Duration::seconds(1)).to_rfc3339();
        proposed.mutation = FileScopeMutation::Propose { proposal };
        store.update_file_scope(&proposed, now()).await.unwrap();
        let before = state(&store).await;
        let mut decision = FileScopeUpdate {
            client_request_id: "decide".into(),
            expected_revision: before.file_scope.revision(),
            mutation: FileScopeMutation::Decide {
                directory_request_id: "select".into(),
                approve: true,
            },
            ..update()
        };
        for (revision, time) in [
            (0, now()),
            (
                before.file_scope.revision() + 1,
                now() + chrono::Duration::seconds(1),
            ),
        ] {
            decision.expected_revision = revision;
            assert!(store.update_file_scope(&decision, time).await.is_err());
            assert_eq!(state(&store).await, before);
            assert!(
                agent_run_event::Entity::find()
                    .filter(agent_run_event::Column::EventId.eq(decision.event_id()))
                    .one(&store.db)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        // A failed approval has no receipt to poison a later explicit rejection.
        decision.expected_revision = before.file_scope.revision();
        decision.mutation = FileScopeMutation::Decide {
            directory_request_id: "select".into(),
            approve: false,
        };
        let expired = now() + chrono::Duration::seconds(2);
        let receipt = store.update_file_scope(&decision, expired).await.unwrap();
        let rejected = state(&store).await;
        assert_eq!(
            rejected.file_scope.revision(),
            before.file_scope.revision() + 1
        );
        assert_eq!(rejected.scope_snapshot, before.scope_snapshot);
        assert!(
            desk_diagnose_core::file_scope::approved_directories(
                &rejected,
                expired.timestamp_millis() as u64,
            )
            .unwrap()
            .is_empty()
        );
        let reopened = scoped(store.db.clone());
        assert_eq!(
            reopened
                .update_file_scope(&decision, expired)
                .await
                .unwrap(),
            receipt
        );
        assert_eq!(state(&reopened).await, rejected);
    }
}

#[tokio::test]
async fn receipt_and_scope_survive_reopen_and_expired_replay_without_resurrection() {
    for (requested, canonical) in [
        ("/private/tmp/test", "/private/tmp/test"),
        (r"D:\测试 输入", r"\\?\D:\测试 输入"),
        (r"D:\", r"\\?\D:\"),
    ] {
        verify_persisted_directory_replay(requested, canonical).await;
    }
}

async fn verify_persisted_directory_replay(requested: &str, canonical: &str) {
    let store = setup().await;
    let mut selected = update();
    let FileScopeMutation::Select { proposal } = &mut selected.mutation else {
        unreachable!()
    };
    proposal.requested_path = requested.into();
    proposal.canonical_path = canonical.into();
    let first = store.update_file_scope(&selected, now()).await.unwrap();
    assert_eq!(first.scope_revision, 2);
    assert!(state(&store).await.scope_snapshot.granted.is_empty());
    let reopened = scoped(store.db.clone());
    let persisted = state(&reopened).await;
    assert_eq!(
        persisted.file_scope.records()[0].proposal.requested_path,
        requested
    );
    assert_eq!(
        persisted.file_scope.records()[0].proposal.canonical_path,
        canonical
    );
    let mut changed_intent = selected.clone();
    let FileScopeMutation::Select { proposal } = &mut changed_intent.mutation else {
        unreachable!()
    };
    proposal.requested_path = "/different-request".into();
    assert!(
        reopened
            .update_file_scope(&changed_intent, now())
            .await
            .is_err()
    );
    assert_eq!(state(&reopened).await, persisted);
    assert_eq!(
        reopened.update_file_scope(&selected, now()).await.unwrap(),
        first
    );
    let revoke = FileScopeUpdate {
        client_request_id: "revoke".into(),
        expected_revision: 2,
        mutation: FileScopeMutation::Revoke {
            directory_request_id: "select".into(),
        },
        ..selected.clone()
    };
    reopened.update_file_scope(&revoke, now()).await.unwrap();
    assert!(state(&store).await.file_scope.records().is_empty());
    let before = state(&store).await;
    assert_eq!(
        store
            .update_file_scope(&selected, "2040-01-01T00:00:00Z".parse().unwrap())
            .await
            .unwrap(),
        first
    );
    assert_eq!(state(&store).await, before);
}

#[tokio::test]
async fn model_proposal_advances_only_its_held_version_and_never_approves() {
    let store = setup().await;
    store.update_file_scope(&update(), now()).await.unwrap();
    let mut held = state(&store).await;
    held.turn_state = desk_diagnose_core::session::TurnState::Running;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(held.encode_json_for_storage().unwrap()),
        )
        .exec(&store.db)
        .await
        .unwrap();
    let FileScopeMutation::Select { mut proposal } = update().mutation else {
        panic!()
    };
    proposal.request_id = "model-request".into();
    proposal.source = DirectoryConsentSource::ModelProposal;
    let stale = held.clone();
    store
        .propose_file_scope_for_turn(&mut held, proposal.clone(), now())
        .await
        .unwrap();
    assert_eq!(held.version, stale.version + 1);
    assert_eq!(held.file_scope, state(&store).await.file_scope);
    assert_eq!(
        held.file_scope.records().last().unwrap().state,
        desk_diagnose_core::file_scope::DirectoryConsentState::Pending
    );
    let mut stale = stale;
    proposal.request_id = "another-model-request".into();
    assert!(
        store
            .propose_file_scope_for_turn(&mut stale, proposal, now())
            .await
            .is_err()
    );
    assert_eq!(held.file_scope, state(&store).await.file_scope);
}

#[tokio::test]
async fn conflicting_and_cross_subject_requests_do_not_change_stored_state() {
    let store = setup().await;
    let selected = update();
    store.update_file_scope(&selected, now()).await.unwrap();
    let before = state(&store).await;
    let mut conflict = selected.clone();
    if let FileScopeMutation::Select { proposal } = &mut conflict.mutation {
        proposal.canonical_path.push_str("-other");
    }
    assert!(store.update_file_scope(&conflict, now()).await.is_err());
    for subject in [
        FileScopeSubject {
            actor_id: "8".into(),
            ..selected.subject.clone()
        },
        FileScopeSubject {
            device_id: "other".into(),
            ..selected.subject.clone()
        },
    ] {
        let foreign = FileScopeUpdate {
            subject,
            ..selected.clone()
        };
        assert!(store.update_file_scope(&foreign, now()).await.is_err());
    }
    assert_eq!(state(&store).await, before);
}

#[tokio::test]
async fn missing_receipt_and_stale_updates_fail_without_partial_writes() {
    let store = setup().await;
    let selected = update();
    store.update_file_scope(&selected, now()).await.unwrap();
    let before = state(&store).await;
    let stale = FileScopeUpdate {
        client_request_id: "remove".into(),
        mutation: FileScopeMutation::Revoke {
            directory_request_id: "select".into(),
        },
        ..selected.clone()
    };
    assert!(store.update_file_scope(&stale, now()).await.is_err());
    agent_run_event::Entity::delete_many()
        .exec(&store.db)
        .await
        .unwrap();
    assert!(store.update_file_scope(&selected, now()).await.is_err());
    assert_eq!(state(&store).await, before);
}

#[tokio::test]
async fn terminal_history_does_not_exhaust_a_long_lived_conversation() {
    let store = setup().await;
    let mut first = None;
    for index in 0..140 {
        let mut selected = update();
        selected.client_request_id = format!("select-{index}");
        selected.expected_revision = index * 3;
        if let FileScopeMutation::Select { proposal } = &mut selected.mutation {
            proposal.request_id = selected.client_request_id.clone();
        }
        let receipt = store.update_file_scope(&selected, now()).await.unwrap();
        if index == 0 {
            first = Some((selected.clone(), receipt.clone()));
        }
        let revoke = FileScopeUpdate {
            client_request_id: format!("revoke-{index}"),
            expected_revision: receipt.scope_revision,
            mutation: FileScopeMutation::Revoke {
                directory_request_id: selected.client_request_id.clone(),
            },
            ..selected
        };
        store.update_file_scope(&revoke, now()).await.unwrap();
    }
    assert!(state(&store).await.file_scope.records().is_empty());
    let (request, receipt) = first.unwrap();
    assert_eq!(
        store.update_file_scope(&request, now()).await.unwrap(),
        receipt
    );
    assert!(state(&store).await.file_scope.records().is_empty());
}

#[tokio::test]
async fn directory_review_observes_owner_decision_once_and_rejects_changed_turn() {
    for approve in [true, false] {
        let store = setup().await;
        let now = chrono::Utc::now();
        store.update_file_scope(&update(), now).await.unwrap();
        let mut held = state(&store).await;
        held.turn_state = desk_diagnose_core::session::TurnState::Running;
        let deadline = now + chrono::Duration::minutes(5);
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(held.encode_json_for_storage().unwrap()),
            )
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Some(deadline)),
            )
            .exec(&store.db)
            .await
            .unwrap();
        let FileScopeMutation::Select { mut proposal } = update().mutation else {
            panic!()
        };
        proposal.request_id = "waiting-directory".into();
        proposal.source = DirectoryConsentSource::ModelProposal;
        store
            .propose_file_scope_for_turn(&mut held, proposal, now)
            .await
            .unwrap();
        let before = held.clone();
        for change in ["input", "lease", "authority", "transcript"] {
            let mut changed = held.clone();
            match change {
                "input" => changed.input_revision += 1,
                "lease" => changed.lease_token += 1,
                "authority" => changed.policy_revision += 1,
                _ => changed
                    .conversation
                    .push(desk_diagnose_core::chat::ChatMessage::text(
                        "new",
                        desk_diagnose_core::chat::ChatRole::User,
                        "new requirement",
                    )),
            }
            assert!(
                desk_diagnose_core::directory_tools::adopt_review_snapshot(
                    &mut held,
                    changed,
                    "waiting-directory"
                )
                .is_err()
            );
            assert_eq!(held, before);
        }

        assert_eq!(
            store
                .poll_file_scope_review(&mut held, "waiting-directory")
                .await
                .unwrap(),
            None
        );
        let decision = FileScopeUpdate {
            client_request_id: "owner-decision".into(),
            expected_revision: held.file_scope.revision(),
            mutation: FileScopeMutation::Decide {
                directory_request_id: "waiting-directory".into(),
                approve,
            },
            ..update()
        };
        store.update_file_scope(&decision, now).await.unwrap();
        assert_eq!(
            store
                .poll_file_scope_review(&mut held, "waiting-directory")
                .await
                .unwrap(),
            Some(approve)
        );
        assert_eq!(held.version, before.version + 1);
        assert_eq!(held.scope_snapshot, before.scope_snapshot);
        let settled = held.clone();
        // HTTP/signaling retries replay the receipt; there is no second turn.
        store.update_file_scope(&decision, now).await.unwrap();
        assert_eq!(state(&store).await, settled);
        let mut stopped = held.clone();
        stopped.turn_state = desk_diagnose_core::session::TurnState::Idle;
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(stopped.encode_json_for_storage().unwrap()),
            )
            .exec(&store.db)
            .await
            .unwrap();
        assert!(
            store
                .poll_file_scope_review(&mut held, "waiting-directory")
                .await
                .is_err()
        );
        assert_eq!(held, settled);
    }
}
