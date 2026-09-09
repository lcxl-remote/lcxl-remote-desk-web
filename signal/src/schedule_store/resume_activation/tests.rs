use super::*;
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    schedule::{ScheduleRule, ScheduledTaskKind},
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

async fn fixture() -> (
    ScheduleStore,
    entity::Model,
    session_row::Model,
    PersistedAgentSession,
) {
    let store = super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(session_row::Entity))
        .await
        .unwrap();
    let mut session = PersistedAgentSession::new(
        "source",
        "1",
        "device-1",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-06T00:00:00Z",
    );
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    let row = session_row::ActiveModel {
        conversation_id: Set("source".into()),
        actor_id: Set("1".into()),
        device_id: Set("device-1".into()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(0),
        lease_token: Set(0),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    let mut draft = super::super::tests::draft();
    draft.kind = ScheduledTaskKind::ConversationResume;
    draft.source_conversation_id = Some("source".into());
    draft.requirement_revision = Some(1);
    draft.spec.rule = ScheduleRule::Once {
        at: "2099-01-01T00:00:00Z".into(),
    };
    let task = store
        .create_draft(1, &draft, store.database_time().await.unwrap())
        .await
        .unwrap();
    (store, task, row, session)
}

#[tokio::test]
async fn confirmation_enables_once_with_receipt_without_granting_authority() {
    let (store, task, row, _) = fixture().await;
    let active = store
        .activate_conversation_resume(1, &task.schedule_id, task.revision)
        .await
        .unwrap();
    assert_eq!(active.status, "active");
    assert_eq!(active.revision, task.revision + 1);
    assert_eq!(active.task_revision, task.task_revision);
    assert_eq!(active.requirement_revision, Some(1));
    assert_eq!(active.next_run_at, Some(4_070_908_800_000));
    assert!(
        active.contract_revision.is_none()
            && active.authorization_revision.is_none()
            && active.active_run_id.is_none()
    );
    let updated = session_row::Entity::find_by_id(row.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let before = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let after = PersistedAgentSession::decode_json(&updated.state_json).unwrap();
    assert_eq!(after.input_revision, before.input_revision);
    assert_eq!(after.latest_input_seq, before.latest_input_seq);
    assert_eq!(after.scope_snapshot, before.scope_snapshot);
    assert_eq!(after.conversation.len(), before.conversation.len() + 1);
    assert!(
        after
            .conversation
            .last()
            .unwrap()
            .text
            .contains("scheduled_task_activated")
    );
    assert!(matches!(
        store
            .activate_conversation_resume(1, &task.schedule_id, task.revision)
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert!(matches!(
        store
            .activate_conversation_resume(2, &task.schedule_id, active.revision)
            .await,
        Err(ScheduleStoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_new_user_input_after_draft_creation_prevents_activation() {
    let (store, task, row, mut session) = fixture().await;
    session.begin_focus_epoch(2, Vec::<String>::new()).unwrap();
    session.input_revision = 2;
    let mut changed: session_row::ActiveModel = row.into();
    changed.state_json = Set(session.encode_json_for_storage().unwrap());
    changed.version = Set(1);
    changed.update(&store.db).await.unwrap();
    assert!(matches!(
        store
            .activate_conversation_resume(1, &task.schedule_id, task.revision)
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), task);
}

#[tokio::test]
async fn elapsed_time_rolls_back_the_confirmation_cas() {
    let (store, task, row, _) = fixture().await;
    let mut changed: entity::ActiveModel = task.clone().into();
    changed.spec_json =
        Set(r#"{"schema_version":1,"rule":{"kind":"once","at":"2000-01-01T00:00:00Z"}}"#.into());
    let expired = changed.update(&store.db).await.unwrap();
    assert!(matches!(
        store
            .activate_conversation_resume(1, &task.schedule_id, task.revision)
            .await,
        Err(ScheduleStoreError::Invalid)
    ));
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), expired);
    assert_eq!(
        session_row::Entity::find_by_id(row.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        row
    );
}

#[tokio::test]
async fn activation_does_not_bypass_task_kind_or_source_surface() {
    let (store, task, row, mut session) = fixture().await;
    let mut changed: entity::ActiveModel = task.clone().into();
    changed.kind = Set("fresh_task".into());
    let fresh = changed.update(&store.db).await.unwrap();
    assert!(matches!(
        store
            .activate_conversation_resume(1, &task.schedule_id, task.revision)
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), fresh);
    let mut changed: entity::ActiveModel = fresh.into();
    changed.kind = Set("conversation_resume".into());
    changed.update(&store.db).await.unwrap();
    session.surface = AgentSessionSurface::TerminalCopilot;
    let mut changed: session_row::ActiveModel = row.into();
    changed.state_json = Set(session.encode_json_for_storage().unwrap());
    changed.update(&store.db).await.unwrap();
    assert!(matches!(
        store
            .activate_conversation_resume(1, &task.schedule_id, task.revision)
            .await,
        Err(ScheduleStoreError::NotFound)
    ));
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), task);
}

#[tokio::test]
async fn relative_activation_uses_confirmation_clock_and_preserves_requirement() {
    let (store, task, row, before) = fixture().await;
    entity::Entity::update_many().set(entity::ActiveModel {
        spec_json: Set(serde_json::json!({"schema_version":1,"rule":{"kind":"after_confirmation","delay_seconds":300}}).to_string()),
        ..Default::default()
    }).filter(entity::Column::Id.eq(task.id)).exec(&store.db).await.unwrap();
    let start = store.database_time().await.unwrap();
    let active = store
        .activate_conversation_resume(1, &task.schedule_id, task.revision)
        .await
        .unwrap();
    let end = store.database_time().await.unwrap();
    let at = active.next_run_at.unwrap();
    assert!(at >= start + 300_000 && at <= end + 301_000);
    let spec: desk_agent_protocol::schedule::ScheduleSpec =
        serde_json::from_str(&active.spec_json).unwrap();
    assert!(matches!(spec.rule, ScheduleRule::Once { .. }));
    let updated = session_row::Entity::find_by_id(row.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let after = PersistedAgentSession::decode_json(&updated.state_json).unwrap();
    assert_eq!(after.input_revision, before.input_revision);
    assert_eq!(after.latest_input_seq, before.latest_input_seq);
    assert!(
        after
            .conversation
            .last()
            .unwrap()
            .text
            .contains("scheduled_task_activated")
    );
    assert!(
        store
            .activate_conversation_resume(1, &task.schedule_id, task.revision)
            .await
            .is_err()
    );
    assert_eq!(
        session_row::Entity::find_by_id(row.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        updated
    );
}

#[tokio::test]
async fn fresh_task_edit_rejects_confirmation_delay_without_changing_the_task() {
    let (store, _, _, _) = fixture().await;
    let mut draft = super::super::tests::draft();
    draft.client_create_key = "fresh-delay-edit".into();
    let task = store
        .create_draft(1, &draft, store.database_time().await.unwrap())
        .await
        .unwrap();
    let spec = desk_agent_protocol::schedule::ScheduleSpec {
        schema_version: 1,
        rule: ScheduleRule::AfterConfirmation { delay_seconds: 300 },
    };
    assert!(
        store
            .change_time(1, &task.schedule_id, task.revision, &spec)
            .await
            .is_err()
    );
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), task);
}

#[tokio::test]
async fn rejection_records_a_durable_decision_and_cannot_activate() {
    let (store, task, row, before) = fixture().await;
    let rejected = store
        .delete(1, &task.schedule_id, task.revision)
        .await
        .unwrap();
    assert_eq!(rejected.status, "deleted");
    assert!(rejected.next_run_at.is_none());
    let row = session_row::Entity::find_by_id(row.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let after = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(after.input_revision, before.input_revision);
    assert_eq!(after.scope_snapshot, before.scope_snapshot);
    let event: serde_json::Value =
        serde_json::from_str(&after.conversation.last().unwrap().text).unwrap();
    assert_eq!(event["event"], "scheduled_task_rejected");
    assert_eq!(event["owner_decision"], "rejected");
    assert!(
        store
            .activate_conversation_resume(1, &task.schedule_id, rejected.revision)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn orphan_draft_can_still_be_rejected() {
    let (store, task, row, _) = fixture().await;
    session_row::Entity::delete_by_id(row.id)
        .exec(&store.db)
        .await
        .unwrap();
    assert_eq!(
        store
            .delete(1, &task.schedule_id, task.revision)
            .await
            .unwrap()
            .status,
        "deleted"
    );
}

#[tokio::test]
async fn live_review_delivers_approval_or_rejection_to_the_same_turn() {
    for approve in [true, false] {
        let (store, _, row, mut session) = fixture().await;
        session
            .begin_turn(
                "review-turn",
                None,
                None,
                1,
                session.scope_snapshot.clone(),
                "2026-09-09T00:00:00Z",
            )
            .unwrap();
        let call = desk_diagnose_core::chat::ToolCall {
            id: "review-call".into(),
            name: desk_diagnose_core::schedule::proposal::REQUEST_SCHEDULE.into(),
            arguments_json: serde_json::json!({"kind":"conversation_resume","title":"Later","prompt":"Say hello","rule":{"kind":"after_confirmation","delay_seconds":60}}).to_string(),
        };
        let mut parent = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "assistant".into(),
            "Schedule hello".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        parent.role = desk_diagnose_core::chat::ChatRole::Assistant;
        parent.replay_disposition =
            Some(desk_diagnose_core::replay::ReplayDisposition::NotRequired {
                source_context_key: desk_diagnose_core::replay::SourceContextKey::derive(
                    desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
                    "test",
                    "test",
                    "test",
                ),
            });
        parent
            .tool_calls
            .push(desk_diagnose_core::chat::ToolCallRef {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            });
        session.conversation.push(parent);
        let mut changed: session_row::ActiveModel = row.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.lease_token = Set(session.lease_token as i64);
        changed.lease_deadline = Set(Some(chrono::Utc::now() + chrono::Duration::minutes(5)));
        changed.update(&store.db).await.unwrap();
        store
            .manage_from_session(&mut session, &call)
            .await
            .unwrap();
        let proposal: serde_json::Value =
            serde_json::from_str(&session.conversation.last().unwrap().text).unwrap();
        assert_eq!(proposal["awaiting_confirmation"], true);
        let id = session.pending_schedule_review.clone().unwrap();
        assert_eq!(proposal["schedule_id"], id);
        let task = store.read(1, &id).await.unwrap();
        assert_eq!(task.status, "pending_review");
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
        assert!(
            !store
                .poll_review_decision(&mut session, &task.schedule_id)
                .await
                .unwrap()
        );
        let before = session.clone();
        if approve {
            store
                .activate_conversation_resume(1, &task.schedule_id, task.revision)
                .await
                .unwrap();
        } else {
            store
                .delete(1, &task.schedule_id, task.revision)
                .await
                .unwrap();
        }
        assert!(
            store
                .poll_review_decision(&mut session, &task.schedule_id)
                .await
                .unwrap()
        );
        assert_eq!(session.current_turn_id, before.current_turn_id);
        assert_eq!(session.lease_token, before.lease_token);
        assert_eq!(session.scope_snapshot, before.scope_snapshot);
        assert!(session.turn_state.is_active());
        let receipt: serde_json::Value =
            serde_json::from_str(&session.conversation.last().unwrap().text).unwrap();
        assert_eq!(
            receipt["owner_decision"],
            if approve { "approved" } else { "rejected" }
        );
    }
}

#[tokio::test]
async fn live_review_cannot_adopt_new_input_or_a_changed_lease() {
    for change_lease in [true, false] {
        let (store, task, row, mut session) = fixture().await;
        session
            .begin_turn(
                "review-turn",
                None,
                None,
                1,
                session.scope_snapshot.clone(),
                "2026-09-09T00:00:00Z",
            )
            .unwrap();
        session.pending_schedule_review = Some(task.schedule_id.clone());
        let before = session.clone();
        let mut updated = session.clone();
        if change_lease {
            updated.lease_token += 1;
        } else {
            updated.begin_focus_epoch(2, Vec::<String>::new()).unwrap();
            updated.input_revision = 2;
        }
        let mut changed: session_row::ActiveModel = row.into();
        changed.state_json = Set(updated.encode_json_for_storage().unwrap());
        changed.lease_token = Set(updated.lease_token as i64);
        changed.lease_deadline = Set(Some(chrono::Utc::now() + chrono::Duration::minutes(5)));
        changed.update(&store.db).await.unwrap();
        assert!(matches!(
            store
                .poll_review_decision(&mut session, &task.schedule_id)
                .await,
            Err(ScheduleStoreError::Conflict)
        ));
        assert_eq!(session, before);
    }
}

#[tokio::test]
async fn pending_requests_are_not_tasks_and_manual_creation_is_atomic() {
    let (store, pending, _, _) = fixture().await;
    assert!(store.list(1, 0, 100).await.unwrap().is_empty());
    assert_eq!(
        store
            .search(1, 0, 100, None, None, None, None, None, false)
            .await
            .unwrap()
            .1,
        0
    );
    let mut draft = super::super::tests::draft();
    draft.kind = ScheduledTaskKind::ConversationResume;
    draft.source_conversation_id = Some("source".into());
    draft.requirement_revision = Some(1);
    draft.client_create_key = "manual-live".into();
    draft.spec.rule = ScheduleRule::AfterConfirmation { delay_seconds: 60 };
    draft.creation_source = desk_agent_protocol::schedule::ScheduleCreationSource::Manual;
    let active = store.create_conversation_task(1, &draft).await.unwrap();
    assert_eq!(active.status, "active");
    assert!(active.next_run_at.is_some());
    assert_eq!(
        store.create_conversation_task(1, &draft).await.unwrap(),
        active
    );
    assert_eq!(store.list(1, 0, 100).await.unwrap().len(), 1);
    draft.client_create_key = "invalid-source".into();
    draft.requirement_revision = Some(2);
    assert!(store.create_conversation_task(1, &draft).await.is_err());
    use sea_orm::PaginatorTrait;
    assert_eq!(entity::Entity::find().count(&store.db).await.unwrap(), 2);
    store
        .delete(1, &pending.schedule_id, pending.revision)
        .await
        .unwrap();
    assert_eq!(store.list(1, 0, 100).await.unwrap().len(), 1);
}

#[tokio::test]
async fn search_filters_conversation_before_pagination_and_counts() {
    let (store, task, _, _) = fixture().await;
    let mut row: entity::ActiveModel = task.clone().into();
    row.status = Set("completed".into());
    row.update(&store.db).await.unwrap();
    let mut other: entity::ActiveModel = task.clone().into();
    other.id = Default::default();
    other.schedule_id = Set("another-schedule".into());
    other.creation_identity = Set("another-create-key".into());
    other.source_conversation_id = Set(Some("other-conversation".into()));
    other.status = Set("active".into());
    other.insert(&store.db).await.unwrap();
    let (rows, total, attention) = store
        .search(
            1,
            0,
            1,
            Some("conversation_resume"),
            None,
            None,
            None,
            Some("source"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].schedule_id, task.schedule_id);
    assert_eq!(rows[0].status, "completed");
    assert_eq!(total, 1);
    assert_eq!(attention, 0);
    let (other_rows, other_total, _) = store
        .search(
            1,
            0,
            1,
            None,
            None,
            None,
            None,
            Some("other-conversation"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(other_rows[0].schedule_id, "another-schedule");
    assert_eq!(other_total, 1);
    assert!(
        store
            .search(1, 0, 10, None, None, None, None, Some("unknown"), false)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        store
            .search(2, 0, 10, None, None, None, None, Some("source"), false)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        store
            .search(
                1,
                0,
                10,
                None,
                None,
                None,
                Some("other-device"),
                Some("source"),
                false
            )
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        store
            .search(1, 0, 10, None, None, None, None, Some(""), false)
            .await
            .is_err()
    );
}
