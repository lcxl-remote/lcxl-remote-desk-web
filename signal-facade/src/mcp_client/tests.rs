use std::{
    net::TcpListener,
    sync::atomic::{AtomicUsize, Ordering},
};

use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use serde_json::json;

use super::*;

#[derive(Default)]
struct Calls {
    initialize: AtomicUsize,
    write: AtomicUsize,
}

async fn mcp(req: HttpRequest, input: web::Json<Value>, calls: web::Data<Calls>) -> HttpResponse {
    assert_eq!(
        req.headers().get("authorization").unwrap(),
        "Bearer test-token"
    );
    let id = &input["id"];
    let response = |result| json!({"jsonrpc":"2.0", "id":id, "result":result});
    let tool = |name| json!({"name":name,"inputSchema":{"type":"object"}});
    let method = input["method"].as_str().unwrap();
    if method != "initialize" {
        assert_eq!(req.headers().get("mcp-session-id").unwrap(), "test-session");
    }
    match method {
        "initialize" => {
            calls.initialize.fetch_add(1, Ordering::SeqCst);
            HttpResponse::Ok()
                .insert_header(("mcp-session-id", "test-session"))
                .json(response(json!({
                    "protocolVersion":input["params"]["protocolVersion"],
                    "serverInfo":{"name":"issue-fixture", "version":"1"},
                    "capabilities":{"tools":{}}
                })))
        }
        "notifications/initialized" => HttpResponse::Accepted().finish(),
        "tools/list" if input["params"]["cursor"].is_null() => {
            HttpResponse::Ok().json(response(json!({
                "tools":[tool("issue_read")], "nextCursor":"second-page"
            })))
        }
        "tools/list" => {
            assert_eq!(input["params"]["cursor"], "second-page");
            let result = response(json!({"tools":[tool("issue_write")]}));
            HttpResponse::Ok()
                .content_type("text/event-stream")
                .body(format!("data: {result}\n\n"))
        }
        "tools/call" => match input["params"]["name"].as_str().unwrap() {
            "issue_read" => HttpResponse::Ok().json(response(json!({
                "content":[{"type":"text","text":"Original issue"}]
            }))),
            "issue_write" => {
                calls.write.fetch_add(1, Ordering::SeqCst);
                HttpResponse::NotFound().finish()
            }
            "large" => {
                let prefix = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\""
                );
                let body = std::iter::once(bytes::Bytes::from(prefix))
                    .chain((0..17).map(|_| bytes::Bytes::from(vec![b'x'; 64 * 1024])))
                    .chain(std::iter::once(bytes::Bytes::from_static(b"\"}]}}")));
                HttpResponse::Ok()
                    .content_type("application/json")
                    .streaming(futures_util::stream::iter(
                        body.map(Ok::<_, std::io::Error>),
                    ))
            }
            _ => panic!("unexpected fixture tool"),
        },
        _ => panic!("unexpected MCP fixture method {method}"),
    }
}

#[actix_web::test]
async fn streamable_http_lists_reads_and_never_replays_a_lost_write() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = web::Data::new(Calls::default());
    let server_calls = calls.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(server_calls.clone())
            .route("/mcp", web::post().to(mcp))
            .route(
                "/mcp",
                web::get().to(|| async { HttpResponse::MethodNotAllowed().finish() }),
            )
            .route(
                "/redirect",
                web::post().to(|| async {
                    HttpResponse::TemporaryRedirect()
                        .insert_header(("location", "/mcp"))
                        .finish()
                }),
            )
    })
    .workers(1)
    .listen(listener)
    .unwrap()
    .run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let endpoint = |path| McpEndpoint {
        url: Url::parse(&format!("{base}{path}")).unwrap(),
        bearer_token: "test-token".into(),
        client: http::client(None).unwrap(),
    };
    assert!(matches!(
        endpoint("/redirect").open().await,
        Err(McpClientError::Unavailable)
    ));
    assert_eq!(calls.initialize.load(Ordering::SeqCst), 0);
    let endpoint = endpoint("/mcp");
    let client = endpoint.open().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        ["issue_read", "issue_write"]
    );
    let result = client
        .call_tool("issue_read".into(), JsonObject::new())
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "Original issue");
    assert_eq!(
        client
            .call_tool("issue_write".into(), JsonObject::new())
            .await,
        Err(McpClientError::Unavailable)
    );
    drop(client);
    assert_eq!(calls.initialize.load(Ordering::SeqCst), 1);
    assert_eq!(calls.write.load(Ordering::SeqCst), 1);

    let fresh = endpoint.open().await.unwrap();
    assert_eq!(
        fresh.call_tool("large".into(), JsonObject::new()).await,
        Err(McpClientError::Unavailable)
    );
    assert_eq!(calls.write.load(Ordering::SeqCst), 1);
    handle.stop(false).await;
}

#[test]
fn endpoint_accepts_only_configured_https_without_url_credentials() {
    assert!(endpoint_url("https://forge.internal:9443/mcp").is_ok());
    for url in [
        "http://forge.internal/mcp",
        "https://user:secret@forge.internal/mcp",
        "https://forge.internal/mcp?token=secret",
        "https://forge.internal/mcp#other",
    ] {
        assert_eq!(endpoint_url(url), Err(McpClientError::InvalidConfiguration));
    }
    assert!(matches!(
        McpEndpoint::new("https://forge.internal/mcp", "bad token", None),
        Err(McpClientError::InvalidConfiguration)
    ));
}
