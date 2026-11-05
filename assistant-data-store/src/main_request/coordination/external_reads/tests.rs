use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode, ai_assistant::AiAssistantAsk};
use desk_diagnose_core::{external_tools::IssueTarget, input_read_context::ReadContextSelection};
use sea_orm::{Database, DatabaseConnection, Schema, SchemaBuilder};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Reader {
    allowed: AtomicBool,
    reads: AtomicUsize,
}

struct SkillRoutingModel {
    policy: desk_diagnose_core::model_egress::ModelEgressPolicy,
    requests: std::cell::RefCell<Vec<ModelRequest>>,
}

#[async_trait::async_trait(?Send)]
impl desk_diagnose_core::seam::ModelSeam for SkillRoutingModel {
    fn model_output_token_limit(&self, _: &ModelRequest) -> Result<i64, AgentError> {
        Ok(1024)
    }
    async fn context_policy(
        &self,
        _: desk_diagnose_core::model_capability::ModelRequirements,
    ) -> Result<desk_diagnose_core::model_context::PinnedContextPolicy, AgentError> {
        Err(desk_diagnose_core::subagent::invalid(
            "Unexpected context-policy lookup in routing seam",
        ))
    }
    async fn call(
        &self,
        request: ModelRequest,
        _: &mut dyn desk_diagnose_core::seam::TurnSink,
    ) -> Result<desk_diagnose_core::chat::ModelTurn, AgentError> {
        use desk_diagnose_core::{
            chat::{ModelTurn, StopReason},
            model_profile::WireProtocol,
            replay::{ProviderResponseMeta, ReplayDisposition, SourceContextKey},
        };
        let first = self.requests.borrow().is_empty();
        let authorized = self
            .policy
            .authorize_request(request.clone())
            .map_err(|error| error.agent_error())?;
        self.requests.borrow_mut().push(request);
        let mut turn = if first {
            ModelTurn { stop_reason: StopReason::ToolUse, tool_calls: vec![
                ToolCall { id: "load-skill".into(), name: desk_diagnose_core::skills::LOAD_TOOL.into(), arguments_json: serde_json::json!({"skill_ref":"issue-analysis"}).to_string() },
                ToolCall { id: "load-reference".into(), name: desk_diagnose_core::skills::LOAD_TOOL.into(), arguments_json: serde_json::json!({"skill_ref":"issue-analysis","resource":"references/checklist.md"}).to_string() },
            ], provider_meta: ProviderResponseMeta { stop_reason: StopReason::ToolUse,
                replay: Some(ReplayDisposition::NotRequired { source_context_key: SourceContextKey::derive(WireProtocol::OpenAiChatCompletions, "test", "test", "test") }), ..Default::default() }, ..Default::default() }
        } else {
            ModelTurn {
                text: "routed".into(),
                ..Default::default()
            }
        };
        turn.provider_meta.data_envelope = Some(
            self.policy
                .derive_model_output_envelope(&turn, &authorized.input_envelopes)
                .map_err(|error| error.agent_error())?,
        );
        Ok(turn)
    }
}

#[tokio::test]
async fn batch_skill_load_projects_first_party_text_and_rechecks_its_original_lease() {
    for revoke in [false, true] {
        let (db, _, batch, request) = setup().await;
        let model = SkillRoutingModel {
            policy: desk_diagnose_core::model_egress::ModelEgressPolicy {
                destination: batch.destination.clone(),
                selected_source_tools: Default::default(),
                export_authorization_id: batch.id.clone(),
                now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
                byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
                permission_resume: false,
            },
            requests: Default::default(),
        };
        let checks = AtomicUsize::new(0);
        let reader = Reader {
            allowed: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
        };
        let result = batch
            .route(
                &model,
                &db,
                request,
                async || {
                    let index = checks.fetch_add(1, Ordering::SeqCst);
                    if revoke && index >= 2 {
                        return Err(desk_diagnose_core::subagent::invalid(
                            "Original lease revoked",
                        ));
                    }
                    Ok(BTreeMap::new())
                },
                async |_, _, _, _| Ok(None),
                RequestReadPorts {
                    devices: async |_: &request::Model, _: &FrozenOwnerInput| Ok(None),
                    issues: &reader,
                },
            )
            .await;
        assert_eq!(result.is_err(), revoke);
        assert_eq!(checks.load(Ordering::SeqCst), 3);
        let requests = model.requests.borrow();
        assert_eq!(requests.len(), if revoke { 1 } else { 2 });
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == desk_diagnose_core::skills::LOAD_TOOL)
        );
        assert!(
            !requests[0]
                .messages
                .iter()
                .any(|message| message.text.contains("# Issue analysis"))
        );
        if !revoke {
            assert!(requests[1].tools.is_empty());
            assert!(
                requests[1]
                    .messages
                    .iter()
                    .any(|message| message.role == ChatRole::System
                        && message.text.contains("# Issue analysis"))
            );
            assert!(
                requests[1]
                    .messages
                    .iter()
                    .any(|message| message.role == ChatRole::UntrustedOutput
                        && message.text.contains("# Investigation and reply checklist"))
            );
        }
        assert_eq!(reader.reads.load(Ordering::SeqCst), 0);
    }
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
