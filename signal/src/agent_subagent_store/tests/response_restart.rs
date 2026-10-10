//! A separate process reopens a committed response and its consumed child inbox.
use super::*;
use crate::entity::agent_subagent_inbox as inbox;
use desk_diagnose_core::session::TurnState;

async fn snapshot(db: &DatabaseConnection) -> serde_json::Value {
    use sea_orm::QueryOrder;
    serde_json::json!({
        "sessions": session_row::Entity::find().order_by_asc(session_row::Column::Id).all(db).await.unwrap(),
        "inbox": inbox::Entity::find().order_by_asc(inbox::Column::Id).all(db).await.unwrap(),
    })
}

#[tokio::test]
async fn committed_response_and_ack_survive_separate_process_reopen_without_redelivery() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("committed.sqlite");
    let db = database_at(&format!("sqlite://{}?mode=rwc", file.display())).await;
    let mut parent = super::observation::read_report(&db).await;
    super::observation::response(&db, &mut parent, true).await;
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let events = inbox::Entity::find().all(&db).await.unwrap();
    let completed = events.iter().find(|e| e.event_kind == "completed").unwrap();
    assert!(completed.model_observed_at_ms.is_some());
    assert!(completed.interpreted_at_ms.is_some());
    let expected = dir.path().join("snapshot.json");
    std::fs::write(&expected, serde_json::to_vec(&snapshot(&db).await).unwrap()).unwrap();
    db.close().await.unwrap();
    assert!(std::fs::metadata(&file).unwrap().len() > 0);
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(
            concat!(module_path!(), "::response_reopen_worker")
                .split_once("::")
                .unwrap()
                .1,
        )
        .arg("--ignored")
        .arg("--nocapture")
        .env("LRD_RESPONSE_REOPEN_DB", &file)
        .env("LRD_RESPONSE_REOPEN_EXPECTED", &expected)
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    if !output.status.success() {
        eprintln!("Retained failed restart fixture: {}", dir.keep().display());
    }
    assert!(
        output.status.success(),
        "worker failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "worker must actually execute"
    );
}

#[tokio::test]
#[ignore = "launched only by the committed response reopen test"]
async fn response_reopen_worker() {
    let file = std::env::var("LRD_RESPONSE_REOPEN_DB").unwrap();
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::env::var("LRD_RESPONSE_REOPEN_EXPECTED").unwrap()).unwrap(),
    )
    .unwrap();
    assert!(std::fs::metadata(&file).unwrap().len() > 0);
    let db = crate::config::test_support::Database::connect(format!("sqlite://{file}?mode=rw"))
        .await
        .unwrap();
    assert_eq!(snapshot(&db).await, expected);
    let store = SubAgentStore::new(db.clone());
    for _ in 0..2 {
        assert!(
            store
                .parent_runtime_candidate("root", "1", "1")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(snapshot(&db).await, expected);
    }
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let parent = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(
        parent
            .conversation
            .iter()
            .filter(|m| m.message_id == "observation-answer")
            .count(),
        1
    );
    db.close().await.unwrap();
}
