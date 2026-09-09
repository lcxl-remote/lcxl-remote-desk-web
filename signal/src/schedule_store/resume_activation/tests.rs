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
