use super::*;
use crate::agent_session_store::SignalAgentSessionStore as AgentSessionStore;
use crate::entity::agent_assistant_history as history;
use desk_agent_protocol::ai_assistant::source::{
    HistoryDependency, SourceAvailability, SourceUsePolicy,
};
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole},
    seam::SessionSeam,
    session::PersistedAgentSession,
};

#[tokio::test]
async fn an_accepted_memory_answer_is_saved_after_forgetting_and_remains_visible_to_its_owner() {
    let (store, _, proof, _) = admitted("visible-memory-after-forget").await;
    let candidate = output(&proof);
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    let archived = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    let settings = store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    store
        .update_settings(
            1,
            &AssistantDataSettingsUpdate {
                expected_revision: settings.revision,
                automatic_memory_enabled: false,
                model_history_search_enabled: false,

                explanation_version: settings.explanation_version,
            },
            &AssistantDataPolicy::default(),
        )
        .await
        .unwrap();
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: proof.usage.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-before-own-visible-save".into(),
            },
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let mut session = visible_session(&store, candidate).await;
    let host = AgentSessionStore::new(store.db.clone());
    host.save(&mut session).await.unwrap();
    let row = history::Entity::find()
        .filter(history::Column::Kind.eq("assistant_message"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.native_id, "actual-memory-answer");
    assert_eq!(row.content, "Completed using the selected memory");
    assert_eq!(row.recorded_at_ms, archived.row.completed_at_ms);
    let dependencies: Vec<HistoryDependency> =
        serde_json::from_str(&row.dependencies_json).unwrap();
    assert!(dependencies.is_empty());
    let source = store
        .owner_source(
            1,
            &row.source_id(),
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    assert!(matches!(
        source.availability,
        SourceAvailability::Available | SourceAvailability::Redacted | SourceAvailability::Partial
    ));
    assert_eq!(
        source.content.as_deref(),
        Some("Completed using the selected memory")
    );
    // Owner retention does not reopen the original physical model use.
    assert!(
        native_export::load_call_set_on(&store.db, &archived.proof.source_set)
            .await
            .is_err()
    );
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    host.save(&mut session).await.unwrap();
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
}

#[tokio::test]
async fn late_source_changes_do_not_withdraw_the_already_accepted_answer_copy() {
    for case in 0..3 {
        let (store, _, proof, origin_id) =
            admitted(&format!("visible-memory-live-copy-policy-{case}")).await;
        let candidate = output(&proof);
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &candidate)
            .await
            .unwrap();
        let before_native = native::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let original = origin::Entity::find()
            .filter(origin::Column::OriginId.eq(&origin_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        if case == 0 {
            origin::Entity::update_many()
                .col_expr(
                    origin::Column::DeletedAtMs,
                    Expr::value(Some(chrono::Utc::now().timestamp_millis())),
                )
                .col_expr(
                    origin::Column::DeletionGeneration,
                    Expr::value(original.deletion_generation + 1),
                )
                .filter(origin::Column::Id.eq(original.id))
                .exec(&store.db)
                .await
                .unwrap();
        } else {
            let mut policy: SourceUsePolicy = serde_json::from_str(&original.policy_json).unwrap();
            if case == 1 {
                policy.no_copy = true;
            } else {
                policy.content_hard_expires_at_unix_ms = Some(2);
            }
            origin::Entity::update_many()
                .col_expr(
                    origin::Column::PolicyJson,
                    Expr::value(serde_json::to_string(&policy).unwrap()),
                )
                .filter(origin::Column::Id.eq(original.id))
                .exec(&store.db)
                .await
                .unwrap();
        }
        let mut session = visible_session(&store, candidate).await;
        AgentSessionStore::new(store.db.clone())
            .save(&mut session)
            .await
            .unwrap();
        let saved = history::Entity::find()
            .filter(history::Column::Kind.eq("assistant_message"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.dependencies_json, "[]");
        assert_eq!(
            native::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before_native
        );
        assert_eq!(
            session.conversation.last().unwrap().text,
            "Completed using the selected memory"
        );
    }
}

async fn visible_session(
    store: &TestStore,
    candidate: desk_agent_protocol::data_lineage::DataEnvelope,
) -> Box<PersistedAgentSession> {
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
    let mut message = ChatMessage::text(
        "actual-memory-answer",
        ChatRole::Assistant,
        "Completed using the selected memory",
    );
    message.data_envelope = Some(candidate);
    message.turn_id = session.current_turn_id.clone();
    session.conversation.push(message);
    session
}

#[tokio::test]
async fn citations_are_bound_to_the_actual_consumed_roots_in_the_original_answer_cas() {
    use crate::entity::agent_assistant_citation as citation;
    for (fail_storage, missing_policy) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let (store, _, proof, _) =
            admitted(&format!("native-citation-{fail_storage}-{missing_policy}")).await;
        let root = proof
            .usage
            .roots
            .first()
            .expect("actual consumed memory source");
        let unknown = desk_diagnose_core::native_ids::fixture_source_id(&"unprovided-citation");
        let text = format!(
            "Known 〔来源:{}〕; unknown 〔来源:{unknown}〕; code `〔来源:{}〕`",
            root.source_id, root.source_id
        );
        let candidate = super::super::output_text(&proof, &text);
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &candidate)
            .await
            .unwrap();
        if missing_policy {
            // A complete old native record may lack whole-answer copy metadata.
            // Its consumed read identities still prove the locators it observed.
            let mut archived = output_archive::find_on(&store.db, "root", &candidate)
                .await
                .unwrap()
                .unwrap();
            archived.proof.retained_policy = None;
            archived.proof.retained_roots.clear();
            output_archive::rewrite_for_test(&store.db, archived.row.id, &archived.proof)
                .await
                .unwrap();
        }
        let mut session = visible_session(&store, candidate).await;
        let answer = session
            .conversation
            .iter_mut()
            .find(|message| message.role == ChatRole::Assistant && !message.text.is_empty())
            .unwrap();
        answer.text = text.clone();
        let answer_id = answer.message_id.clone();
        if fail_storage {
            store.db.execute_raw(sea_orm::Statement::from_string(sea_orm::DatabaseBackend::Sqlite,
            "CREATE TRIGGER fail_optional_citation BEFORE INSERT ON agent_assistant_citation BEGIN SELECT RAISE(ABORT, 'citation storage unavailable'); END")).await.unwrap();
        }
        let host = crate::agent_session_store::SignalAgentSessionStore::new(store.db.clone());
        host.save(&mut session).await.unwrap();
        let saved = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            PersistedAgentSession::decode_json(&saved.state_json)
                .unwrap()
                .conversation
                .iter()
                .any(|message| message.message_id == answer_id && message.text == text)
        );
        let output = PersistedAgentSession::decode_json(&saved.state_json)
            .unwrap()
            .conversation
            .iter()
            .find(|message| message.message_id == answer_id)
            .and_then(|message| message.data_envelope.as_ref())
            .map(|envelope| envelope.envelope_id.clone());
        let rows = citation::Entity::find().all(&store.db).await.unwrap();
        assert_eq!(rows.len(), usize::from(!fail_storage));
        let bindings = crate::assistant_data_store::load_citations_on(
            &store.db,
            "1",
            &session.conversation_id,
            &[(answer_id.clone(), text.clone(), output.clone())],
            chrono::Utc::now().timestamp_millis() as u64,
            &AssistantDataPolicy::default(),
        )
        .await
        .unwrap();
        if fail_storage {
            for stack_bytes in [1024 * 1024, 2 * 1024 * 1024] {
                let db = store.db.clone();
                let conversation = session.conversation_id.clone();
                let answers = vec![(answer_id.clone(), text.clone(), output.clone())];
                let handle = tokio::runtime::Handle::current();
                let (done, completed) = tokio::sync::oneshot::channel();
                let thread = std::thread::Builder::new()
                    .stack_size(stack_bytes)
                    .spawn(move || {
                        handle.block_on(async move {
                            let result = crate::assistant_data_store::load_citations_on(
                                &db,
                                "1",
                                &conversation,
                                &answers,
                                chrono::Utc::now().timestamp_millis() as u64,
                                &AssistantDataPolicy::default(),
                            )
                            .await;
                            let _ = done.send(result);
                        });
                    })
                    .unwrap();
                let actual = completed.await.unwrap().unwrap();
                thread.join().unwrap();
                assert_eq!(actual[&answer_id].citations, bindings[&answer_id].citations);
            }
        }
        let values = &bindings.get(&answer_id).unwrap_or_else(|| panic!("missing citations: fail_storage={fail_storage}, missing_policy={missing_policy}")).citations;
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].source_id, root.source_id);
        assert_eq!(values[0].source_revision, root.source_revision);
        assert!(
            rows.iter()
                .all(|row| !row.citations_json.contains(&unknown))
        );
        for (actor, conversation, body, envelope) in [
            ("2", "root", text.clone(), output.clone()),
            ("1", "other", text.clone(), output.clone()),
            (
                "1",
                "root",
                format!("changed {text}"),
                Some("other-output".to_string()),
            ),
        ] {
            assert!(
                crate::assistant_data_store::load_citations_on(
                    &store.db,
                    actor,
                    conversation,
                    &[(answer_id.clone(), body, envelope)],
                    chrono::Utc::now().timestamp_millis() as u64,
                    &AssistantDataPolicy::default(),
                )
                .await
                .unwrap()
                .is_empty()
            );
        }
        let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
        host.save(&mut session).await.unwrap();
        assert_eq!(citation::Entity::find().all(&store.db).await.unwrap(), rows);
        assert_eq!(
            quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn decoded_archive_reuse_still_reselects_exact_rows() {
    use crate::entity::agent_assistant_model_output as archived;
    for case in 0..3 {
        let (store, _, proof, _) = admitted(&format!("archive-raw-revalidation-{case}")).await;
        let candidate = output(&proof);
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &candidate)
            .await
            .unwrap();
        let known = output_archive::find_on(&store.db, "root", &candidate)
            .await
            .unwrap()
            .unwrap();
        let txn = store.db.begin().await.unwrap();
        crate::model_egress_store::SignalModelEgressStore::read_context_output_reusing_on(
            &txn,
            "root",
            &candidate,
            Some(&known),
        )
        .await
        .unwrap();
        match case {
            0 => {
                archived::Entity::update_many()
                    .filter(archived::Column::Id.eq(known.row.id))
                    .col_expr(
                        archived::Column::ProofJson,
                        Expr::value(format!(" {}", known.row.proof_json)),
                    )
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            1 => {
                archived::Entity::update_many()
                    .filter(archived::Column::Id.eq(known.row.id))
                    .col_expr(
                        archived::Column::OutputEnvelopeId,
                        Expr::value("changed-output"),
                    )
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            _ => {
                native::Entity::update_many()
                    .col_expr(native::Column::State, Expr::value("changed"))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
        }
        let current =
            crate::model_egress_store::SignalModelEgressStore::read_context_output_with_archive_on(
                &txn,
                "root",
                &candidate,
                Some(&known),
            )
            .await;
        if case == 0 {
            let (_, refreshed) = current.unwrap();
            assert_ne!(
                refreshed.unwrap().row,
                known.row,
                "the final native fence must see the changed raw row"
            );
        } else {
            assert!(current.is_err(), "tampered archive case {case}");
        }
        txn.rollback().await.unwrap();
        crate::model_egress_store::SignalModelEgressStore::read_context_output_reusing_on(
            &store.db,
            "root",
            &candidate,
            Some(&known),
        )
        .await
        .unwrap();
    }
}
