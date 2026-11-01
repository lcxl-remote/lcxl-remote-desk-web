use super::*;
use crate::agent_session_store::{SignalAgentSessionStore, UpdateObjectContext};
use desk_agent_protocol::{
    Capability, ExecutionMode,
    ai_assistant::{AiAssistantObjectContextOperation, AiAssistantObjectContextUpdate},
    computer_use::{ObjectKind, ObjectRef},
};
use desk_diagnose_core::{
    chat::ToolCall,
    input_read_context::object_read::ObjectReadBinding,
    model_message_labels::model_bound_user_message,
    seam::{ClaimError, ClaimTurnParams, SessionSeam},
};
use sea_orm::{ConnectionTrait, sea_query::Expr};

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
        actor_id: "1",
        device_id: "device",
        client_conversation_id: Some("client-run"),
    }
}

async fn setup(url: &str) -> SignalAgentRunEventStore {
    let db = crate::config::test_support::Database::connect(url)
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    SignalAgentRunEventStore::new(db)
}

fn sessions(store: &SignalAgentRunEventStore) -> SignalAgentSessionStore {
    SignalAgentSessionStore::new(store.db.clone())
        .with_client_metadata(Some("client-run".into()), AgentSessionSurface::AiAssistant)
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

async fn select_live_document(store: &SignalAgentRunEventStore) {
    use desk_agent_protocol::computer_use::{
        ComputerUseAdapterKind, ComputerUseAdapterRef, ComputerUseCapabilityReadiness,
        ComputerUseContextReference, ComputerUseReadiness,
    };
    use desk_diagnose_core::{
        capability_availability::CapabilityAvailability,
        live_context::{LiveContextBuild, build_live_context},
    };

    let now = Utc::now();
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let capability = registry
        .capability_for_tool("inspect_live_document")
        .unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let inventory = [CapabilityAvailability {
        provider_id: provider.wire.provider_id.clone(),
        capability_id: capability.wire.capability_id.clone(),
        tool_name: capability.wire.tool_name.clone(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let readiness = ComputerUseReadiness {
        schema_version: 1,
        revision: 1,
        observed_at: now.to_rfc3339(),
        expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
        server_api_version: 1,
        os: "macos".into(),
        interactive_user_home: None,
        interactive_session_incarnation: "worker-1".into(),
        local_ceiling_revision: 1,
        capabilities: vec![ComputerUseCapabilityReadiness {
            capability: Capability::DocumentLiveInspect,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::IworkPages,
                version: desk_diagnose_core::ai_assistant::IWORK_ADAPTER_VERSION.into(),
            },
            supported: true,
            ready: true,
            reason: None,
        }],
        context_references: vec![ComputerUseContextReference {
            capability: Capability::DocumentLiveInspect,
            object_ref: ObjectRef {
                token: "original-document".into(),
                snapshot_id: "snapshot-1".into(),
                object_kind: ObjectKind::Document,
                expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
            },
        }],
    };
    let selected_capability_ids = vec![capability.wire.capability_id.clone()];
    let selection = build_live_context(
        LiveContextBuild {
            registry: &registry,
            inventory: &inventory,
            readiness: Some(&readiness),
            selected_capability_ids: &selected_capability_ids,
            request_id: "select-live-document",
            actor_id: "1",
            device_id: "device",
            destination: &destination(),
            now_unix_ms: now.timestamp_millis() as u64,
        },
        || uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    sessions(store)
        .with_context_selection(selection)
        .update_context_selection(
            "run",
            "1",
            "device",
            "select-live-document",
            &now.to_rfc3339(),
        )
        .await
        .unwrap();
}

async fn attach(store: &SignalAgentRunEventStore, id: &str, kind: ObjectKind) -> ContextAttachment {
    let reference = ObjectRef {
        token: format!("ref-{id}"),
        snapshot_id: "worker".into(),
        object_kind: kind,
        expires_at: (Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
    };
    let operation = if kind == ObjectKind::TerminalOutput {
        AiAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref: reference,
            display_summary: "selected output".into(),
        }
    } else {
        AiAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref: reference,
            display_summary: "arbitrary metadata, not a file type".into(),
        }
    };
    sessions(store)
        .seed_read_context(&UpdateObjectContext {
            run_id: "run".into(),
            actor_id: "1".into(),
            device_id: "device".into(),
            update: AiAssistantObjectContextUpdate {
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
            actor_id: "1".into(),
            device_id: "device".into(),
            update: AiAssistantObjectContextUpdate {
                conversation_id: "client-run".into(),
                client_request_id: format!("detach-{}", object.attachment_id),
                operation: AiAssistantObjectContextOperation::Detach {
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
        client_timezone: "UTC".into(),
        temporary_conversation: None,
        event_id: id.into(),
        run_id: "run".into(),
        actor_id: "1".into(),
        device_id: "device".into(),
        client_conversation_id: Some("client-run".into()),
        surface: AgentSessionSurface::AiAssistant,
        policy_revision: desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        current_scope: AgentScope {
            granted: vec![Capability::FileContentRead],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        read_context: Some(ReadContextSelection {
            tool_names: vec!["read_text_file".into()],
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
    let first = store.append_user_followup(&params).await.unwrap();
    let saved = session_row(&store).await;
    let retry = store.append_user_followup(&params).await.unwrap();
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
            .append_user_followup(&expired_retry)
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
            .append_user_followup(&params)
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
    store.append_user_followup(&params).await.unwrap();
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
            store.append_user_followup(&retry).await.is_err(),
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
    assert!(store.append_user_followup(&params).await.is_err());
    assert_eq!(session_row(&store).await, saved);
    store.db.execute_unprepared("CREATE TRIGGER reject_input BEFORE INSERT ON agent_run_event WHEN NEW.kind = 'user_followup' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").await.unwrap();
    assert!(
        store
            .append_user_followup(&input("new", vec![]))
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
    let receipt = store.append_user_followup(&first).await.unwrap();
    let next = store
        .append_user_followup(&input("followup", vec![]))
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
        actor_id: "1".into(),
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
        let receipt = store.append_user_followup(&params).await.unwrap();
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
        assert!(store.append_user_followup(&params).await.is_err());
    }
}

#[tokio::test]
async fn shared_object_binding_clamps_all_file_reads_and_rejects_expired_or_changed_destinations() {
    let store = setup("sqlite::memory:").await;
    let mut object = attach(&store, "first", ObjectKind::File).await;
    object.bounds.max_bytes = 4096;
    object.bounds.max_objects = 1;
    for name in [
        "inspect_files",
        "read_text_file",
        "inspect_spreadsheets",
        "preview_spreadsheet_merge",
        "inspect_numbers_file",
        "inspect_pages_file",
        "inspect_keynote_file",
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
            format: desk_diagnose_core::seam::ToolOutputFormat::Text,
            content: "x".repeat(4097),
            image_data_url: None,
            document_preview: None,
        };
        assert!(
            binding
                .label(&call, &oversized, object.envelope.clone())
                .is_err()
        );
        let image = desk_diagnose_core::seam::ToolRunOutput {
            format: desk_diagnose_core::seam::ToolOutputFormat::Text,
            content: "".into(),
            image_data_url: Some("data:image/png;base64,synthetic".into()),
            document_preview: None,
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
    params.read_context.as_mut().unwrap().tool_names = vec!["read_terminal_output".into()];
    let call = ToolCall {
        id: "terminal-read".into(),
        name: "read_terminal_output".into(),
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
    let second = SignalAgentRunEventStore::new(
        crate::config::test_support::Database::connect(&url)
            .await
            .unwrap(),
    );
    let original = input("message", vec![object]);
    let (left, right) = tokio::join!(
        first.append_user_followup(&original),
        second.append_user_followup(&original),
    );
    assert!(left.is_ok() || right.is_ok());
    // SQLITE_BUSY may reject a losing write transaction; the same receipt is
    // replayable after that transaction has ended, without creating new input.
    let left = first.append_user_followup(&original).await.unwrap();
    let right = second.append_user_followup(&original).await.unwrap();
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
        first.append_user_followup(&original),
        second.append_user_followup(&conflict),
    );
    assert!(replay.is_ok());
    assert!(rejected.is_err());
    assert_eq!(session_row(&first).await, saved);
    first.db.close().await.unwrap();
    second.db.close().await.unwrap();
}

async fn save_recovery_fixture(
    store: &SignalAgentRunEventStore,
    session: &mut PersistedAgentSession,
) {
    let row = session_row(store).await;
    session.version = row.version + 1;
    let raw = session.encode_json_for_storage().unwrap();
    PersistedAgentSession::decode_json(&raw).unwrap();
    let mut active: agent_session::ActiveModel = row.into();
    active.version = Set(session.version);
    active.state_json = Set(raw);
    active.update(&store.db).await.unwrap();
}

/// The real input ledger recovers the model answer after a committed forget.
/// Native forget authorization is covered by the storage tests.
async fn recovered_control_fixture() -> (
    SignalAgentRunEventStore,
    AppendUserFollowupParams,
    UserFollowupAck,
    desk_agent_protocol::ai_assistant::memory::AssistantDataControlReceipt,
) {
    use desk_agent_protocol::ai_assistant::memory::{
        AssistantDataControlReceipt, DataControlOutcome, DataControlReason,
    };
    use desk_diagnose_core::{
        ai_assistant::{data_control, data_tools},
        native_ids::{self, Kind},
    };
    let store = setup("sqlite::memory:").await;
    let params = input("original-control", vec![]);
    let accepted = store.append_user_followup(&params).await.unwrap();
    assert_eq!(
        store.handled_outcome(&params, &accepted).await.unwrap(),
        None
    );
    let mut session = state(&store).await;
    session
        .begin_turn(
            "original-turn",
            None,
            None,
            session.policy_revision,
            session.scope_snapshot.clone(),
            &params.created_at,
        )
        .unwrap();
    session
        .conversation
        .iter_mut()
        .find(|message| message.message_id == params.message.message_id)
        .unwrap()
        .turn_id = Some("original-turn".into());
    let call = ToolCall { id: "forget-call".into(), name: data_tools::MANAGE_MEMORIES.into(), arguments_json: serde_json::json!({"operation":"forget","memory_id": native_ids::stable(Kind::Memory, &"target"), "expected_revision":1}).to_string() };
    session.conversation.push(
        ChatMessage::assistant_tool_calls(
            "forget-producer",
            "old model wording",
            vec![call.to_ref()],
        )
        .with_turn_id("original-turn"),
    );
    let hash =
        data_control::tool_group_sha256(&session.conversation[session.conversation.len() - 1..])
            .unwrap();
    let data = session.assistant_data.as_ref().unwrap();
    let receipt = AssistantDataControlReceipt {
        operation_id: "recovered-control".into(),
        outcome: DataControlOutcome::Applied,
        reason: DataControlReason::MemoryForget,
        affected_count: 1,
        control_generation: data.control_generation + 1,
        memory_state_generation: data.memory_state_generation + 1,
        cleanup_pending: true,
    };
    data_control::commit_memory_forget_group(
        &mut session,
        "forget-producer",
        &hash,
        &receipt,
        &params.created_at,
    )
    .unwrap();
    // The synthetic committed control image includes its runtime generations.
    use crate::entity::agent_assistant_runtime as runtime;
    runtime::Entity::update_many()
        .set(runtime::ActiveModel {
            control_generation: Set(receipt.control_generation as i64),
            memory_state_generation: Set(receipt.memory_state_generation as i64),
            ..Default::default()
        })
        .filter(runtime::Column::ActorId.eq(&session.actor_id))
        .exec(&store.db)
        .await
        .unwrap();
    save_recovery_fixture(&store, &mut session).await;
    assert_eq!(
        store.handled_outcome(&params, &accepted).await.unwrap(),
        None
    );
    session.conversation.push(
        ChatMessage::text(
            "forget-confirmation",
            ChatRole::Assistant,
            "The memory was deleted.",
        )
        .with_turn_id("original-turn"),
    );
    session.handled_input_seq = session.latest_input_seq;
    session.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        &params.created_at,
    );
    save_recovery_fixture(&store, &mut session).await;
    let later = input("later-owner-input", vec![]);
    store.append_user_followup(&later).await.unwrap();
    let mut session = state(&store).await;
    session
        .begin_turn(
            "later-turn",
            None,
            None,
            session.policy_revision,
            session.scope_snapshot.clone(),
            &later.created_at,
        )
        .unwrap();
    session
        .conversation
        .iter_mut()
        .find(|message| message.message_id == later.message.message_id)
        .unwrap()
        .turn_id = Some("later-turn".into());
    session.conversation.push(
        ChatMessage::text("later-answer", ChatRole::Assistant, "unrelated answer")
            .with_turn_id("later-turn"),
    );
    session.handled_input_seq = session.latest_input_seq;
    session.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        &later.created_at,
    );
    save_recovery_fixture(&store, &mut session).await;
    (store, params, accepted, receipt)
}

#[tokio::test]
async fn handled_outcome_binds_the_original_receipt_message_and_subject_without_writes() {
    use desk_diagnose_core::agent_loop::LoopOutcome;
    let (store, params, accepted, _receipt) = recovered_control_fixture().await;
    let before = session_row(&store).await;
    let events = agent_run_event::Entity::find()
        .all(&store.db)
        .await
        .unwrap();
    assert_eq!(
        store.handled_outcome(&params, &accepted).await.unwrap(),
        Some(LoopOutcome::Answered("The memory was deleted.".into()))
    );
    let later = input("later-owner-input", vec![]);
    let later_ack = store.append_user_followup(&later).await.unwrap();
    assert_eq!(
        store.handled_outcome(&later, &later_ack).await.unwrap(),
        Some(LoopOutcome::Answered("unrelated answer".into()))
    );
    for change in 0..9 {
        let mut retry = params.clone();
        let mut ack = accepted.clone();

        match change {
            0 => retry.actor_id = "other".into(),
            1 => retry.device_id = "other-device".into(),
            2 => retry.client_conversation_id = Some("other-client".into()),
            3 => ack.event_seq += 1,
            4 => ack.input_seq += 1,
            5 => ack.input_revision += 1,
            6 => ack.event_id = "missing".into(),
            7 => retry.message.text.push_str("changed"),
            _ => retry.message.message_id = "later-owner-input".into(),
        }
        assert!(
            store.handled_outcome(&retry, &ack).await.is_err(),
            "change {change}"
        );
    }
    assert_eq!(session_row(&store).await, before);
    assert_eq!(
        agent_run_event::Entity::find()
            .all(&store.db)
            .await
            .unwrap(),
        events
    );
}

#[test]
fn handled_control_recovery_fits_explicit_small_stacks() {
    const STACK: &str = "LRD_TEST_INPUT_OUTCOME_STACK_BYTES";
    if let Some(bytes) = std::env::var_os(STACK) {
        let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
        assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
        actix_web::rt::System::new().block_on(async {
            let (store, params, accepted, _receipt) = recovered_control_fixture().await;
            let (done, ready) = tokio::sync::oneshot::channel();
            let thread = std::thread::Builder::new()
                .name("input-outcome-stack".into())
                .stack_size(bytes)
                .spawn(move || {
                    actix_web::rt::System::new().block_on(async move {
                        let before = session_row(&store).await;
                        let outcome = desk_diagnose_core::future::send_phase(|| {
                            store.handled_outcome(&params, &accepted)
                        })
                        .await
                        .unwrap();
                        assert_eq!(
                            outcome,
                            Some(desk_diagnose_core::agent_loop::LoopOutcome::Answered(
                                "The memory was deleted.".into()
                            ))
                        );
                        assert_eq!(session_row(&store).await, before);
                        let _ = done.send(());
                    });
                })
                .unwrap();
            ready.await.unwrap();
            thread.join().unwrap();
        });
        return;
    }
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "handled_control_recovery_fits_explicit_small_stacks",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(STACK, bytes.to_string())
            .env_remove("RUST_MIN_STACK")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "input outcome recovery failed on {bytes} bytes"
        );
    }
}
