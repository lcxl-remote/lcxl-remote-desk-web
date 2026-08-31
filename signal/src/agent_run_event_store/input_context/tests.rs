use super::*;
use crate::agent_session_store::{SignalAgentSessionStore, UpdateObjectContext};
use desk_agent_protocol::{
    Capability, ExecutionMode,
    computer_use::{ObjectKind, ObjectRef},
    device_assistant::{DeviceAssistantObjectContextOperation, DeviceAssistantObjectContextUpdate},
};
use desk_diagnose_core::{
    chat::ToolCall,
    input_read_context::object_read::ObjectReadBinding,
    model_message_labels::model_bound_user_message,
    seam::{ClaimError, ClaimTurnParams, SessionSeam},
};
use sea_orm::{ConnectionTrait, Database, sea_query::Expr};

mod grants;
mod live_targets;
mod wire;

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "synthetic".into(),
        profile_revision: 1,
    }
}

fn subject() -> InputSubject<'static> {
    InputSubject {
        run_id: "run",
        actor_id: "7",
        device_id: "device",
        client_conversation_id: Some("client-run"),
    }
}

async fn setup(url: &str) -> SignalAgentRunEventStore {
    let db = Database::connect(url).await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    SignalAgentRunEventStore::new(db)
}

fn sessions(store: &SignalAgentRunEventStore) -> SignalAgentSessionStore {
    SignalAgentSessionStore::new(store.db.clone()).with_client_metadata(
        Some("client-run".into()),
        AgentSessionSurface::DeviceAssistant,
    )
}

async fn session_row(store: &SignalAgentRunEventStore) -> agent_session::Model {
    agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
}

async fn state(store: &SignalAgentRunEventStore) -> PersistedAgentSession {
    PersistedAgentSession::decode_json(&session_row(store).await.state_json).unwrap()
}

async fn attach(store: &SignalAgentRunEventStore, id: &str, kind: ObjectKind) -> ContextAttachment {
    let reference = ObjectRef {
        token: format!("ref-{id}"),
        snapshot_id: "worker".into(),
        object_kind: kind,
        expires_at: (Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
    };
    let operation = if kind == ObjectKind::TerminalOutput {
        DeviceAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref: reference,
            display_summary: "selected output".into(),
        }
    } else {
        DeviceAssistantObjectContextOperation::AttachFile {
            object_ref: reference,
            display_summary: "arbitrary metadata, not a file type".into(),
        }
    };
    sessions(store)
        .update_object_context(&UpdateObjectContext {
            run_id: "run".into(),
            actor_id: "7".into(),
            device_id: "device".into(),
            update: DeviceAssistantObjectContextUpdate {
                conversation_id: "client-run".into(),
                client_request_id: id.into(),
                operation,
            },
            destination: Some(destination()),
            created_at: Utc::now().to_rfc3339(),
        })
        .await
        .unwrap();
    state(store)
        .await
        .context_attachments
        .into_iter()
        .find(|object| object.client_request_id == id)
        .unwrap()
}

async fn detach(store: &SignalAgentRunEventStore, object: &ContextAttachment) {
    sessions(store)
        .update_object_context(&UpdateObjectContext {
            run_id: "run".into(),
            actor_id: "7".into(),
            device_id: "device".into(),
            update: DeviceAssistantObjectContextUpdate {
                conversation_id: "client-run".into(),
                client_request_id: format!("detach-{}", object.attachment_id),
                operation: DeviceAssistantObjectContextOperation::Detach {
                    attachment_id: object.attachment_id.clone(),
                },
            },
            destination: None,
            created_at: Utc::now().to_rfc3339(),
        })
        .await
        .unwrap();
}

fn input(id: &str, mut objects: Vec<ContextAttachment>) -> AppendUserFollowupParams {
    objects.sort_by(|a, b| a.attachment_id.cmp(&b.attachment_id));
    AppendUserFollowupParams {
        event_id: id.into(),
        run_id: "run".into(),
        actor_id: "7".into(),
        device_id: "device".into(),
        client_conversation_id: Some("client-run".into()),
        surface: AgentSessionSurface::DeviceAssistant,
        policy_revision: desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        current_scope: AgentScope {
            granted: vec![Capability::FileContentRead],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        read_context: Some(ReadContextSelection {
            tool_names: vec!["read_selected_text_file".into()],
            expires_at: None,
            object_attachments: objects,
            live_targets: Vec::new(),
        }),
        message: model_bound_user_message(
            id.into(),
            "read only the chosen object".into(),
            destination(),
        )
        .unwrap(),
        created_at: Utc::now().to_rfc3339(),
    }
}

async fn validate(
    store: &SignalAgentRunEventStore,
    params: &AppendUserFollowupParams,
    revision: u64,
) -> Result<(), AgentError> {
    store
        .validate_object_read(
            subject(),
            revision,
            params.read_context.as_ref().unwrap(),
            &destination(),
            Utc::now().timestamp_millis() as u64,
        )
        .await
}

#[tokio::test]
async fn original_input_survives_reopen_and_later_unselected_context_without_renewal() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("input.db").display()
    );
    let store = setup(&url).await;
    let chosen = attach(&store, "first", ObjectKind::File).await;
    let params = input("message", vec![chosen.clone()]);
    let first = store.append_user_followup(params.clone()).await.unwrap();
    let saved = session_row(&store).await;
    let retry = store.append_user_followup(params.clone()).await.unwrap();
    assert!(!retry.newly_appended);
    assert_eq!(retry.input_revision, first.input_revision);
    assert_eq!(session_row(&store).await, saved);
    let later = attach(&store, "later", ObjectKind::File).await;
    assert_eq!(
        store
            .original_read_context(subject(), first.input_revision)
            .await
            .unwrap(),
        params.read_context
    );
    validate(&store, &params, first.input_revision)
        .await
        .unwrap();
    assert!(
        store
            .select_objects(
                subject(),
                "message",
                &[chosen.attachment_id.clone(), later.attachment_id],
                Utc::now().timestamp_millis() as u64
            )
            .await
            .is_err()
    );
    assert!(
        store
            .select_objects(
                subject(),
                "message",
                &[],
                Utc::now().timestamp_millis() as u64
            )
            .await
            .is_err()
    );
    detach(&store, &chosen).await;
    assert!(
        validate(&store, &params, first.input_revision)
            .await
            .is_err()
    );
    let detached = session_row(&store).await;
    let mut expired_retry = params.clone();
    expired_retry.created_at = (Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    assert!(
        !store
            .append_user_followup(expired_retry)
            .await
            .unwrap()
            .newly_appended
    );
    assert_eq!(session_row(&store).await, detached);
    store.db.close().await.unwrap();
    let reopened = setup(&url).await;
    assert_eq!(
        reopened
            .original_read_context(subject(), first.input_revision)
            .await
            .unwrap(),
        params.read_context
    );
    assert!(
        !reopened
            .append_user_followup(params)
            .await
            .unwrap()
            .newly_appended
    );
    reopened.db.close().await.unwrap();
}

#[tokio::test]
async fn replay_checks_full_message_subject_and_selection_without_any_writes() {
    let store = setup("sqlite::memory:").await;
    let object = attach(&store, "original", ObjectKind::File).await;
    let params = input("message", vec![object]);
    store.append_user_followup(params.clone()).await.unwrap();
    let saved = session_row(&store).await;
    for mutation in 0..9 {
        let mut retry = params.clone();
        match mutation {
            0 => retry.actor_id = "8".into(),
            1 => retry.device_id = "other".into(),
            2 => retry.client_conversation_id = Some("other".into()),
            3 => {
                retry.message = model_bound_user_message(
                    "message".into(),
                    "different requirement".into(),
                    destination(),
                )
                .unwrap()
            }
            4 => retry.message.message_id = "different-message".into(),
            5 => retry.read_context = None,
            6 => retry
                .read_context
                .as_mut()
                .unwrap()
                .object_attachments
                .clear(),
            7 => {
                retry.read_context.as_mut().unwrap().object_attachments[0].display_summary =
                    "different label".into()
            }
            _ => {
                retry.read_context.as_mut().unwrap().expires_at =
                    Some((Utc::now() + chrono::Duration::minutes(1)).to_rfc3339())
            }
        }
        assert!(
            store.append_user_followup(retry).await.is_err(),
            "mutation={mutation}"
        );
        assert_eq!(session_row(&store).await, saved);
    }
}

#[tokio::test]
async fn withdrawal_between_selection_and_append_and_event_failure_roll_back() {
    let store = setup("sqlite::memory:").await;
    let object = attach(&store, "first", ObjectKind::File).await;
    let params = input("message", vec![object.clone()]);
    detach(&store, &object).await;
    let saved = session_row(&store).await;
    assert!(store.append_user_followup(params).await.is_err());
    assert_eq!(session_row(&store).await, saved);
    store.db.execute_unprepared("CREATE TRIGGER reject_input BEFORE INSERT ON agent_run_event WHEN NEW.kind = 'user_followup' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").await.unwrap();
    assert!(
        store
            .append_user_followup(input("new", vec![]))
            .await
            .is_err()
    );
    assert_eq!(session_row(&store).await, saved);
}

#[tokio::test]
async fn stale_prepared_turn_and_changed_input_cannot_claim_or_read_the_new_requirement() {
    let store = setup("sqlite::memory:").await;
    let object = attach(&store, "first", ObjectKind::File).await;
    let first = input("message", vec![object]);
    let receipt = store.append_user_followup(first.clone()).await.unwrap();
    let next = store
        .append_user_followup(input("followup", vec![]))
        .await
        .unwrap();
    let saved = session_row(&store).await;
    assert!(
        store
            .original_read_context(subject(), receipt.input_revision)
            .await
            .is_err()
    );
    assert!(
        validate(&store, &first, receipt.input_revision)
            .await
            .is_err()
    );
    let claim = ClaimTurnParams {
        conversation_id: "run".into(),
        actor_id: "7".into(),
        device_id: "device".into(),
        policy_revision: first.policy_revision,
        current_pdp_scope: first.current_scope,
        turn_id: "turn".into(),
        request_id: Some("transport".into()),
        connection_id: Some("browser".into()),
        trigger_origin: desk_diagnose_core::session::TriggerOrigin::User,
        now: Utc::now().to_rfc3339(),
    };
    assert!(matches!(
        sessions(&store)
            .with_expected_input_revision(receipt.input_revision)
            .claim_turn(claim.clone())
            .await,
        Err(ClaimError::Backend(_))
    ));
    assert_eq!(session_row(&store).await, saved);
    assert!(
        sessions(&store)
            .with_expected_input_revision(next.input_revision)
            .claim_turn(claim)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn original_context_schema_and_columns_cannot_be_downgraded_or_relabelled() {
    for mutation in 0..5 {
        let store = setup("sqlite::memory:").await;
        let object = attach(&store, "first", ObjectKind::File).await;
        let params = input("message", vec![object]);
        let receipt = store.append_user_followup(params.clone()).await.unwrap();
        let mut update = agent_run_event::Entity::update_many()
            .filter(agent_run_event::Column::EventId.eq("message"));
        update = match mutation {
            0 => update.col_expr(
                agent_run_event::Column::PayloadSchemaVersion,
                Expr::value(1),
            ),
            1 => update.col_expr(agent_run_event::Column::CorrelationId, Expr::value("other")),
            2 => update.col_expr(agent_run_event::Column::ActorId, Expr::value("other")),
            3 => update.col_expr(
                agent_run_event::Column::SourceEnvelopeIdsJson,
                Expr::value("[]"),
            ),
            _ => update.col_expr(agent_run_event::Column::InputSeq, Expr::value(9_i64)),
        };
        update.exec(&store.db).await.unwrap();
        assert!(
            store
                .original_read_context(subject(), receipt.input_revision)
                .await
                .is_err()
        );
        assert!(store.append_user_followup(params).await.is_err());
    }
}

#[tokio::test]
async fn shared_object_binding_clamps_all_file_reads_and_rejects_expired_or_changed_destinations() {
    let store = setup("sqlite::memory:").await;
    let mut object = attach(&store, "first", ObjectKind::File).await;
    object.bounds.max_bytes = 4096;
    object.bounds.max_objects = 1;
    for name in [
        "inspect_selected_file_metadata",
        "read_selected_text_file",
        "inspect_selected_spreadsheets",
        "preview_spreadsheet_merge",
        "inspect_selected_numbers_with_iwork",
        "inspect_selected_pages_with_iwork",
        "inspect_selected_keynote_with_iwork",
    ] {
        let selection = ReadContextSelection {
            tool_names: vec![name.into()],
            expires_at: None,
            object_attachments: vec![object.clone()],
            live_targets: Vec::new(),
        };
        let call = ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments_json: if name == "preview_spreadsheet_merge" {
                r#"{"columns":[{"output_header":"A","source_headers":["A"]}]}"#.into()
            } else {
                "{}".into()
            },
        };
        let (_, mut operation) =
            desk_diagnose_core::read_tools::build_read_operation(&call).unwrap();
        let destination = destination();
        let binding = ObjectReadBinding {
            original: &selection,
            destination: &destination,
            now_unix_ms: Utc::now().timestamp_millis() as u64,
        };
        binding.bind(&call, &mut operation).unwrap();
        let json = serde_json::to_string(&operation).unwrap();
        assert!(json.contains("4096"), "{name}: {json}");
        assert!(json.contains("ref-first"));
        assert!(!json.contains("server_resolved"));
        let mut changed_destination = destination.clone();
        if let DestinationIdentity::Model { connection_id, .. } = &mut changed_destination {
            *connection_id = "another-gateway".into();
        }
        let changed = ObjectReadBinding {
            destination: &changed_destination,
            ..binding
        };
        assert!(changed.bind(&call, &mut operation).is_err());
        let oversized = desk_diagnose_core::seam::ToolRunOutput {
            content: "x".repeat(4097),
            image_data_url: None,
        };
        assert!(
            binding
                .label(&call, &oversized, object.envelope.clone())
                .is_err()
        );
        let image = desk_diagnose_core::seam::ToolRunOutput {
            content: "".into(),
            image_data_url: Some("data:image/png;base64,synthetic".into()),
        };
        assert!(
            binding
                .label(&call, &image, object.envelope.clone())
                .is_err()
        );
        let expired = ObjectReadBinding {
            now_unix_ms: object.expires_at_unix_ms,
            ..binding
        };
        assert!(expired.bind(&call, &mut operation).is_err());
    }
}

#[tokio::test]
async fn terminal_read_binds_only_original_terminal_with_original_byte_limit() {
    let store = setup("sqlite::memory:").await;
    let mut terminal = attach(&store, "terminal", ObjectKind::TerminalOutput).await;
    terminal.bounds.max_bytes = 1024;
    let file = attach(&store, "file", ObjectKind::File).await;
    let mut params = input("message", vec![terminal.clone(), file]);
    params.read_context.as_mut().unwrap().tool_names =
        vec!["inspect_selected_terminal_output".into()];
    let call = ToolCall {
        id: "terminal-read".into(),
        name: "inspect_selected_terminal_output".into(),
        arguments_json: "{}".into(),
    };
    let (_, mut operation) = desk_diagnose_core::read_tools::build_read_operation(&call).unwrap();
    ObjectReadBinding {
        original: params.read_context.as_ref().unwrap(),
        destination: &destination(),
        now_unix_ms: Utc::now().timestamp_millis() as u64,
    }
    .bind(&call, &mut operation)
    .unwrap();
    let desk_agent_protocol::OperationInput::ReadContext(desk_agent_protocol::ReadContextInput {
        kind: desk_agent_protocol::ContextKind::TerminalOutputInspect(read),
    }) = operation
    else {
        panic!("expected terminal read")
    };
    assert_eq!(read.max_bytes, 1024);
    assert_eq!(
        read.roots,
        [serde_json::from_str::<ObjectRef>(&terminal.object_ref.opaque_token).unwrap()]
    );
}

#[tokio::test]
async fn independent_sqlite_pools_preserve_one_original_input_under_competing_retries() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("input.db").display()
    );
    let first = setup(&url).await;
    let object = attach(&first, "object", ObjectKind::File).await;
    let second = SignalAgentRunEventStore::new(Database::connect(&url).await.unwrap());
    let original = input("message", vec![object]);
    let (left, right) = tokio::join!(
        first.append_user_followup(original.clone()),
        second.append_user_followup(original.clone()),
    );
    assert!(left.is_ok() || right.is_ok());
    // SQLITE_BUSY may reject a losing write transaction; the same receipt is
    // replayable after that transaction has ended, without creating new input.
    let left = first.append_user_followup(original.clone()).await.unwrap();
    let right = second.append_user_followup(original.clone()).await.unwrap();
    assert_eq!(left, right);
    assert!(!left.newly_appended);
    let saved = session_row(&first).await;
    let snapshot = state(&first).await;
    assert_eq!(snapshot.input_revision, 1);
    assert_eq!(snapshot.latest_input_seq, 1);
    assert_eq!(snapshot.conversation.len(), 1);
    assert_eq!(
        first
            .user_followups_after("run", 0, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let mut conflict = original.clone();
    conflict
        .read_context
        .as_mut()
        .unwrap()
        .object_attachments
        .clear();
    let (replay, rejected) = tokio::join!(
        first.append_user_followup(original),
        second.append_user_followup(conflict),
    );
    assert!(replay.is_ok());
    assert!(rejected.is_err());
    assert_eq!(session_row(&first).await, saved);
    first.db.close().await.unwrap();
    second.db.close().await.unwrap();
}
