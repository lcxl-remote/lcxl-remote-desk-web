use super::*;
use crate::agent_session_store::SignalAgentSessionStore as Sessions;
use crate::entity::{agent_assistant_history as history, agent_file_recovery_cleanup as removal};
use desk_agent_protocol::ai_assistant::history::{
    ConversationSearchRequest, ConversationSearchResult,
};
use desk_diagnose_core::{
    ai_assistant::{history::serialized_tool_response_bytes, seam::AssistantDataRead},
    prompt::ResponseFormatSpec,
    seam::{ModelRequest, SessionSeam},
    sink_authorizer::SinkProjectionAudit,
};

async fn fixture(label: &str) -> (Frame, AssistantDataRead<ConversationSearchResult>, String) {
    let frame = Frame::with_call_scope(label, true, None, true, false, None, async |_, _| {
        vec![ToolCall {
            id: "history-call".into(),
            name: data_tools::SEARCH_CONVERSATIONS.into(),
            arguments_json: r#"{"queries":["report"]}"#.into(),
        }]
    })
    .await;
    let schema = sea_orm::Schema::new(frame.store.db.get_database_backend());
    for mut table in [
        schema.create_table_from_entity(crate::entity::agent_approval_trace::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_trace_part::Entity),
        schema.create_table_from_entity(crate::entity::agent_subagent_run::Entity),
        schema.create_table_from_entity(crate::entity::agent_schedule::Entity),
    ] {
        frame
            .store
            .db
            .execute(&table.if_not_exists().to_owned())
            .await
            .unwrap();
    }
    let source = super::super::keyword_search::capture_at(
        &frame.store,
        &frame
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .source_device_ref,
        "snapshot-original",
        "snapshot-source",
        "report approved project plan",
        &[],
        frame
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .history_access_scope
            .as_ref()
            .unwrap()
            .accepted_at_unix_ms
            - 1000,
    )
    .await;
    let sessions = sessions(&frame);
    let request: ConversationSearchRequest =
        serde_json::from_str(&frame.calls[0].arguments_json).unwrap();
    let read = SessionSeam::conversation_search(&sessions)
        .unwrap()
        .search_conversations(frame.context(), &request)
        .await
        .unwrap();
    assert!(
        read.result.hits.iter().any(|hit| hit.source_id == source),
        "source={source} result={:?} scope={:?}",
        read.result,
        frame
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .history_access_scope
    );
    (frame, read, source)
}
fn sessions(frame: &Frame) -> Sessions {
    Sessions::new(frame.store.db.clone()).with_client_metadata(
        None,
        desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
    )
}
fn stage(frame: &mut Frame, read: &AssistantDataRead<ConversationSearchResult>) -> ChatMessage {
    let proof = read.projection.as_ref().unwrap();
    let body = serde_json::to_string(&read.result).unwrap();
    let mut message =
        ChatMessage::tool_result("accepted-search-snapshot", &frame.calls[0].id, &body);
    message.turn_id = frame.session.current_turn_id.clone();
    message.tool_ok = Some(true);
    message.data_envelope = Some(proof.snapshot_envelope().unwrap());
    frame
        .session
        .assistant_data
        .as_mut()
        .unwrap()
        .retrieval_budget
        .settle(
            &frame.logical[0],
            serialized_tool_response_bytes(&frame.calls[0].id, &body, None).unwrap(),
        )
        .unwrap();
    frame.session.conversation.push(message.clone());
    message
}
async fn remove_original(frame: &Frame) {
    removal::ActiveModel {
        conversation_id: Set("snapshot-original".into()),
        actor_id: Set(frame.session.actor_id.clone()),
        device_id: Set(frame.session.device_id.clone()),
        created_at_unix_ms: Set(frame.now as i64),
        history_deleted_at_unix_ms: Set(None),
        next_attempt_at_unix_ms: Set(frame.now as i64),
        attempts: Set(0),
        completed_at_unix_ms: Set(Some(frame.now as i64)),
        ..Default::default()
    }
    .insert(&frame.store.db)
    .await
    .unwrap();
    assert!(
        sessions(frame)
            .delete_for_subject(
                "snapshot-original",
                &frame.session.actor_id,
                &frame.session.device_id
            )
            .await
            .unwrap()
            .is_none()
    );
}
async fn request(frame: &mut Frame) -> ModelRequest {
    let host = sessions(frame);
    let access = SessionSeam::assistant_data_access(&host, &frame.session, frame.now)
        .await
        .unwrap()
        .unwrap();
    let data = frame.session.assistant_data.as_mut().unwrap();
    data.control_generation = access.control_generation;
    data.memory_state_generation = access.memory_state_generation;
    data.current_memories.clear();
    data.read_uses.clear();
    data.history_dependencies.clear();
    SessionSeam::save(&host, &mut frame.session).await.unwrap();
    let mut request = ModelRequest::text_only(
        frame.session.model_conversation().into_owned(),
        ResponseFormatSpec::None,
    );
    request.context_conversation_id = Some(frame.session.conversation_id.clone());
    request.assistant_data =
        SessionSeam::assistant_data_request_manifest(&host, &frame.session, &request, frame.now)
            .await
            .unwrap()
            .map(Box::new);
    assert!(
        request
            .assistant_data
            .as_ref()
            .unwrap()
            .read_projection_ids
            .is_empty()
    );
    request
}

#[tokio::test]
async fn accepted_snapshot_and_inflight_answer_survive_source_deletion_and_cache_cleanup() {
    let (frame, read, source) = fixture("owned-snapshot-lifetime").await;
    exercise_accepted(frame, read, source).await;
}

async fn exercise_accepted(
    mut frame: Frame,
    read: AssistantDataRead<ConversationSearchResult>,
    source: String,
) {
    let saved = stage(&mut frame, &read);
    SessionSeam::save(&sessions(&frame), &mut frame.session)
        .await
        .unwrap();
    let copies = history::Entity::find()
        .filter(history::Column::ConversationId.eq(&frame.session.conversation_id))
        .filter(history::Column::NativeId.eq(&saved.message_id))
        .all(&frame.store.db)
        .await
        .unwrap();
    assert!(!copies.is_empty());
    assert!(copies.iter().all(|row| row.dependencies_json == "[]"));
    let before = frame.session.clone();
    SessionSeam::save(&sessions(&frame), &mut frame.session)
        .await
        .unwrap();
    assert_eq!(
        history::Entity::find()
            .filter(history::Column::ConversationId.eq(&frame.session.conversation_id))
            .filter(history::Column::NativeId.eq(&saved.message_id))
            .all(&frame.store.db)
            .await
            .unwrap(),
        copies
    );
    assert_eq!(
        frame
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget,
        before.assistant_data.as_ref().unwrap().retrieval_budget
    );
    let model_request = request(&mut frame).await;
    let selected: Vec<_> = model_request
        .messages
        .iter()
        .filter_map(|m| m.data_envelope.clone())
        .collect();
    let sink = SinkProjectionAudit {
        destination: frame.destination.clone(),
        envelope_ids: selected.iter().map(|e| e.envelope_id.clone()).collect(),
        digests_sha256: selected.iter().map(|e| e.digest_sha256.clone()).collect(),
        total_bytes: selected
            .iter()
            .map(|e| match e.content {
                desk_agent_protocol::data_lineage::ContentRef::ImmutableBlob {
                    size_bytes, ..
                } => size_bytes as usize,
                _ => panic!("immutable"),
            })
            .sum(),
    };
    let answer_text = format!("The project plan was approved.〔来源:{source}〕");
    let output = ModelEgressPolicy {
        destination: frame.destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: "snapshot-export".into(),
        now_unix_ms: frame.now,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    }
    .derive_model_output_envelope(
        &ModelTurn {
            text: answer_text.clone(),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        },
        &selected,
    )
    .unwrap();
    let txn = frame.store.db.begin().await.unwrap();
    let prepared = super::super::super::model_gate::prepare_on(&txn, &model_request)
        .await
        .unwrap()
        .unwrap();
    use crate::model_egress_store::SignalModelEgressStore;
    SignalModelEgressStore::record_dispatch_intent_on(
        &txn,
        "snapshot-answer".into(),
        "snapshot-export".into(),
        2,
        &sink,
        &selected,
    )
    .await
    .unwrap();
    SignalModelEgressStore::link_context_dispatch_on(
        &txn,
        "snapshot-answer",
        &frame.session.conversation_id,
    )
    .await
    .unwrap();
    super::super::super::model_gate::authorize_on(
        &txn,
        Some(&prepared),
        &model_request,
        "snapshot-answer",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    remove_original(&frame).await;
    assert_eq!(
        frame
            .store
            .owner_source(1, &source, &AssistantDataPolicy::default(), frame.now + 1)
            .await
            .unwrap()
            .availability,
        desk_agent_protocol::ai_assistant::source::SourceAvailability::Unavailable
    );
    let native = SignalModelEgressStore::new(frame.store.db.clone());
    native
        .record_terminal_usage(
            "snapshot-answer",
            &desk_diagnose_core::chat::TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    native
        .mark_succeeded("snapshot-answer", &output)
        .await
        .unwrap();
    let mut answer = ChatMessage::text(
        "snapshot-answer-message",
        desk_diagnose_core::chat::ChatRole::Assistant,
        answer_text,
    );
    answer.turn_id = frame.session.current_turn_id.clone();
    answer.data_envelope = Some(output);
    frame.session.conversation.push(answer.clone());
    SessionSeam::save(&sessions(&frame), &mut frame.session)
        .await
        .unwrap();
    projection_row::Entity::delete_many()
        .filter(
            projection_row::Column::ProjectionId.eq(&read
                .projection
                .as_ref()
                .unwrap()
                .usage
                .projection_id),
        )
        .exec(&frame.store.db)
        .await
        .unwrap();
    let next = request(&mut frame).await;
    assert!(next.messages.contains(&saved) && next.messages.contains(&answer));
    let txn = frame.store.db.begin().await.unwrap();
    assert!(
        super::super::super::model_gate::prepare_on(&txn, &next)
            .await
            .unwrap()
            .is_some()
    );
    txn.rollback().await.unwrap();
    let own_answer = history::Entity::find()
        .filter(history::Column::ConversationId.eq(&frame.session.conversation_id))
        .filter(history::Column::NativeId.eq(&answer.message_id))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(own_answer.content.contains("approved"));
    assert_eq!(own_answer.dependencies_json, "[]");
}

#[tokio::test]
async fn source_deleted_before_snapshot_adoption_rejects_the_save_atomically() {
    let (mut frame, read, _) = fixture("owned-snapshot-before-delete").await;
    let before = agent_session::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    remove_original(&frame).await;
    stage(&mut frame, &read);
    assert!(
        SessionSeam::save(&sessions(&frame), &mut frame.session)
            .await
            .is_err()
    );
    assert_eq!(
        agent_session::Entity::find_by_id(before.id)
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        history::Entity::find()
            .filter(history::Column::NativeId.eq("accepted-search-snapshot"))
            .count(&frame.store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn snapshot_adoption_rejects_forged_body_call_and_current_session() {
    for case in 0..4 {
        let (mut frame, read, _) = fixture(&format!("owned-snapshot-forged-{case}")).await;
        stage(&mut frame, &read);
        match case {
            0 => frame
                .session
                .conversation
                .last_mut()
                .unwrap()
                .text
                .push_str(" forged"),
            1 => {
                frame.session.conversation.last_mut().unwrap().tool_call_id =
                    Some("another-call".into())
            }
            2 => frame.session.actor_id = "another-owner".into(),
            _ => frame.session.lease_token += 1,
        }
        assert!(
            SessionSeam::save(&sessions(&frame), &mut frame.session)
                .await
                .is_err(),
            "case {case}"
        );
        assert_eq!(
            history::Entity::find()
                .filter(history::Column::NativeId.eq("accepted-search-snapshot"))
                .count(&frame.store.db)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn accepted_snapshot_keeps_its_body_after_source_update_or_ordinary_expiry() {
    for expire in [false, true] {
        let (mut frame, read, source) = fixture(if expire {
            "snapshot-expiry"
        } else {
            "snapshot-update"
        })
        .await;
        let saved = stage(&mut frame, &read);
        SessionSeam::save(&sessions(&frame), &mut frame.session)
            .await
            .unwrap();
        let mut update = history::Entity::update_many();
        if expire {
            update = update.col_expr(
                history::Column::ExpiresAtMs,
                Expr::value(frame.now as i64 - 1),
            );
        } else {
            let text = "report changed after the original read";
            update = update
                .col_expr(history::Column::Content, Expr::value(text))
                .col_expr(
                    history::Column::ContentSha256,
                    Expr::value(format!("{:x}", Sha256::digest(text.as_bytes()))),
                )
                .col_expr(history::Column::Revision, Expr::value(2i64));
        }
        update
            .filter(history::Column::SourceId.eq(source))
            .exec(&frame.store.db)
            .await
            .unwrap();
        let next = request(&mut frame).await;
        assert!(next.messages.contains(&saved));
        let txn = frame.store.db.begin().await.unwrap();
        assert!(
            super::super::super::model_gate::prepare_on(&txn, &next)
                .await
                .unwrap()
                .is_some()
        );
        txn.rollback().await.unwrap();
        let copy = history::Entity::find()
            .filter(history::Column::ConversationId.eq(&frame.session.conversation_id))
            .filter(history::Column::NativeId.eq(&saved.message_id))
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(copy.content, saved.text);
        assert_eq!(copy.dependencies_json, "[]");
    }
}

#[test]
fn independent_snapshot_lifecycle_fits_small_stacks() {
    const CHILD: &str = "LRD_TEST_ASSISTANT_OWNED_SNAPSHOT_STACK_BYTES";
    if let Some(bytes) = std::env::var_os(CHILD) {
        let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
        assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
        actix_web::rt::System::new().block_on(async {
            let (frame, read, source) = fixture("snapshot-small-stack").await;
            let (done, ready) = tokio::sync::oneshot::channel();
            let thread = std::thread::Builder::new()
                .name("owned-snapshot-stack".into())
                .stack_size(bytes)
                .spawn(move || {
                    actix_web::rt::System::new().block_on(async move {
                        Box::pin(exercise_accepted(frame, read, source)).await;
                        let _ = done.send(());
                    })
                })
                .unwrap();
            ready.await.unwrap();
            thread.join().unwrap();
        });
        return;
    }
    let test = concat!(
        module_path!(),
        "::independent_snapshot_lifecycle_fits_small_stacks"
    );
    let test = test.split_once("::").unwrap().1;
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture"])
            .env(CHILD, bytes.to_string())
            .env_remove("RUST_MIN_STACK")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "snapshot lifecycle failed on {bytes} bytes: {status}"
        );
    }
}

#[tokio::test]
async fn next_owner_input_clears_only_ephemeral_memory_proofs_and_keeps_saved_snapshots() {
    let (mut frame, read, _) = fixture("snapshot-next-input").await;
    let saved = stage(&mut frame, &read);
    SessionSeam::save(&sessions(&frame), &mut frame.session)
        .await
        .unwrap();
    let proof = read.projection.as_ref().unwrap();
    let data = frame.session.assistant_data.as_mut().unwrap();
    data.read_uses.push(proof.usage.reference());
    data.history_dependencies = proof.usage.history_dependencies.clone();
    data.current_memories.push(
        desk_diagnose_core::ai_assistant::data_control::CurrentMemoryUse {
            memory_id: "previous-current-memory".into(),
            revision: 1,
            purpose: desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose::CurrentFact,
        },
    );
    let old_episode = data.retrieval_budget.episode_id.clone();
    let txn = frame.store.db.begin().await.unwrap();

    super::super::super::accept_input_on(&txn, &mut frame.session, "UTC", None, frame.now + 1)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let data = frame.session.assistant_data.as_ref().unwrap();
    assert!(
        data.read_uses.is_empty()
            && data.history_dependencies.is_empty()
            && data.current_memories.is_empty()
    );
    assert_ne!(data.retrieval_budget.episode_id, old_episode);
    assert!(frame.session.conversation.contains(&saved));
    assert!(
        history::Entity::find()
            .filter(history::Column::NativeId.eq(&saved.message_id))
            .one(&frame.store.db)
            .await
            .unwrap()
            .is_some()
    );
}
