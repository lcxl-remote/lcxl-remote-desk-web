use std::{
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

use super::*;

#[actix_web::test]
async fn github_typed_and_legacy_tools_verify_compact_receipts_without_reposting() {
    use crate::mcp_client::test_support::IssueFixture;
    for legacy in [false, true] {
        let fixture = IssueFixture::start_for(IssuePlatform::Github).await;
        if legacy {
            fixture.use_legacy_responses();
        }
        let connection = IssueConnection::new(
            IssuePlatform::Github,
            &fixture.base_url,
            &format!("{}/mcp", fixture.base_url),
            "fixture-token",
            Some(fixture.ca_pem.as_bytes()),
        )
        .unwrap();
        let probe = connection.test_connection().await.unwrap();
        assert!(probe.can_read_issues && probe.can_reply);
        let target = connection.bind_issue("Org", "Project", 4).await.unwrap();
        assert_eq!(target.platform, IssuePlatform::Github);
        assert_eq!(target.issue_id, 82);
        let remote = connection.reopen_issue(&target).await.unwrap();
        assert_eq!(remote.issue().title, "Native issue fixture");
        assert_eq!(remote.comments().await.unwrap()[0].user, "reporter");
        fixture.enable_replies(false);
        let comment = remote
            .add_comment("Frozen reply", remote.comment_signature().unwrap())
            .await
            .unwrap();
        assert_eq!((comment.id, comment.body.as_str()), (90, "Frozen reply"));
        assert_eq!(fixture.writes(), 1);
        let page = connection
            .comment_page(&target, std::num::NonZeroU32::new(1).unwrap())
            .await
            .unwrap();
        assert_eq!(page[0].body, "Frozen reply");
        let mut changed = target.clone();
        changed.platform = IssuePlatform::Gitea;
        assert!(matches!(
            connection.reopen_issue(&changed).await,
            Err(McpClientError::TargetChanged)
        ));
        fixture.invalidate_receipt_body();
        assert!(
            remote
                .add_comment("Second frozen reply", remote.comment_signature().unwrap())
                .await
                .is_err()
        );
        assert_eq!(fixture.writes(), 2);
        fixture.enable_replies(true);
        assert!(
            remote
                .add_comment("Lost receipt", remote.comment_signature().unwrap())
                .await
                .is_err()
        );
        assert_eq!(fixture.writes(), 3);
        let page = connection
            .comment_page(&target, std::num::NonZeroU32::new(1).unwrap())
            .await
            .unwrap();
        assert_eq!(page[2].body, "Lost receipt");
        assert_eq!(fixture.writes(), 3);
        fixture.stop().await;
    }
}

#[test]
fn github_public_and_enterprise_rest_origins_are_fixed_by_platform() {
    let public = IssueConnection::new(
        IssuePlatform::Github,
        "https://github.com",
        "https://mcp.example/mcp",
        "test-token",
        None,
    )
    .unwrap();
    assert_eq!(
        public.rest_url(&["user"]).unwrap().as_str(),
        "https://api.github.com/user"
    );
    let enterprise = IssueConnection::new(
        IssuePlatform::Github,
        "https://github.example",
        "https://mcp.example/mcp",
        "test-token",
        None,
    )
    .unwrap();
    assert_eq!(
        enterprise.rest_url(&["user"]).unwrap().as_str(),
        "https://github.example/api/v3/user"
    );
}

struct Fixture {
    issue_url: String,
    rest_repository_id: AtomicU64,
    mcp_account_id: AtomicU64,
    writes: AtomicU64,
}

fn tool(name: &str, args: &[(&str, &str)], methods: &[&str]) -> Value {
    let mut properties: serde_json::Map<_, _> = args
        .iter()
        .map(|(name, kind)| ((*name).into(), json!({"type":kind})))
        .collect();
    if !methods.is_empty() {
        properties["method"]["enum"] = json!(methods);
    }
    json!({"name": name, "inputSchema": {"type":"object", "properties": properties}})
}

async fn mcp(
    req: HttpRequest,
    input: web::Json<Value>,
    fixture: web::Data<Fixture>,
) -> HttpResponse {
    assert_eq!(
        req.headers().get("authorization").unwrap(),
        "Bearer test-token"
    );
    let result = match input["method"].as_str().unwrap() {
        "initialize" => json!({
            "protocolVersion":input["params"]["protocolVersion"],
            "serverInfo":{"name":"gitea-mcp-fixture","version":"1.12.0"},
            "capabilities":{"tools":{}}
        }),
        "notifications/initialized" => return HttpResponse::Accepted().finish(),
        "tools/list" => json!({"tools":[
            tool(GET_ME, &[], &[]),
            tool(ISSUE_READ, ISSUE_ARGS, &["get", "get_comments", "download_attachment"]),
            tool(ISSUE_WRITE, COMMENT_ARGS, &["add_comment", "delete_comment"])
        ]}),
        "tools/call" => {
            let args = &input["params"]["arguments"];
            let data = match input["params"]["name"].as_str().unwrap() {
                GET_ME => {
                    json!({"id":fixture.mcp_account_id.load(Ordering::SeqCst), "login":"bot"})
                }
                ISSUE_READ => {
                    assert_eq!(args["owner"], "Org");
                    assert_eq!(args["repo"], "Project");
                    assert_eq!(args["issue_number"], 4);
                    match args["method"].as_str().unwrap() {
                        "get" => {
                            json!({"id":82,"number":4,"title":"A reproducible issue","body":"Original","html_url":fixture.issue_url})
                        }
                        "get_comments" => {
                            json!([{ "id":89,"body":"Reproduction details","user":"tester","created_at":"2026-08-25T00:00:00Z" }])
                        }
                        _ => panic!("unsupported read method"),
                    }
                }
                ISSUE_WRITE => {
                    assert_eq!(
                        args,
                        &json!({
                            "owner":"Org", "repo":"Project", "issue_number":4,
                            "method":"add_comment", "body":"Frozen comment <!-- marker -->"
                        })
                    );
                    fixture.writes.fetch_add(1, Ordering::SeqCst);
                    json!({"id":90,"body":args["body"],"user":"bot","created_at":"2026-08-25T00:00:00Z"})
                }
                _ => panic!("unsupported tool"),
            };
            json!({"content":[{"type":"text","text":data.to_string()}]})
        }
        _ => panic!("unsupported method"),
    };
    HttpResponse::Ok().json(json!({"jsonrpc":"2.0","id":input["id"],"result":result}))
}

#[actix_web::test]
async fn bound_https_issue_checks_identity_signature_and_exact_reply() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let ca = cert.cert.pem();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.cert.der().clone()],
        PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der())),
    )
    .unwrap();
    let fixture = web::Data::new(Fixture {
        issue_url: format!("{base}/Org/Project/issues/4"),
        rest_repository_id: AtomicU64::new(42),
        mcp_account_id: AtomicU64::new(3),
        writes: AtomicU64::new(0),
    });
    let server_fixture = fixture.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(server_fixture.clone())
            .route("/mcp", web::post().to(mcp))
            .route("/api/v1/user", web::get().to(|| async { HttpResponse::Ok().json(json!({"id":3,"login":"bot"})) }))
            .route("/api/v1/repos/Org/Project", web::get().to(|fixture: web::Data<Fixture>| async move {
                HttpResponse::Ok().json(json!({"id":fixture.rest_repository_id.load(Ordering::SeqCst),"full_name":"Org/Project","private":true}))
            }))
            .route("/api/v1/repos/Org/Project/issues/4", web::get().to(|fixture: web::Data<Fixture>| async move {
                HttpResponse::Ok().json(json!({"id":82,"number":4,"html_url":fixture.issue_url}))
            }))
            .route("/api/v1/repos/Org/Project/issues/4/comments", web::get().to(|req: HttpRequest| async move {
                assert_eq!(req.query_string(), "page=2&limit=30");
                HttpResponse::Ok().json(json!([{
                    "id":90,"body":"Frozen comment <!-- marker -->","user":{"id":3,"login":"bot","type":"Bot"},
                    "created_at":"2026-08-25T00:00:00Z","updated_at":"2026-08-25T00:00:00Z"
                }]))
            }))
    })
    .workers(1)
    .listen_rustls_0_23(listener, tls)
    .unwrap()
    .run();
    let handle = server.handle();
    actix_web::rt::spawn(server);

    let untrusted = IssueConnection::new(
        desk_agent_protocol::external_tools::IssuePlatform::Gitea,
        &base,
        &format!("{base}/mcp"),
        "test-token",
        None,
    )
    .unwrap();
    assert_eq!(
        untrusted.bind_issue("Org", "Project", 4).await,
        Err(McpClientError::Unavailable)
    );
    let conn = IssueConnection::new(
        desk_agent_protocol::external_tools::IssuePlatform::Gitea,
        &base,
        &format!("{base}/mcp"),
        "test-token",
        Some(ca.as_bytes()),
    )
    .unwrap();
    let probe = conn.test_connection().await.unwrap();
    assert_eq!(probe.account_id, 3);
    assert_eq!(probe.account_login, "bot");
    assert!(probe.can_read_issues);
    assert!(probe.can_reply);
    let configuration = super::super::settings::IssueConnectionsConfig::default()
        .candidate(&super::super::settings::IssueConnectionsUpdate {
            expected_revision: 0,
            connections: vec![super::super::settings::IssueConnectionUpdate {
                platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
                connection_ref: "forge".into(),
                name: "Fixture".into(),
                enabled: true,
                instance_url: base.clone(),
                mcp_url: format!("{base}/mcp"),
                allowed_actor_ids: vec!["7".into()],
                token: Some("test-token".into()),
                extra_ca_pem: Some(ca.clone()),
            }],
        })
        .unwrap();
    let selected = [desk_agent_protocol::external_tools::IssueSelection {
        connection_ref: "forge".into(),
        owner: "Org".into(),
        repository: "Project".into(),
        issue_number: 4,
    }];
    assert_eq!(
        configuration.bind_issues("8", &selected).await,
        Err(McpClientError::InvalidConfiguration)
    );
    let access = configuration.bind_issues("7", &selected).await.unwrap();
    assert_eq!(access.len(), 1);
    assert!(access[0].matches_selection(&selected[0]));
    assert_eq!(access[0].target.issue_id, 82);
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
    let target = conn.bind_issue("Org", "Project", 4).await.unwrap();
    assert_eq!(
        (target.account_id, target.repository_id, target.issue_id),
        (3, 42, 82)
    );
    let issue = conn.reopen_issue(&target).await.unwrap();
    let read = conn
        .read_issue(
            &target,
            desk_diagnose_core::external_tools::IssueReadMethod::Get,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&read.content).unwrap()["issue"]["id"],
        82
    );
    assert_eq!(read.tool_signature, issue.read_signature());
    let comments = conn
        .read_issue(
            &target,
            desk_diagnose_core::external_tools::IssueReadMethod::GetComments,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&comments.content).unwrap()["comments"][0]["body"],
        "Reproduction details"
    );
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
    assert_eq!(issue.issue().body, "Original");
    assert_eq!(issue.comments().await.unwrap()[0].id, 89);
    let signature = issue.comment_signature().unwrap();
    let body = "Frozen comment <!-- marker -->";
    assert_eq!(
        issue.add_comment(body, "old-schema").await,
        Err(McpClientError::InvalidCatalog)
    );
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
    assert_eq!(issue.add_comment(body, signature).await.unwrap().id, 90);
    let unavailable_mcp = IssueConnection::new(
        desk_agent_protocol::external_tools::IssuePlatform::Gitea,
        &base,
        &format!("{base}/missing-mcp"),
        "test-token",
        Some(ca.as_bytes()),
    )
    .unwrap();
    assert!(matches!(
        unavailable_mcp.reopen_issue(&target).await,
        Err(McpClientError::Unavailable)
    ));
    let page = unavailable_mcp
        .comment_page(&target, 2.try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(
        (page[0].id, page[0].body.as_str(), page[0].user.id),
        (90, body, 3)
    );
    fixture.mcp_account_id.store(8, Ordering::SeqCst);
    assert!(matches!(
        conn.reopen_issue(&target).await,
        Err(McpClientError::TargetChanged)
    ));
    fixture.mcp_account_id.store(3, Ordering::SeqCst);
    fixture.rest_repository_id.store(51, Ordering::SeqCst);
    assert!(matches!(
        conn.reopen_issue(&target).await,
        Err(McpClientError::TargetChanged)
    ));
    assert_eq!(
        conn.comment_page(&target, 2.try_into().unwrap()).await,
        Err(McpClientError::TargetChanged)
    );
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 1);
    handle.stop(false).await;
}

#[actix_web::test]
async fn frozen_reply_is_sent_once_and_a_lost_response_is_reconciled_from_original_comments() {
    use crate::mcp_client::{
        settings::IssueConnectionsConfig,
        test_support::{IssueFixture, reply_artifact},
    };
    use desk_diagnose_core::external_tools::IssueAccess;
    for lost in [false, true] {
        let fixture = IssueFixture::start().await;
        fixture.enable_replies(lost);
        let config = IssueConnectionsConfig::default()
            .candidate(&fixture.update("owner"))
            .unwrap();
        let connection = config.connections[0].clone();
        let client = tokio::task::spawn_blocking(move || connection.client())
            .await
            .unwrap()
            .unwrap();
        let target = client.bind_issue("Org", "Project", 4).await.unwrap();
        let issue = client.reopen_issue(&target).await.unwrap();
        let artifact = reply_artifact(
            IssueAccess {
                connection_ref: "forge".into(),
                target,
            },
            issue.comment_signature().unwrap().into(),
        );
        artifact.authorize_delivery(1002).unwrap();
        if lost {
            // A copied marker by another author and an unmarked same-author body are not receipts.
            fixture.seed_comment(4, artifact.wire_body());
            fixture.seed_comment(3, artifact.body.clone());
            for _ in 2..COMMENT_PAGE_SIZE {
                fixture.seed_comment(4, "Other comment".into());
            }
        }
        let result = issue.deliver_reply(&artifact).await;
        assert_eq!(fixture.writes(), 1);
        if lost {
            assert_eq!(result.unwrap_err(), McpClientError::Unavailable);
            let first = client
                .reconcile_reply_page(
                    &artifact.receipt_key(),
                    std::num::NonZeroU32::new(1).unwrap(),
                )
                .await
                .unwrap();
            assert!(first.receipt.is_none());
            assert_eq!(first.next_page.unwrap().get(), 2);
            let second = client
                .reconcile_reply_page(&artifact.receipt_key(), first.next_page.unwrap())
                .await
                .unwrap();
            let receipt = second.receipt.unwrap();
            assert!(artifact.matches_receipt(receipt.account_id, &receipt.body));
            assert_eq!(receipt.comment_id, 120);
            assert!(second.next_page.is_none());
        } else {
            let receipt = result.unwrap();
            assert!(artifact.matches_receipt(receipt.account_id, &receipt.body));
            assert_eq!(receipt.comment_id, 90);
            let mut altered = artifact.clone();
            altered.comment_tool_signature = "f".repeat(64);
            assert_eq!(
                issue.deliver_reply(&altered).await.unwrap_err(),
                McpClientError::InvalidCatalog
            );
            altered = artifact.clone();
            altered.access.target.issue_id += 1;
            assert_eq!(
                issue.deliver_reply(&altered).await.unwrap_err(),
                McpClientError::TargetChanged
            );
        }
        assert_eq!(fixture.writes(), 1);
        fixture.stop().await;
    }
}
