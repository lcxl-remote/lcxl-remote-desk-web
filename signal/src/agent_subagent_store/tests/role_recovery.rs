//! Raw orchestration writes remain forbidden across actual checkpoint and recovery boundaries.
use super::*;
use desk_diagnose_core::{
    chat::ToolCall,
    session::{TriggerOrigin, TurnState},
    subagent::{AgentRole, tools},
};
use serde_json::json;

async fn reject_writes(db: &DatabaseConnection, id: &str) {
    let original = super::paused_permission::session(db, id).await;
    assert!(!original.agent_role.is_main());
    let before = original.encode_json_for_storage().unwrap();
    let runs = run_row::Entity::find().all(db).await.unwrap();
    let grants = grant_row::Entity::find().all(db).await.unwrap();
    let goals = goal_row::Entity::find().all(db).await.unwrap();
    let proposals = open_row::Entity::find().all(db).await.unwrap();
    let schedules = schedule_row::Entity::find().all(db).await.unwrap();
    for spoof in [false, true] {
        let mut held = PersistedAgentSession::decode_json(&before).unwrap();
        if spoof {
            held.agent_role = AgentRole::Main;
            held.trigger_origin = TriggerOrigin::User;
        }
        let request = super::creation::spawn_request();
        let call = ToolCall {
            id: "forbidden-nested-spawn".into(),
            name: tools::SPAWN.into(),
            arguments_json: serde_json::to_string(&request).unwrap(),
        };
        assert!(
            SubAgentStore::new(db.clone())
                .spawn_for_turn(&held, &call, &request)
                .await
                .is_err()
        );
        for kind in ["fresh_task", "conversation_resume"] {
            let mut scheduled = held.clone();
            let call = ToolCall { id: format!("forbidden-{kind}"),
                name: desk_diagnose_core::schedule::proposal::REQUEST_SCHEDULE.into(),
                arguments_json: json!({"kind":kind,"title":"Forbidden child timer",
                    "prompt":"Continue the delegated task", "rule":{"kind":"after_confirmation","delay_seconds":60}}).to_string() };
            if spoof {
                // Valid input must reach the persisted-state comparison, not fail JSON parsing.
                assert!(
                    desk_diagnose_core::schedule::management_tools::parse(&scheduled, &call)
                        .is_ok()
                );
            }
            let result = ScheduleStore::new(db.clone())
                .manage_from_session(&mut scheduled, &call)
                .await;
            if spoof {
                assert!(matches!(result, Err(ScheduleStoreError::Conflict)));
            } else {
                assert!(matches!(result, Err(ScheduleStoreError::Invalid)));
            }
        }
        let event = super::goal_role_guard::proposal(&mut held);
        let error = goal_open::save_model_request(db, &mut held, &event)
            .await
            .unwrap_err();
        if spoof {
            assert_eq!(error.message, "stored child session cannot propose a goal");
        }
    }
    assert_eq!(
        super::paused_permission::session(db, id)
            .await
            .encode_json_for_storage()
            .unwrap(),
        before
    );
    assert_eq!(run_row::Entity::find().all(db).await.unwrap(), runs);
    assert_eq!(grant_row::Entity::find().all(db).await.unwrap(), grants);
    assert_eq!(goal_row::Entity::find().all(db).await.unwrap(), goals);
    assert_eq!(open_row::Entity::find().all(db).await.unwrap(), proposals);
    assert_eq!(
        schedule_row::Entity::find().all(db).await.unwrap(),
        schedules
    );
}

#[tokio::test]
async fn raw_child_writes_stay_forbidden_after_compaction_permission_resume_and_process_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("roles.sqlite");
    let db = database_at(&format!("sqlite://{}?mode=rwc", file.display())).await;
    super::input_sources::add_input_tables(&db).await;
    for mut statement in [
        Schema::new(db.get_database_backend()).create_table_from_entity(grant_row::Entity),
        Schema::new(db.get_database_backend()).create_table_from_entity(schedule_row::Entity),
    ] {
        db.execute(statement.if_not_exists()).await.unwrap();
    }
    let mut child = super::lifecycle::child(&db).await;
    child.policy_revision =
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
    save_child_session(&db, &mut child).await.unwrap();
    reject_writes(&db, &child.conversation_id).await;
    super::compaction_races::add_history(&mut child);
    save_child_session(&db, &mut child).await.unwrap();
    let (plan, summary) = super::compaction_races::plan(&child);
    super::compaction_races::apply(&mut child, &plan, summary);
    save_child_session(&db, &mut child).await.unwrap();
    reject_writes(&db, &child.conversation_id).await;
    let event = super::paused_permission::request(&mut child);
    super::paused_permission::publish(&db, &mut child, &event).await;
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_child_session(&db, &mut child).await.unwrap();
    super::paused_permission::approve(&db, &child.conversation_id).await;
    super::paused_permission::assert_claim(&db, &child.conversation_id, true).await;
    let recovered = super::paused_permission::session(&db, &child.conversation_id).await;
    assert_eq!(recovered.trigger_origin, TriggerOrigin::PermissionDecision);
    assert!(
        recovered
            .model_context_state
            .entries
            .iter()
            .any(|entry| entry.checkpoint.is_some())
    );
    reject_writes(&db, &child.conversation_id).await;
    db.close().await.unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(
            concat!(module_path!(), "::role_reopen_worker")
                .split_once("::")
                .unwrap()
                .1,
        )
        .arg("--ignored")
        .arg("--nocapture")
        .env("LRD_ROLE_REOPEN_DB", &file)
        .env("LRD_ROLE_REOPEN_CHILD", &child.conversation_id)
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    if !output.status.success() {
        eprintln!("Retained failed role fixture: {}", dir.keep().display());
    }
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[tokio::test]
#[ignore = "launched by the child role recovery test only"]
async fn role_reopen_worker() {
    let file = std::env::var("LRD_ROLE_REOPEN_DB").unwrap();
    let id = std::env::var("LRD_ROLE_REOPEN_CHILD").unwrap();
    let db = Database::connect(format!("sqlite://{file}?mode=rw"))
        .await
        .unwrap();
    let session = super::paused_permission::session(&db, &id).await;
    assert_eq!(session.trigger_origin, TriggerOrigin::PermissionDecision);
    assert!(
        session
            .model_context_state
            .entries
            .iter()
            .any(|entry| entry.checkpoint.is_some())
    );
    reject_writes(&db, &id).await;
    db.close().await.unwrap();
}

use crate::entity::{
    agent_capability_grant as grant_row, agent_goal_open_request as open_row,
    agent_schedule as schedule_row,
};
use crate::{
    agent_goal_open_store as goal_open,
    schedule_store::{ScheduleStore, ScheduleStoreError},
};
