use super::*;
use sea_orm::{ConnectionTrait, Database};

fn params() -> UpdateLiveContext {
    let now = Utc::now();
    let mut selection = crate::agent_session_store::tests::context_selection(
        true,
        "candidate-1",
        "worker-1",
        now.timestamp_millis() as u64,
    );
    selection.candidates[0].client_request_id =
        desk_diagnose_core::live_context::selection_request_id("request-1", "desktop.ui.inspect");
    UpdateLiveContext {
        run_id: "conversation-1".into(),
        actor_id: "1".into(),
        device_id: "device-1".into(),
        update: DeviceAssistantContextUpdate {
            conversation_id: "client-conversation".into(),
            client_request_id: "request-1".into(),
            selected_capability_ids: selection.selected_capability_ids.clone(),
        },
        selection: Some(selection),
        created_at: now.to_rfc3339(),
    }
}

async fn store(url: &str) -> SignalAgentSessionStore {
    let db = Database::connect(url).await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    SignalAgentSessionStore::new(db).with_client_metadata(
        Some("client-conversation".into()),
        AgentSessionSurface::DeviceAssistant,
    )
}

async fn rows(
    store: &SignalAgentSessionStore,
) -> (Vec<agent_session::Model>, Vec<agent_run_event::Model>) {
    (
        agent_session::Entity::find().all(&store.db).await.unwrap(),
        agent_run_event::Entity::find()
            .all(&store.db)
            .await
            .unwrap(),
    )
}

fn clear(original: &UpdateLiveContext) -> UpdateLiveContext {
    UpdateLiveContext {
        update: DeviceAssistantContextUpdate {
            selected_capability_ids: vec![],
            client_request_id: "clear-1".into(),
            ..original.update.clone()
        },
        selection: Some(ContextSelectionClaim {
            selected_capability_ids: vec![],
            runtime_bindings: vec![],
            candidates: vec![],
            now_unix_ms: Utc::now().timestamp_millis() as u64,
        }),
        created_at: Utc::now().to_rfc3339(),
        run_id: original.run_id.clone(),
        actor_id: original.actor_id.clone(),
        device_id: original.device_id.clone(),
    }
}

#[tokio::test]
async fn original_live_receipt_survives_deselection_and_replays_without_new_authority() {
    let store = store("sqlite::memory:").await;
    let mut original = params();
    assert!(store.update_live_context(&original).await.unwrap());
    let first = rows(&store).await;
    assert!(store.update_live_context(&original).await.unwrap());
    assert_eq!(rows(&store).await, first);
    let cleared = clear(&original);
    assert!(store.update_live_context(&cleared).await.unwrap());
    let saved = rows(&store).await;
    original.selection = None;
    assert_eq!(
        store.replay_live_context(&original).await.unwrap(),
        Some(true)
    );
    assert!(store.update_live_context(&original).await.unwrap());
    assert_eq!(rows(&store).await, saved);
    let session = PersistedAgentSession::decode_json(&saved.0[0].state_json).unwrap();
    assert!(session.scope_snapshot.granted.is_empty());
    assert_eq!(session.input_revision, 0);
    assert!(session.conversation.is_empty());
    assert!(matches!(
        session.context_attachments[0].state,
        AttachmentState::Stale { .. }
    ));
    let mut no_change = clear(&original);
    no_change.update.client_request_id = "clear-again".into();
    assert!(!store.update_live_context(&no_change).await.unwrap());
    assert_eq!(
        store.replay_live_context(&no_change).await.unwrap(),
        Some(false)
    );
}

#[tokio::test]
async fn changed_request_subject_and_missing_old_receipt_cannot_create_history() {
    let store = store("sqlite::memory:").await;
    let mut original = params();
    store.update_live_context(&original).await.unwrap();
    let saved = rows(&store).await;
    original.update.selected_capability_ids.clear();
    assert!(store.replay_live_context(&original).await.is_err());
    original = params();
    original.actor_id = "other".into();
    assert!(store.update_live_context(&original).await.is_err());
    assert_eq!(rows(&store).await, saved);
    agent_run_event::Entity::delete_many()
        .exec(&store.db)
        .await
        .unwrap();
    assert!(store.replay_live_context(&params()).await.is_err());
    assert!(store.update_live_context(&params()).await.is_err());
    assert!(rows(&store).await.1.is_empty());
}

#[tokio::test]
async fn live_selection_and_receipt_insert_roll_back_together() {
    let store = store("sqlite::memory:").await;
    store.db.execute_unprepared("CREATE TRIGGER reject_live_receipt BEFORE INSERT ON agent_run_event BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").await.unwrap();
    let original = params();
    assert!(store.update_live_context(&original).await.is_err());
    assert_eq!(rows(&store).await, (vec![], vec![]));
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_live_receipt")
        .await
        .unwrap();
    store.update_live_context(&original).await.unwrap();
    let saved = rows(&store).await;
    store.db.execute_unprepared("CREATE TRIGGER reject_live_receipt BEFORE INSERT ON agent_run_event BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").await.unwrap();
    assert!(store.update_live_context(&clear(&original)).await.is_err());
    assert_eq!(rows(&store).await, saved);
}

#[tokio::test]
async fn independent_sqlite_pools_converge_on_one_original_selection_receipt_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("live.db").display()
    );
    let first = store(&url).await;
    let second = store(&url).await;
    let original = params();
    let (a, b) = tokio::join!(
        first.update_live_context(&original),
        second.update_live_context(&original)
    );
    assert!(a.is_ok() || b.is_ok());
    assert!(first.update_live_context(&original).await.unwrap());
    assert!(second.update_live_context(&original).await.unwrap());
    let saved = rows(&first).await;
    assert_eq!(saved.0.len(), 1);
    assert_eq!(saved.1.len(), 1);
    first.db.close().await.unwrap();
    second.db.close().await.unwrap();
    let reopened = store(&url).await;
    assert_eq!(
        reopened.replay_live_context(&original).await.unwrap(),
        Some(true)
    );
    assert_eq!(rows(&reopened).await, saved);
}

#[tokio::test]
async fn active_turn_even_with_expired_lease_and_stale_preparation_cannot_be_overwritten() {
    let store = store("sqlite::memory:").await;
    let original = params();
    store.update_live_context(&original).await.unwrap();
    let row = rows(&store).await.0.remove(0);
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.turn_state = desk_diagnose_core::session::TurnState::Running;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .col_expr(
            agent_session::Column::LeaseDeadline,
            Expr::value(Some(Utc::now() - Duration::seconds(1))),
        )
        .exec(&store.db)
        .await
        .unwrap();
    let saved = rows(&store).await;
    assert!(store.update_live_context(&clear(&original)).await.is_err());
    assert_eq!(
        store.replay_live_context(&original).await.unwrap(),
        Some(true)
    );
    assert_eq!(rows(&store).await, saved);
}
