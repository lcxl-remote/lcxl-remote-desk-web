use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode, ai_assistant::AiAssistantAsk};
use desk_diagnose_core::{external_tools::IssueTarget, input_read_context::ReadContextSelection};
use sea_orm::{Database, DatabaseConnection, Schema, SchemaBuilder};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Reader {
    allowed: AtomicBool,
    reads: AtomicUsize,
}

#[async_trait::async_trait]
impl ExternalIssueReader for Reader {
    async fn permitted(&self, _: &IssueAccess) -> Result<bool, AgentError> {
        Ok(self.allowed.load(Ordering::SeqCst))
    }
    async fn read(
        &self,
        _: &IssueReadScope,
        _: IssueReadMethod,
    ) -> Result<IssueReadOutput, AgentError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(IssueReadOutput {
            content: r#"{"issue":{"title":"macOS regression"}}"#.into(),
            tool_signature: "a".repeat(64),
        })
    }
}

fn access(number: u64) -> IssueAccess {
    IssueAccess {
        connection_ref: "forge".into(),
        target: IssueTarget {
            platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
            instance: "https://forge.test/".into(),
            account_id: 3,
            repository_id: 42,
            owner: "org".into(),
            repository: "project".into(),
            issue_number: number,
            issue_id: 82 + number,
            private: true,
            internal: false,
        },
    }
}

async fn setup() -> (
    DatabaseConnection,
    PersistedAgentSession,
    Batch,
    ModelRequest,
) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::entity::register_schema(SchemaBuilder::new(Schema::new(db.get_database_backend())))
        .sync(&db)
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let mut session = PersistedAgentSession::new_workbench_coordinator(
        "owner",
        "root",
        None,
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        &now.to_rfc3339(),
    )
    .unwrap();
    session.surface = desk_diagnose_core::session::AgentSessionSurface::AiAssistant;
    // The coordinator may be created before it has ever activated a user input.
    assert_eq!(session.input_revision, 0);
    let mut first = None;
    for message_id in ["b", "c"] {
        let issues = if message_id == "b" {
            vec![access(4)]
        } else {
            vec![access(4), access(5)]
        };
        let input = FrozenOwnerInput {
            ask: AiAssistantAsk {
                question: "Investigate the selected issue".into(),
                client_message_id: message_id.into(),
                client_timezone: "UTC".into(),
                selected_issues: issues
                    .iter()
                    .map(
                        |issue| desk_agent_protocol::external_tools::IssueSelection {
                            connection_ref: issue.connection_ref.clone(),
                            owner: issue.target.owner.clone(),
                            repository: issue.target.repository.clone(),
                            issue_number: issue.target.issue_number,
                        },
                    )
                    .collect(),
                ..Default::default()
            },
            device_id: String::new(),
            targets: vec![],
            reads: ReadContextSelection {
                external_issue_sources: Vec::new(),
                external_issues: issues,
                tool_names: vec![],
                expires_at: None,
                object_attachments: vec![],
                live_targets: vec![],
            },
            destination: DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        };
        let row = enqueue(
            &db,
            "owner",
            "root",
            &input,
            None,
            4,
            now.timestamp_millis(),
        )
        .await
        .unwrap();
        if first.is_none() {
            first = Some(row);
        }
    }
    let (batch, request) = reserve(
        &db,
        &mut session,
        &first.unwrap(),
        now.timestamp_millis(),
        vec![],
    )
    .await
    .unwrap()
    .unwrap();
    (db, session, batch, request)
}

fn call(request_id: &str, number: u64, method: &str) -> ToolCall {
    ToolCall {id:"read-issue".into(), name:external_tools::TOOL_NAME.into(),
        arguments_json:serde_json::json!({"request_id":request_id,"tool_ref":access(number).tool_ref(),"args":{"method":method}}).to_string()}
}

#[tokio::test]
async fn reads_bind_the_queued_request_without_advancing_the_active_input() {
    let (db, session, batch, _) = setup().await;
    let reader = Reader {
        allowed: AtomicBool::new(true),
        reads: AtomicUsize::new(0),
    };
    let tool = batch.issue_tool(&db, &reader).await.unwrap().unwrap();
    assert!(tool.description.contains(&batch.items[0].request_id));
    assert!(tool.description.contains(&batch.items[1].request_id));
    let b = batch
        .issue_call(&db, &reader, &call(&batch.items[0].request_id, 4, "get"))
        .await
        .unwrap()
        .unwrap();
    let c = batch
        .issue_call(&db, &reader, &call(&batch.items[1].request_id, 4, "get"))
        .await
        .unwrap()
        .unwrap();
    assert!(external_tools::is_issue_read_label(&b.label));
    assert_ne!(b.label.envelope_id, c.label.envelope_id);
    assert_eq!(b.label.allowed_destinations, c.label.allowed_destinations);
    assert_eq!(b.content, c.content);
    assert_eq!(session.input_revision, 0);
    assert_eq!(session.automation_turns_used, 0);
    assert!(batch.is_current(&session, chrono::Utc::now().timestamp_millis()));
    for invalid in [
        call(&batch.items[0].request_id, 4, "add_comment"),
        call(&batch.items[0].request_id, 5, "get"),
        call("another-request", 4, "get"),
    ] {
        assert!(
            batch
                .issue_call(&db, &reader, &invalid)
                .await
                .unwrap()
                .is_none()
        );
    }
    let mut forged = call(&batch.items[0].request_id, 4, "get");
    let mut args: serde_json::Value = serde_json::from_str(&forged.arguments_json).unwrap();
    args["args"]["body"] = "write".into();
    forged.arguments_json = args.to_string();
    assert!(
        batch
            .issue_call(&db, &reader, &forged)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(reader.reads.load(Ordering::SeqCst), 2);
    cancel_owner_request(
        &db,
        "owner",
        "root",
        &b.source.request_id,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .unwrap();
    assert!(!batch.issue_source_current(&db, &b.source).await.unwrap());
    assert!(batch.issue_source_current(&db, &c.source).await.unwrap());
    assert!(
        batch
            .issue_call(&db, &reader, &call(&b.source.request_id, 4, "get"))
            .await
            .unwrap()
            .is_none()
    );
    reader.allowed.store(false, Ordering::SeqCst);
    assert!(batch.issue_tool(&db, &reader).await.unwrap().is_none());
    assert!(
        batch
            .issue_call(&db, &reader, &call(&c.source.request_id, 4, "get"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(reader.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_edit_to_the_frozen_request_invalidates_the_result_source() {
    let (db, _, batch, _) = setup().await;
    let reader = Reader {
        allowed: AtomicBool::new(true),
        reads: AtomicUsize::new(0),
    };
    let result = batch
        .issue_call(&db, &reader, &call(&batch.items[0].request_id, 4, "get"))
        .await
        .unwrap()
        .unwrap();
    let row = request::Entity::find_by_id(batch.items[0].id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut input = FrozenOwnerInput::decode(&row).unwrap();
    input.reads.external_issues[0].target.issue_id += 1;
    request::Entity::update_many()
        .filter(request::Column::Id.eq(row.id))
        .col_expr(
            request::Column::PendingInputJson,
            sea_orm::sea_query::Expr::value(serde_json::to_string(&input).unwrap()),
        )
        .exec(&db)
        .await
        .unwrap();
    assert!(
        !batch
            .issue_source_current(&db, &result.source)
            .await
            .unwrap()
    );
    assert!(
        batch
            .issue_call(&db, &reader, &call(&batch.items[0].request_id, 4, "get"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(reader.reads.load(Ordering::SeqCst), 1);
}
