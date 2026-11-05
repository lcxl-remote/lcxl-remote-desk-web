use super::*;
use desk_assistant_data_store::{entity::agent_issue_reply as reply, issue_reply};
use desk_diagnose_core::{
    external_tools::{IssueAccess, IssueTarget},
    issue_reply::ReplyOrigin,
};

pub(super) async fn seed(db: &DatabaseConnection, execution: &str, root: &str) -> reply::Model {
    seed_for_input(db, execution, root, 1).await
}

async fn seed_for_input(
    db: &DatabaseConnection,
    execution: &str,
    root: &str,
    input_revision: u64,
) -> reply::Model {
    let artifact = desk_signal_facade::mcp_client::test_support::reply_artifact_for(
        ReplyOrigin {
            actor_id: "1".into(),
            root_conversation_id: root.into(),
            request_id: format!("request-{execution}"),
            execution_conversation_id: execution.into(),
            tool_call_id: "publish-1".into(),
            input_revision,
            control_revision: 0,
            authorization_id: "original-grant".into(),
            authorization_revision: 1,
        },
        IssueAccess {
            connection_ref: "forge".into(),
            target: IssueTarget {
                platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
                instance: "https://forge.test/".into(),
                account_id: 3,
                repository_id: 42,
                owner: "org".into(),
                repository: "project".into(),
                issue_number: 4,
                issue_id: 82,
                private: false,
                internal: false,
            },
        },
        "b".repeat(64),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    );
    issue_reply::freeze(db, &artifact, issue_reply::DeliveryMode::Automatic, 100)
        .await
        .unwrap()
}

pub(super) async fn read(db: &DatabaseConnection, original: &reply::Model) -> reply::Model {
    reply::Entity::find_by_id(original.id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn native_idle_retention_removes_publication_content() {
    let db = database().await;
    seed_parent(&db).await;
    let original = seed(&db, "root", "root").await;
    let unrelated = seed(&db, "other-chat", "other-chat").await;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let (_, deleted) =
        crate::usage_retention::cleanup_agent_sessions(&db, cutoff, chrono::Utc::now())
            .await
            .unwrap();
    assert_eq!(deleted, 1);
    let removed = read(&db, &original).await;
    assert_eq!(removed.state, issue_reply::NOT_SENT);
    assert!(removed.artifact_json.is_none());
    assert!(removed.retired_at_ms.is_some());
    assert_eq!(read(&db, &unrelated).await, unrelated);
}

#[tokio::test]
async fn native_request_stop_keeps_a_prior_delivery_from_the_same_executor() {
    let db = database().await;
    seed_parent(&db).await;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut held = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    held.input_revision = 2;
    held.focus_epoch.input_revision = 2;
    held.latest_input_seq = 2;
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            sea_orm::sea_query::Expr::value(held.encode_json_for_storage().unwrap()),
        )
        .filter(session_row::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();
    let earlier = seed_for_input(&db, "root", "root", 1).await;
    let current = seed_for_input(&db, "root", "root", 2).await;
    let txn = db.begin().await.unwrap();
    super::super::native_cancel::cancel_native_actions_on(&txn, &held, "current-request-stop", 200)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(read(&db, &earlier).await, earlier);
    let stopped = read(&db, &current).await;
    assert_eq!(stopped.state, issue_reply::NOT_SENT);
    assert_eq!(stopped.artifact_json, current.artifact_json);
    assert_eq!(stopped.retired_at_ms, None);
}

#[tokio::test]
async fn explicit_stop_all_cancels_delivery_after_worker_execution_finished() {
    use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};
    let db = database().await;
    let (parent, _) = super::creation::runnable_parent(&db).await;
    let completed = seed(&db, "completed-executor", "root").await;
    let other = seed(&db, "other-executor", "other-root").await;
    let store = SubAgentStore::new(db.clone());
    let control = AiAssistantStopControl {
        client_request_id: "stop-completed-delivery".into(),
        expected_input_revision: parent.input_revision,
        expected_control_revision: parent.control_revision,
        subagent_choice: Some(SubAgentStopChoice::IncludeSubAgents),
    };
    let first = store
        .stop_for_owner("root", "1", "1", &control)
        .await
        .unwrap();
    let stopped = read(&db, &completed).await;
    assert_eq!(stopped.state, issue_reply::NOT_SENT);
    assert_eq!(stopped.artifact_json, completed.artifact_json);
    assert_eq!(read(&db, &other).await, other);
    assert_eq!(
        store
            .stop_for_owner("root", "1", "1", &control)
            .await
            .unwrap(),
        first
    );
    assert_eq!(read(&db, &completed).await, stopped);
}
