use super::*;
use sea_orm::{ActiveModelTrait, ConnectionTrait, TransactionTrait};

async fn fixture(
    ai: bool,
) -> (
    super::super::ScheduleStore,
    run::Model,
    PersistedAgentSession,
) {
    let (store, work, _, session) = super::super::resume_claim::tests::fixture().await;
    if ai {
        entity::Entity::update_many()
            .col_expr(
                entity::Column::CreationSource,
                sea_orm::sea_query::Expr::value("ai_proposal"),
            )
            .exec(&store.db)
            .await
            .unwrap();
    }
    (store, work, session)
}
async fn request(
    store: &super::super::ScheduleStore,
    session: &PersistedAgentSession,
    owner: i32,
    id: &str,
    revision: i64,
) -> Value {
    let now = store.database_time().await.unwrap();
    let txn = store.db.begin().await.unwrap();
    let action = Action::Cancel {
        schedule_id: id.into(),
        expected_revision: revision,
    };
    let task = lock_target(&txn, owner, session, &action).await.unwrap();
    let reply = cancel(&txn, task, revision, now, &ToolObservation::default())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    serde_json::from_str(&reply).unwrap()
}

#[tokio::test]
async fn manual_tasks_are_readable_but_cannot_be_cancelled() {
    let (store, work, session) = fixture(false).await;
    let before = store.read(1, &work.schedule_id).await.unwrap();
    let txn = store.db.begin().await.unwrap();
    let page: Value =
        serde_json::from_str(&list(&txn, 1, &session, 0, 10, 1).await.unwrap()).unwrap();
    txn.commit().await.unwrap();
    assert_eq!(page["tasks"][0]["creation_source"], "manual");
    assert_eq!(page["tasks"][0]["can_cancel"], false);
    let reply = request(&store, &session, 1, &work.schedule_id, before.revision).await;
    assert_eq!(reply["reason"], "manual_task_cannot_be_cancelled_by_ai");
    assert_eq!(store.read(1, &work.schedule_id).await.unwrap(), before);
    assert_eq!(
        run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        work
    );
}

#[tokio::test]
async fn ai_cancellation_stops_queued_work_and_is_idempotent_without_grants() {
    let (store, work, session) = fixture(true).await;
    assert!(session.permission_requests.is_empty());
    let task = store.read(1, &work.schedule_id).await.unwrap();
    let reply = request(&store, &session, 1, &work.schedule_id, task.revision).await;
    assert_eq!(reply["state"], "cancelled");
    let after = store.read(1, &work.schedule_id).await.unwrap();
    assert_eq!(after.status, "deleted");
    assert!(after.next_run_at.is_none() && after.active_run_id.is_none());
    let cancelled = run::Entity::find_by_id(work.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(cancelled.attempt, 0);
    assert_eq!(
        request(&store, &session, 1, &work.schedule_id, task.revision).await["state"],
        "already_cancelled"
    );
}

#[tokio::test]
async fn other_subjects_and_stale_revisions_cannot_cancel() {
    let (store, work, session) = fixture(true).await;
    let before = store.read(1, &work.schedule_id).await.unwrap();
    for change in 0..3 {
        let mut other = session.clone();
        let owner = if change == 0 { 2 } else { 1 };
        if change == 1 {
            other.device_id = "other-device".into();
        }
        if change == 2 {
            other.conversation_id = "other-conversation".into();
        }
        assert_eq!(
            request(&store, &other, owner, &work.schedule_id, before.revision).await["reason"],
            "task_not_in_current_conversation"
        );
    }
    assert_eq!(
        request(&store, &session, 1, &work.schedule_id, before.revision - 1).await["reason"],
        "revision_changed_query_again"
    );
    assert_eq!(store.read(1, &work.schedule_id).await.unwrap(), before);
}

#[tokio::test]
async fn running_cancellation_reports_intent_without_claiming_effects_were_undone() {
    let (store, work, session) = fixture(true).await;
    let before = store.read(1, &work.schedule_id).await.unwrap();
    let mut running: run::ActiveModel = work.clone().into();
    running.status = Set("running".into());
    running.started_at = Set(Some(1));
    running.attempt = Set(1);
    running.update(&store.db).await.unwrap();
    assert_eq!(
        request(&store, &session, 1, &work.schedule_id, before.revision).await["state"],
        "cancellation_requested"
    );
    let after = run::Entity::find_by_id(work.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, "running");
    assert!(after.cancel_requested_at.is_some());
    assert!(after.finished_at.is_none());
}

#[tokio::test]
async fn fresh_task_is_bound_by_server_receipt_and_queries_are_paginated() {
    let (store, _, mut session) = fixture(false).await;
    let mut draft = super::super::tests::draft();
    draft.client_create_key = "ai-fresh".into();
    draft.creation_source = desk_agent_protocol::schedule::ScheduleCreationSource::AiProposal;
    let task = store
        .create_draft(1, &draft, store.database_time().await.unwrap())
        .await
        .unwrap();
    let text = json!({"state":"draft","schedule_id":task.schedule_id}).to_string();
    let parent = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "input".into(),
        "schedule a task".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let mut receipt =
        desk_diagnose_core::chat::ChatMessage::tool_result("receipt", "call", text.clone());
    receipt.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            parent.data_envelope.as_ref(),
            "call",
            &text,
            "schedule_proposal",
        )
        .unwrap();
    session.conversation.push(receipt);
    let txn = store.db.begin().await.unwrap();
    let first: Value =
        serde_json::from_str(&list(&txn, 1, &session, 0, 1, 1).await.unwrap()).unwrap();
    assert_eq!(first["tasks"].as_array().unwrap().len(), 1);
    let next = first["next_after"].as_i64().unwrap();
    let page: Value =
        serde_json::from_str(&list(&txn, 1, &session, next, 1, 1).await.unwrap()).unwrap();
    assert_eq!(page["tasks"][0]["schedule_id"], task.schedule_id);
    assert_eq!(page["tasks"][0]["can_cancel"], true);
    txn.commit().await.unwrap();
    assert_eq!(
        request(&store, &session, 1, &task.schedule_id, task.revision).await["state"],
        "cancelled"
    );
}

#[tokio::test]
async fn tool_turn_commits_cancellation_and_receipt_together_and_rejects_stale_state() {
    use crate::entity::agent_session;
    use desk_diagnose_core::chat::{ChatRole, ToolCall};
    for case in 0..3 {
        let (store, work, mut session) = fixture(true).await;
        let task = store.read(1, &work.schedule_id).await.unwrap();
        let now = chrono::Utc::now();
        session
            .begin_turn(
                "cancel-turn",
                Some("cancel-input".into()),
                Some("browser".into()),
                1,
                session.scope_snapshot.clone(),
                now.to_rfc3339(),
            )
            .unwrap();
        let call = ToolCall {
            id: "cancel-call".into(),
            name: desk_diagnose_core::schedule::management_tools::CANCEL.into(),
            arguments_json: json!({"schedule_id":task.schedule_id}).to_string(),
        };
        let mut parent = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "assistant".into(),
            "Cancel the timer".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        parent.role = ChatRole::Assistant;
        parent
            .tool_calls
            .push(desk_diagnose_core::chat::ToolCallRef {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            });
        parent.turn_id = session.current_turn_id.clone();
        let query_text = json!({"ok":true,"tasks":[view(&task)]}).to_string();
        let mut query = desk_diagnose_core::chat::ChatMessage::tool_result(
            "schedule-query",
            "query-call",
            &query_text,
        );
        query.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                parent.data_envelope.as_ref(),
                "schedule-query",
                &query_text,
                "schedule_query",
            )
            .unwrap();
        let input = query.data_envelope.clone().unwrap();
        let policy = desk_diagnose_core::model_egress::ModelEgressPolicy {
            destination: input.allowed_destinations[0].clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "schedule-control-test".into(),
            now_unix_ms: now.timestamp_millis() as u64,
            byte_cap: 1024 * 1024,
            permission_resume: false,
        };
        let turn = desk_diagnose_core::chat::ModelTurn {
            text: parent.text.clone(),
            tool_calls: vec![call.clone()],
            stop_reason: desk_diagnose_core::chat::StopReason::ToolUse,
            ..Default::default()
        };
        let output = policy
            .derive_model_output_envelope(&turn, std::slice::from_ref(&input))
            .unwrap();
        parent.model_controls = Some(Box::new(
            desk_diagnose_core::model_controls::FrozenModelControls::capture(&session, &[query]),
        ));
        parent.data_envelope = Some(output.clone());
        let audit = desk_diagnose_core::sink_authorizer::SinkProjectionAudit {
            destination: policy.destination.clone(),
            envelope_ids: vec![input.envelope_id.clone()],
            digests_sha256: vec![input.digest_sha256.clone()],
            total_bytes: query_text.len(),
        };
        let schema = sea_orm::Schema::new(store.db.get_database_backend());
        store
            .db
            .execute(&schema.create_table_from_entity(crate::entity::model_egress_receipt::Entity))
            .await
            .unwrap();
        let egress = crate::model_egress_store::SignalModelEgressStore::new(store.db.clone());
        egress
            .record_dispatch_intent(
                "schedule-test-model".into(),
                policy.export_authorization_id.clone(),
                1,
                &audit,
                &[input],
            )
            .await
            .unwrap();
        egress
            .link_context_dispatch("schedule-test-model", &session.conversation_id)
            .await
            .unwrap();
        egress
            .mark_succeeded("schedule-test-model", &output)
            .await
            .unwrap();
        session.conversation.push(parent);
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let row_id = row.id;
        let mut row: agent_session::ActiveModel = row.into();
        row.state_json = Set(session.encode_json_for_storage().unwrap());
        row.lease_token = Set(session.lease_token as i64);
        row.lease_deadline = Set(Some(now + chrono::Duration::minutes(2)));
        row.update(&store.db).await.unwrap();
        if case == 1 {
            session.version += 1;
        }
        if case == 2 {
            use sea_orm::ConnectionTrait;
            store.db.execute_unprepared("CREATE TRIGGER reject_tool_receipt BEFORE UPDATE OF state_json ON agent_session BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END").await.unwrap();
        }
        let reply = store
            .manage_from_session(
                &mut session,
                &call,
                &desk_diagnose_core::model_observability::tool::ToolObservation::default(),
            )
            .await;
        if case != 0 {
            assert!(reply.is_err());
            assert_eq!(store.read(1, &work.schedule_id).await.unwrap(), task);
            assert_eq!(
                run::Entity::find_by_id(work.id)
                    .one(&store.db)
                    .await
                    .unwrap()
                    .unwrap(),
                work
            );
        } else {
            reply.unwrap();
            assert_eq!(
                store.read(1, &work.schedule_id).await.unwrap().status,
                "deleted"
            );
            let receipt = session.conversation.last().unwrap();
            assert_eq!(receipt.tool_call_id.as_deref(), Some("cancel-call"));
            assert_eq!(
                serde_json::from_str::<Value>(&receipt.text).unwrap()["state"],
                "cancelled"
            );
            let row = agent_session::Entity::find_by_id(row_id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.state_json, session.encode_json_for_storage().unwrap());
        }
    }
}
