use super::*;
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use sea_orm::{ConnectionTrait, Database, Schema};

mod wire;

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "model".into(),
        connection_revision: 1,
        model_id: "test".into(),
        profile_revision: 1,
    }
}

fn params() -> UpdateObjectContext {
    let update = DeviceAssistantObjectContextUpdate {
        conversation_id: "client-conversation".into(),
        client_request_id: "attach".into(),
        operation: DeviceAssistantObjectContextOperation::AttachFile {
            object_ref: ObjectRef {
                token: "opaque-file".into(),
                snapshot_id: "snapshot".into(),
                object_kind: ObjectKind::File,
                expires_at: (Utc::now() + Duration::minutes(3)).to_rfc3339(),
            },
            display_summary: "selected file".into(),
        },
    };
    UpdateObjectContext {
        run_id: desk_diagnose_core::conversation_key::derive_conversation_key(
            "7",
            "device",
            Some(&update.conversation_id),
            "unused",
        ),
        actor_id: "7".into(),
        device_id: "device".into(),
        update,
        destination: Some(destination()),
        created_at: Utc::now().to_rfc3339(),
    }
}

fn scoped(db: DatabaseConnection) -> SignalAgentSessionStore {
    SignalAgentSessionStore::new(db).with_client_metadata(
        Some("client-conversation".into()),
        AgentSessionSurface::DeviceAssistant,
    )
}

async fn setup(db: DatabaseConnection) -> SignalAgentSessionStore {
    let schema = Schema::new(db.get_database_backend());
    for table in [
        schema.create_table_from_entity(agent_session::Entity),
        schema.create_table_from_entity(agent_run_event::Entity),
    ] {
        db.execute(&table).await.unwrap();
    }
    scoped(db)
}

async fn memory() -> SignalAgentSessionStore {
    setup(Database::connect("sqlite::memory:").await.unwrap()).await
}

async fn row(store: &SignalAgentSessionStore) -> agent_session::Model {
    agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
}
async fn state(store: &SignalAgentSessionStore) -> PersistedAgentSession {
    PersistedAgentSession::decode_json(&row(store).await.state_json).unwrap()
}
async fn events(store: &SignalAgentSessionStore) -> Vec<agent_run_event::Model> {
    agent_run_event::Entity::find()
        .order_by_asc(agent_run_event::Column::Id)
        .all(&store.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn first_object_receipts_survive_detach_expiry_and_missing_model_without_renewal() {
    let store = memory().await;
    let mut original = params();
    assert!(store.update_object_context(&original).await.unwrap());
    let first = row(&store).await;
    let attachment = state(&store).await.context_attachments.remove(0);
    assert!(store.update_object_context(&original).await.unwrap());
    assert_eq!(row(&store).await, first);
    let detach = UpdateObjectContext {
        run_id: original.run_id.clone(),
        actor_id: "7".into(),
        device_id: "device".into(),
        update: DeviceAssistantObjectContextUpdate {
            conversation_id: original.update.conversation_id.clone(),
            client_request_id: "detach".into(),
            operation: DeviceAssistantObjectContextOperation::Detach {
                attachment_id: attachment.attachment_id.clone(),
            },
        },
        destination: None,
        created_at: original.created_at.clone(),
    };
    assert!(store.update_object_context(&detach).await.unwrap());
    let detached = row(&store).await;
    assert!(store.update_object_context(&detach).await.unwrap());
    original.created_at = (Utc::now() + Duration::minutes(5)).to_rfc3339();
    original.destination = None;
    assert_eq!(
        store.replay_object_context(&original).await.unwrap(),
        Some(true)
    );
    assert!(store.update_object_context(&original).await.unwrap());
    // No model tables exist: this proves the actual orchestrator probe does not
    // resolve current configuration for a historical operation or for detach.
    assert!(
        crate::device_assistant_orchestrator::apply_object_context_update(
            store.db.clone(),
            7,
            "device".into(),
            &original.update
        )
        .await
        .unwrap()
    );
    assert!(
        crate::device_assistant_orchestrator::apply_object_context_update(
            store.db.clone(),
            7,
            "device".into(),
            &detach.update
        )
        .await
        .unwrap()
    );
    assert_eq!(row(&store).await, detached);
    assert_eq!(events(&store).await.len(), 2);
    let session = state(&store).await;
    assert_eq!(
        (
            session.input_revision,
            session.latest_input_seq,
            session.handled_input_seq
        ),
        (0, 0, 0)
    );
    assert!(session.scope_snapshot.granted.is_empty());
    assert_eq!(
        session.context_attachments[0].expires_at_unix_ms,
        attachment.expires_at_unix_ms
    );
    assert!(!session.context_attachments[0].is_active_at(Utc::now().timestamp_millis() as u64));
}

#[tokio::test]
async fn fresh_duplicate_object_has_a_durable_false_receipt_and_keeps_capability_context_separate()
{
    let store = memory().await;
    let mut original = params();
    assert!(store.update_object_context(&original).await.unwrap());
    let attachment = state(&store).await.context_attachments[0].clone();
    original.update.client_request_id = "same-file-new-operation".into();
    assert!(!store.update_object_context(&original).await.unwrap());
    let first_false = row(&store).await;
    assert_eq!(
        store.replay_object_context(&original).await.unwrap(),
        Some(false)
    );
    assert!(!store.update_object_context(&original).await.unwrap());
    assert_eq!(row(&store).await, first_false);
    let select = store.clone().with_context_selection(ContextSelectionClaim {
        selected_capability_ids: vec![],
        runtime_bindings: vec![],
        candidates: vec![],
        now_unix_ms: Utc::now().timestamp_millis() as u64,
    });
    assert!(
        !select
            .update_context_selection(
                &original.run_id,
                "7",
                "device",
                state(&store).await.scope_snapshot,
                &original.created_at
            )
            .await
            .unwrap()
    );
    assert_eq!(state(&store).await.context_attachments, [attachment]);
    assert_eq!(events(&store).await.len(), 2);
}

#[tokio::test]
async fn conflicting_object_request_identity_or_subject_cannot_replay_or_mutate() {
    let store = memory().await;
    let original = params();
    assert!(store.update_object_context(&original).await.unwrap());
    let saved = row(&store).await;
    for mutation in 0..6 {
        let mut changed = params();
        changed.update = original.update.clone();
        match mutation {
            0 => changed.actor_id = "8".into(),
            1 => changed.device_id = "other-device".into(),
            2 => changed.update.conversation_id = "other-client".into(),
            3 => {
                if let DeviceAssistantObjectContextOperation::AttachFile {
                    display_summary, ..
                } = &mut changed.update.operation
                {
                    *display_summary = "different request".into();
                }
            }
            4 => {
                if let DeviceAssistantObjectContextOperation::AttachFile { object_ref, .. } =
                    &mut changed.update.operation
                {
                    object_ref.token = "different-file".into();
                }
            }
            _ => {
                changed.update.operation = DeviceAssistantObjectContextOperation::Detach {
                    attachment_id: "different".into(),
                }
            }
        }
        assert!(
            store.replay_object_context(&changed).await.is_err(),
            "mutation={mutation}"
        );
        assert!(
            store.update_object_context(&changed).await.is_err(),
            "mutation={mutation}"
        );
        assert_eq!(row(&store).await, saved);
    }
}

#[tokio::test]
async fn directory_refresh_and_terminal_receipts_preserve_original_sources() {
    let store = memory().await;
    let mut first = params();
    let DeviceAssistantObjectContextOperation::AttachFile { object_ref, .. } =
        &mut first.update.operation
    else {
        unreachable!()
    };
    object_ref.object_kind = ObjectKind::Directory;
    let directory_ref = object_ref.clone();
    assert!(store.update_object_context(&first).await.unwrap());
    let directory = state(&store).await.context_attachments[0].clone();
    let mut refresh = params();
    refresh.update.client_request_id = "refresh".into();
    refresh.update.operation = DeviceAssistantObjectContextOperation::RefreshFile {
        stale_attachment_id: directory.attachment_id.clone(),
        object_ref: ObjectRef {
            token: "new-directory-ref".into(),
            ..directory_ref.clone()
        },
        display_summary: "refreshed directory".into(),
    };
    assert!(store.update_object_context(&refresh).await.unwrap());
    let mut terminal = params();
    terminal.update.client_request_id = "terminal".into();
    terminal.update.operation = DeviceAssistantObjectContextOperation::AttachTerminalOutput {
        object_ref: ObjectRef {
            token: "terminal-ref".into(),
            object_kind: ObjectKind::TerminalOutput,
            ..directory_ref
        },
        display_summary: "selected terminal output".into(),
    };
    assert!(store.update_object_context(&terminal).await.unwrap());
    let saved = row(&store).await;
    for mut request in [first, refresh, terminal] {
        request.created_at = (Utc::now() + Duration::minutes(5)).to_rfc3339();
        request.destination = None;
        assert!(store.update_object_context(&request).await.unwrap());
        assert_eq!(row(&store).await, saved);
    }
    let session = state(&store).await;
    assert_eq!(session.context_attachments.len(), 3);
    assert!(!session.context_attachments[0].is_active_at(Utc::now().timestamp_millis() as u64));
    assert_eq!(session.context_attachments[0].envelope, directory.envelope);
    assert_eq!(events(&store).await.len(), 3);
}

#[tokio::test]
async fn expired_new_selection_and_missing_refresh_target_never_create_state_or_receipts() {
    let store = memory().await;
    let mut request = params();
    request.created_at = (Utc::now() + Duration::minutes(5)).to_rfc3339();
    assert!(store.update_object_context(&request).await.is_err());
    assert!(
        agent_session::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(events(&store).await.is_empty());
    let mut request = params();
    let DeviceAssistantObjectContextOperation::AttachFile {
        object_ref,
        display_summary,
    } = request.update.operation
    else {
        unreachable!()
    };
    request.update.operation = DeviceAssistantObjectContextOperation::RefreshFile {
        stale_attachment_id: "missing".into(),
        object_ref,
        display_summary,
    };
    assert!(store.update_object_context(&request).await.is_err());
    assert!(
        agent_session::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(events(&store).await.is_empty());
}

#[tokio::test]
async fn active_turn_replays_history_but_refuses_new_object_mutations_even_after_lease_expiry() {
    let store = memory().await;
    let mut original = params();
    assert!(store.update_object_context(&original).await.unwrap());
    let mut session = state(&store).await;
    session.turn_state = TurnState::Running;
    session.lease_token = 1;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .col_expr(agent_session::Column::LeaseToken, Expr::value(1_i64))
        .col_expr(
            agent_session::Column::LeaseDeadline,
            Expr::value(Utc::now() - Duration::seconds(1)),
        )
        .exec(&store.db)
        .await
        .unwrap();
    let saved = row(&store).await;
    assert!(store.update_object_context(&original).await.unwrap());
    original.update.client_request_id = "new".into();
    assert!(
        store
            .update_object_context(&original)
            .await
            .unwrap_err()
            .retryable
    );
    assert_eq!(row(&store).await, saved);
    assert_eq!(events(&store).await.len(), 1);
}

#[tokio::test]
async fn receipt_insert_failure_rolls_back_existing_and_new_object_sessions() {
    for existing in [false, true] {
        let store = memory().await;
        let mut original = params();
        if existing {
            store.update_object_context(&original).await.unwrap();
            original.update.client_request_id = "next".into();
        }
        let before = agent_session::Entity::find().all(&store.db).await.unwrap();
        let before_events = events(&store).await;
        store.db.execute_unprepared("CREATE TRIGGER reject_object_receipt BEFORE INSERT ON agent_run_event BEGIN SELECT RAISE(ABORT, 'synthetic receipt failure'); END").await.unwrap();
        assert!(store.update_object_context(&original).await.is_err());
        assert_eq!(
            agent_session::Entity::find().all(&store.db).await.unwrap(),
            before
        );
        assert_eq!(events(&store).await, before_events);
    }
}

#[tokio::test]
async fn corrupted_receipt_columns_and_payload_fail_closed() {
    for mutation in 0..6 {
        let store = memory().await;
        let original = params();
        store.update_object_context(&original).await.unwrap();
        let event = events(&store).await.remove(0);
        let (column, value) = match mutation {
            0 => (agent_run_event::Column::ActorId, "other".into()),
            1 => (agent_run_event::Column::Kind, "user_followup".into()),
            2 => (agent_run_event::Column::RunId, "other".into()),
            3 => (
                agent_run_event::Column::SourceEnvelopeIdsJson,
                "[\"forged\"]".into(),
            ),
            4 => (agent_run_event::Column::CorrelationId, "other".into()),
            _ => {
                let mut payload: serde_json::Value =
                    serde_json::from_str(&event.payload_json).unwrap();
                payload["device_id"] = "other".into();
                (agent_run_event::Column::PayloadJson, payload.to_string())
            }
        };
        agent_run_event::Entity::update_many()
            .col_expr(column, Expr::value(value))
            .exec(&store.db)
            .await
            .unwrap();
        assert!(
            store.replay_object_context(&original).await.is_err(),
            "mutation={mutation}"
        );
        assert!(
            store.update_object_context(&original).await.is_err(),
            "mutation={mutation}"
        );
    }
}

#[tokio::test]
async fn retained_metadata_without_original_receipt_cannot_fabricate_a_first_result() {
    let store = memory().await;
    let mut original = params();
    assert!(store.update_object_context(&original).await.unwrap());
    agent_run_event::Entity::delete_many()
        .exec(&store.db)
        .await
        .unwrap();
    let saved = row(&store).await;
    // This is also the shape of metadata persisted without receipt support.
    assert!(store.replay_object_context(&original).await.is_err());
    assert!(store.update_object_context(&original).await.is_err());
    assert_eq!(row(&store).await, saved);
    assert!(events(&store).await.is_empty());
    original.update.client_request_id = "new-explicit-request".into();
    assert!(!store.update_object_context(&original).await.unwrap());
    assert_eq!(
        store.replay_object_context(&original).await.unwrap(),
        Some(false)
    );
    assert_eq!(events(&store).await.len(), 1);
}

#[tokio::test]
async fn object_receipts_survive_sqlite_reopen_and_second_connection_pool() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("objects.db").display()
    );
    let a = setup(Database::connect(&url).await.unwrap()).await;
    let original = params();
    assert!(a.update_object_context(&original).await.unwrap());
    let first = row(&a).await;
    a.db.close().await.unwrap();
    let b = scoped(Database::connect(&url).await.unwrap());
    let c = scoped(Database::connect(&url).await.unwrap());
    let (left, right) = tokio::join!(
        b.update_object_context(&original),
        c.update_object_context(&original)
    );
    assert!(left.unwrap());
    assert!(right.unwrap());
    assert_eq!(row(&b).await, first);
    assert_eq!(events(&c).await.len(), 1);
    b.db.close().await.unwrap();
    c.db.close().await.unwrap();
}

#[tokio::test]
async fn simultaneous_first_operations_converge_without_losing_or_duplicating_receipts() {
    for same_request in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("race.db").display());
        let a = setup(Database::connect(&url).await.unwrap()).await;
        let b = scoped(Database::connect(&url).await.unwrap());
        let first = params();
        let mut second = params();
        second.update = first.update.clone();
        if !same_request {
            second.update.client_request_id = "second-operation".into();
        }
        let (left, right) = tokio::join!(
            a.update_object_context(&first),
            b.update_object_context(&second)
        );
        // A SQLite writer conflict may require a client retry, but it must not
        // become a permanent error or create a second receipt for one request.
        let mut results = vec![];
        for (store, request, result) in [(&a, &first, left), (&b, &second, right)] {
            results.push(match result {
                Ok(changed) => changed,
                Err(error) => {
                    assert!(error.retryable, "{error:?}");
                    store.update_object_context(request).await.unwrap()
                }
            });
        }
        assert_eq!(
            results.iter().filter(|changed| **changed).count(),
            if same_request { 2 } else { 1 }
        );
        let saved = row(&a).await;
        assert_eq!(state(&a).await.context_attachments.len(), 1);
        assert_eq!(events(&a).await.len(), if same_request { 1 } else { 2 });
        assert_eq!(a.update_object_context(&first).await.unwrap(), results[0]);
        assert_eq!(b.update_object_context(&second).await.unwrap(), results[1]);
        assert_eq!(row(&a).await, saved);
        a.db.close().await.unwrap();
        b.db.close().await.unwrap();
    }
}
