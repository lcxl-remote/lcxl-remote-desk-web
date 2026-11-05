//! Loopback-only issue transport used by the two hosts' native read tests.

use super::*;
use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use serde_json::json;
use std::{
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;

struct State {
    base: String,
    platform: settings::IssuePlatform,
    typed_responses: AtomicBool,
    invalid_receipt_body: AtomicBool,
    reads: AtomicUsize,
    writes: AtomicUsize,
    pause_next: AtomicBool,
    allow_replies: AtomicBool,
    pause_reply: AtomicBool,
    expire_after_reply: AtomicBool,
    comments: Mutex<Vec<Value>>,
    started: Notify,
    release: Notify,
}

pub struct IssueFixture {
    pub base_url: String,
    pub ca_pem: String,
    state: Arc<State>,
    server: actix_web::dev::ServerHandle,
}

impl IssueFixture {
    pub async fn start() -> Self {
        Self::start_for(settings::IssuePlatform::Gitea).await
    }

    pub async fn start_for(platform: settings::IssuePlatform) -> Self {
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let ca_pem = certificate.cert.pem();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
                certificate.key_pair.serialize_der(),
            )),
        )
        .unwrap();
        let state = Arc::new(State {
            base: base.clone(),
            platform,
            typed_responses: AtomicBool::new(true),
            invalid_receipt_body: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            pause_next: AtomicBool::new(false),
            allow_replies: AtomicBool::new(false),
            pause_reply: AtomicBool::new(false),
            expire_after_reply: AtomicBool::new(false),
            comments: Mutex::new(Vec::new()),
            started: Notify::new(),
            release: Notify::new(),
        });
        let server_state = state.clone();
        let api_prefix = match platform {
            settings::IssuePlatform::Gitea => "/api/v1",
            settings::IssuePlatform::Github => "/api/v3",
        };
        let server = HttpServer::new(move || {
            App::new()
                    .app_data(web::Data::from(server_state.clone()))
                    .route("/mcp", web::post().to(mcp))
                    .service(
                        web::scope(api_prefix)
                            .route(
                                "/user",
                                web::get().to(|| async {
                                    HttpResponse::Ok()
                                        .insert_header(("Mcp-Session-Id", "fixture-session"))
                                        .json(json!({"id":3,"login":"issue-bot"}))
                                }),
                            )
                            .route(
                                "/repos/Org/Project",
                                web::get().to(|| async {
                                    HttpResponse::Ok().json(
                                        json!({"id":42,"full_name":"Org/Project","private":true}),
                                    )
                                }),
                            )
                            .route(
                                "/repos/Org/Project/issues/4/comments",
                                web::get().to(
                                    |state: web::Data<State>,
                                     query: web::Query<
                                        std::collections::HashMap<String, String>,
                                    >| async move {
                                        let page =
                                            query.get("page").unwrap().parse::<usize>().unwrap();
                                        assert_eq!(
                                            query
                                                .get(match state.platform {
                                                    settings::IssuePlatform::Gitea => "limit",
                                                    settings::IssuePlatform::Github => "per_page",
                                                })
                                                .unwrap(),
                                            "30"
                                        );
                                        let comments = state
                                            .comments
                                            .lock()
                                            .unwrap()
                                            .iter()
                                            .skip((page - 1) * issue_connection::COMMENT_PAGE_SIZE)
                                            .take(issue_connection::COMMENT_PAGE_SIZE)
                                            .cloned()
                                            .collect::<Vec<_>>();
                                        HttpResponse::Ok().json(comments)
                                    },
                                ),
                            )
                            .route(
                                "/repos/Org/Project/issues/4",
                                web::get().to(|state: web::Data<State>| async move {
                                    HttpResponse::Ok().json(issue(&state.base))
                                }),
                            )
                            .route(
                                "/repos/Org/Project/issues/comments/{id}",
                                web::get().to(
                                    |req: HttpRequest,
                                     state: web::Data<State>,
                                     id: web::Path<u64>| async move {
                                        assert_eq!(
                                            req.headers().get("user-agent").unwrap(),
                                            "lcxl-remote-desk-issue-client"
                                        );
                                        let mut comment = state
                                            .comments
                                            .lock()
                                            .unwrap()
                                            .iter()
                                            .find(|comment| comment["id"].as_u64() == Some(*id))
                                            .unwrap()
                                            .clone();
                                        comment["issue_url"] = json!(format!(
                                            "{}/api/v3/repos/Org/Project/issues/4",
                                            state.base
                                        ));
                                        if state.invalid_receipt_body.load(Ordering::SeqCst) {
                                            comment["body"] = json!("Different remote content");
                                        }
                                        HttpResponse::Ok().json(comment)
                                    },
                                ),
                            ),
                    )
        })
        .workers(1)
        .listen_rustls_0_23(listener, tls)
        .unwrap()
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);
        Self {
            base_url: base,
            ca_pem,
            state,
            server: handle,
        }
    }

    pub fn update(&self, actor: &str) -> settings::IssueConnectionsUpdate {
        settings::IssueConnectionsUpdate {
            expected_revision: 0,
            connections: vec![settings::IssueConnectionUpdate {
                platform: self.state.platform,
                connection_ref: "forge".into(),
                name: "Fixture Gitea".into(),
                enabled: true,
                instance_url: self.base_url.clone(),
                mcp_url: format!("{}/mcp", self.base_url),
                allowed_actor_ids: vec![actor.into()],
                token: Some("fixture-token".into()),
                extra_ca_pem: Some(self.ca_pem.clone()),
            }],
        }
    }

    pub fn selection(&self) -> desk_agent_protocol::external_tools::IssueSelection {
        desk_agent_protocol::external_tools::IssueSelection {
            connection_ref: "forge".into(),
            owner: "Org".into(),
            repository: "Project".into(),
            issue_number: 4,
        }
    }

    pub fn use_legacy_responses(&self) {
        self.state.typed_responses.store(false, Ordering::SeqCst);
    }
    pub fn invalidate_receipt_body(&self) {
        self.state
            .invalid_receipt_body
            .store(true, Ordering::SeqCst);
    }
    pub fn pause_next_read(&self) {
        self.state.pause_next.store(true, Ordering::SeqCst);
    }
    pub async fn wait_for_read(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.state.started.notified())
            .await
            .unwrap();
    }
    pub fn release_read(&self) {
        self.state.release.notify_one();
    }
    pub fn pause_next_reply(&self) {
        self.state.pause_reply.store(true, Ordering::SeqCst);
    }
    pub fn enable_replies(&self, expire_after_reply: bool) {
        self.state.allow_replies.store(true, Ordering::SeqCst);
        self.state
            .expire_after_reply
            .store(expire_after_reply, Ordering::SeqCst);
    }
    pub fn seed_comment(&self, account_id: u64, body: String) {
        let mut comments = self.state.comments.lock().unwrap();
        let id = 90 + comments.len();
        comments.push(
            json!({"id":id,"body":body,"user":{"id":account_id,"login":"fixture-user"},
            "created_at":"2026-11-04T00:00:00Z","updated_at":"2026-11-04T00:00:00Z"}),
        );
    }
    pub fn reads(&self) -> usize {
        self.state.reads.load(Ordering::SeqCst)
    }
    pub fn writes(&self) -> usize {
        self.state.writes.load(Ordering::SeqCst)
    }
    pub async fn stop(self) {
        self.server.stop(false).await;
    }
}

fn issue(base: &str) -> Value {
    json!({"id":82,"number":4,"title":"Native issue fixture","body":"Reproduce the macOS problem","html_url":format!("{base}/Org/Project/issues/4")})
}

fn tool(name: &str, args: &[(&str, &str)], methods: &[&str]) -> Value {
    let mut properties: serde_json::Map<_, _> = args
        .iter()
        .map(|(key, kind)| ((*key).into(), json!({"type":kind})))
        .collect();
    if !methods.is_empty() {
        properties["method"]["enum"] = json!(methods);
    }
    json!({"name":name,"inputSchema":{"type":"object","properties":properties}})
}

async fn mcp(req: HttpRequest, input: web::Json<Value>, state: web::Data<State>) -> HttpResponse {
    assert_eq!(
        req.headers().get("authorization").unwrap(),
        "Bearer fixture-token"
    );
    let result = match input["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":input["params"]["protocolVersion"],"serverInfo":{"name":"gitea-fixture","version":"1.12.0"},"capabilities":{"tools":{}}})
        }
        "notifications/initialized" => return HttpResponse::Accepted().finish(),
        "tools/list" => {
            let mut write = match state.platform {
                settings::IssuePlatform::Gitea => tool(
                    "issue_write",
                    &[
                        ("method", "string"),
                        ("owner", "string"),
                        ("repo", "string"),
                        ("issue_number", "number"),
                        ("body", "string"),
                    ],
                    &["add_comment"],
                ),
                settings::IssuePlatform::Github => tool(
                    "add_issue_comment",
                    &[
                        ("owner", "string"),
                        ("repo", "string"),
                        ("issue_number", "number"),
                        ("body", "string"),
                    ],
                    &[],
                ),
            };
            if state.platform == settings::IssuePlatform::Github {
                write["inputSchema"]["properties"]["reaction"] =
                    json!({"type":"string","enum":["+1","heart"]});
                write["inputSchema"]["properties"]["comment_id"] = json!({"type":"integer"});
                write["inputSchema"]["required"] = json!(["owner", "repo", "issue_number"]);
            }
            json!({"tools":[tool("get_me",&[],&[]), tool("issue_read",&[("method","string"),("owner","string"),("repo","string"),("issue_number","number")],&["get","get_comments"]),write]})
        }
        "tools/call" => {
            let args = &input["params"]["arguments"];
            let value = match input["params"]["name"].as_str().unwrap() {
                "get_me" => json!({"id":3,"login":"issue-bot"}),
                "issue_read" => {
                    assert_eq!(
                        (
                            args["owner"].as_str(),
                            args["repo"].as_str(),
                            args["issue_number"].as_u64()
                        ),
                        (Some("Org"), Some("Project"), Some(4))
                    );
                    state.reads.fetch_add(1, Ordering::SeqCst);
                    if state.pause_next.swap(false, Ordering::SeqCst) {
                        state.started.notify_one();
                        state.release.notified().await;
                    }
                    match args["method"].as_str().unwrap() {
                        "get" => {
                            let mut value = issue(&state.base);
                            if state.platform == settings::IssuePlatform::Github {
                                value.as_object_mut().unwrap().remove("id");
                                if state.typed_responses.load(Ordering::SeqCst) {
                                    value = json!({"method":"get","issue":value});
                                }
                            }
                            value
                        }
                        "get_comments" => {
                            let mut value = json!([{"id":89,"body":"Steps from an external user","user":"reporter","created_at":"2026-11-04T00:00:00Z"}]);
                            if state.platform == settings::IssuePlatform::Github {
                                value[0]["user"] = json!({"id":7,"login":"reporter"});
                                if state.typed_responses.load(Ordering::SeqCst) {
                                    value = json!({"method":"get_comments","comments":value});
                                }
                            }
                            value
                        }
                        _ => panic!("Unexpected issue read method"),
                    }
                }
                "issue_write" | "add_issue_comment" => {
                    state.writes.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        state.allow_replies.load(Ordering::SeqCst),
                        "Native read tests must not write to the external issue"
                    );
                    assert_eq!(args["owner"], "Org");
                    assert_eq!(args["repo"], "Project");
                    assert_eq!(args["issue_number"], 4);
                    match state.platform {
                        settings::IssuePlatform::Gitea => {
                            assert_eq!(input["params"]["name"], "issue_write");
                            assert_eq!(args["method"], "add_comment");
                            assert_eq!(args.as_object().unwrap().len(), 5);
                        }
                        settings::IssuePlatform::Github => {
                            assert_eq!(input["params"]["name"], "add_issue_comment");
                            assert_eq!(args.as_object().unwrap().len(), 4);
                            assert!(
                                args.get("method").is_none()
                                    && args.get("reaction").is_none()
                                    && args.get("comment_id").is_none()
                            );
                        }
                    }
                    let id = {
                        let mut comments = state.comments.lock().unwrap();
                        let id = 90 + comments.len();
                        comments.push(
                            json!({"id":id,"body":args["body"],"user":{"id":3,"login":"issue-bot"},
                        "created_at":"2026-11-04T00:00:00Z","updated_at":"2026-11-04T00:00:00Z"}),
                        );
                        id
                    };
                    if state.pause_reply.swap(false, Ordering::SeqCst) {
                        state.started.notify_one();
                        state.release.notified().await;
                    }
                    if state.expire_after_reply.load(Ordering::SeqCst) {
                        return HttpResponse::NotFound().finish();
                    }
                    match state.platform {
                        settings::IssuePlatform::Gitea => {
                            json!({"id":id,"body":args["body"],"user":"issue-bot","created_at":"2026-11-04T00:00:00Z"})
                        }
                        settings::IssuePlatform::Github => {
                            json!({"id":id.to_string(),"html_url":format!("{}/Org/Project/issues/4#issuecomment-{id}",state.base)})
                        }
                    }
                }
                _ => panic!("Unexpected tool"),
            };
            if state.platform == settings::IssuePlatform::Github
                && state.typed_responses.load(Ordering::SeqCst)
            {
                json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value})
            } else {
                json!({"content":[{"type":"text","text":value.to_string()}]})
            }
        }
        _ => panic!("Unexpected MCP method"),
    };
    HttpResponse::Ok().json(json!({"jsonrpc":"2.0","id":input["id"],"result":result}))
}

/// Synthetic successful model receipt for transport/lifecycle tests. Production
/// artifacts are frozen only from the host's actual native output store.
pub fn reply_artifact(
    access: desk_diagnose_core::external_tools::IssueAccess,
    comment_tool_signature: String,
) -> desk_diagnose_core::issue_reply::FrozenIssueReply {
    use desk_agent_protocol::data_lineage::DestinationIdentity;
    use desk_diagnose_core::issue_reply::ReplyOrigin;
    let origin = ReplyOrigin {
        actor_id: "owner".into(),
        root_conversation_id: "root".into(),
        request_id: "request-a".into(),
        execution_conversation_id: "issue-work".into(),
        tool_call_id: "publish-1".into(),
        input_revision: 1,
        control_revision: 0,
        authorization_id: "subscription-grant".into(),
        authorization_revision: 1,
    };
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    reply_artifact_for(origin, access, comment_tool_signature, destination)
}

pub fn reply_request_for(
    policy: &desk_diagnose_core::model_egress::ModelEgressPolicy,
    origin: desk_diagnose_core::issue_reply::ReplyOrigin,
    access: desk_diagnose_core::external_tools::IssueAccess,
) -> desk_diagnose_core::seam::ModelRequest {
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::{
        chat::{ChatMessage, ChatRole, ToolCall},
        external_tools,
        input_read_context::ReadContextSelection,
        session::PersistedAgentSession,
    };
    let scope = AgentScope {
        granted: vec![],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut session = PersistedAgentSession::new(
        &origin.execution_conversation_id,
        &origin.actor_id,
        "",
        1,
        scope.clone(),
        "now",
    );
    session.input_revision = origin.input_revision;
    session.control_revision = origin.control_revision;
    let mut original = ReadContextSelection::capture(&[], &scope).unwrap();
    original.external_issues.push(access.clone());
    let call = ToolCall {
        id: "read-issue".into(),
        name: external_tools::TOOL_NAME.into(),
        arguments_json: json!({"tool_ref":access.tool_ref(),"args":{"method":"get"}}).to_string(),
    };
    let content = r#"{"title":"macOS problem"}"#;
    let label = external_tools::label_issue_read(
        &session,
        &original,
        &policy.destination,
        &call,
        &"a".repeat(64),
        content,
    )
    .unwrap();
    let mut material = ChatMessage::text("issue-material", ChatRole::UntrustedOutput, content);
    material.data_envelope = Some(label);
    policy
        .prepare_issue_reply_call(origin, access, vec![material])
        .unwrap()
}

pub fn reply_artifact_for(
    origin: desk_diagnose_core::issue_reply::ReplyOrigin,
    access: desk_diagnose_core::external_tools::IssueAccess,
    comment_tool_signature: String,
    destination: desk_agent_protocol::data_lineage::DestinationIdentity,
) -> desk_diagnose_core::issue_reply::FrozenIssueReply {
    use desk_diagnose_core::{
        chat::ModelTurn,
        issue_reply::FrozenIssueReply,
        model_egress::{
            ModelEgressPolicy, NativeModelOutputEvidence, NativeModelOutputSources,
            project_model_input_lineage,
        },
    };
    let policy = ModelEgressPolicy {
        destination,
        selected_source_tools: Default::default(),
        export_authorization_id: desk_diagnose_core::issue_reply::publication_export_id(
            &origin, &access,
        ),
        now_unix_ms: 1000,
        byte_cap: 1024 * 1024,
        permission_resume: false,
    };
    let request = reply_request_for(&policy, origin.clone(), access.clone());
    let policy = policy.for_request(&request).unwrap();
    let prepared = policy.authorize_request(request).unwrap();
    let turn = ModelTurn {
        text: "The issue is under investigation.".into(),
        ..Default::default()
    };
    let output = policy
        .derive_model_output_envelope(&turn, &prepared.input_envelopes)
        .unwrap();
    let inputs = project_model_input_lineage(&prepared.audit, &prepared.input_envelopes).unwrap();
    let snapshot = prepared.input_snapshot().unwrap();
    let evidence = NativeModelOutputEvidence::from_receipt(
        "physical-call",
        &origin.execution_conversation_id,
        &policy.export_authorization_id,
        1000,
        1001,
        &prepared.audit,
        &inputs,
        &snapshot,
        &output,
    )
    .unwrap();
    FrozenIssueReply::freeze(
        origin,
        access,
        turn.text,
        output,
        NativeModelOutputSources {
            evidence,
            inputs,
            input_snapshot: snapshot,
        },
        comment_tool_signature,
        &[9; 16],
    )
    .unwrap()
}

/// A trusted intake fixture; hosts persist it before accepting the native input.
pub fn admitted_issue_for(
    actor: &str,
    root: &str,
    issue: desk_diagnose_core::external_tools::IssueAccess,
) -> desk_diagnose_core::issue_authority::IssueAdmission {
    use desk_diagnose_core::{
        issue_authority::*,
        native_ids::{self, Kind},
    };
    let target = &issue.target;
    let subscription = IssueSubscription {
        subscription_id: native_ids::stable(Kind::IssueSubscription, &(actor, root)),
        revision: 1,
        actor_id: actor.into(),
        conversation_id: root.into(),
        enabled: true,
        repository: IssueRepositoryScope {
            platform: target.platform,
            connection_ref: issue.connection_ref.clone(),
            instance: target.instance.clone(),
            account_id: target.account_id,
            repository_id: target.repository_id,
            owner: target.owner.clone(),
            repository: target.repository.clone(),
            private: target.private,
            internal: target.internal,
        },
        issue_filter: None,
        allowed_sender_ids: vec![7],
        automation_sender_ids: vec![],
        work_kind: IssueWorkKind::Analysis,
        auto_reply: true,
    };
    IssueAdmission {
        authorization_id: IssueAdmission::id(&subscription, "comment-1"),
        subscription,
        event_id: "comment-1".into(),
        sender_id: 7,
        sender_automation: false,
        echo: false,
        workset: desk_diagnose_core::issue_authority::IssueWorkset {
            title: "macOS problem".into(),
            body: "Reproduce the macOS problem; Linux succeeds".into(),
            comments: vec![desk_diagnose_core::issue_authority::IssueCommentSnapshot {
                id: 1,
                author_id: 7,
                body: "Please investigate".into(),
            }],
        },
        issue,
        request_id: native_ids::stable(Kind::AssistantMainRequest, &(actor, root, "comment-1")),
        execution_root_id: root.into(),
        admitted_at_ms: 0,
        expires_at_ms: None,
    }
}

impl IssueFixture {
    /// A completed publication awaiting its independent delivery, with a real
    /// catalog signature. The host persists its authority and result in its DB.
    pub async fn publication_for(
        &self,
        actor: &str,
        root: &str,
        destination: desk_agent_protocol::data_lineage::DestinationIdentity,
    ) -> (
        desk_diagnose_core::issue_authority::IssueAdmission,
        desk_diagnose_core::issue_reply::FrozenIssueReply,
    ) {
        let base = self.base_url.clone();
        let ca = self.ca_pem.clone();
        let platform = self.state.platform;
        let client = tokio::task::spawn_blocking(move || {
            issue_connection::IssueConnection::new(
                platform,
                &base,
                &format!("{base}/mcp"),
                "fixture-token",
                Some(ca.as_bytes()),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let target = client.bind_issue("Org", "Project", 4).await.unwrap();
        let remote = client.reopen_issue(&target).await.unwrap();
        let access = desk_diagnose_core::external_tools::IssueAccess {
            connection_ref: "forge".into(),
            target,
        };
        let admitted = admitted_issue_for(actor, root, access.clone());
        let artifact = reply_artifact_for(
            desk_diagnose_core::issue_reply::ReplyOrigin {
                actor_id: actor.into(),
                root_conversation_id: root.into(),
                request_id: admitted.request_id.clone(),
                execution_conversation_id: format!("completed-publication-{actor}"),
                tool_call_id: "publish-1".into(),
                input_revision: 1,
                control_revision: 0,
                authorization_id: admitted.authorization_id.clone(),
                authorization_revision: 1,
            },
            access,
            remote.comment_signature().unwrap().into(),
            destination,
        );
        (admitted, artifact)
    }
}
