// Shared synthetic provider for actual answer capture and cross-chat tools.
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

pub const ANSWER: &str = "Cobaltarchive is the answer-only search marker.";
pub const FOLLOWUP: &str = "Jadefollowup is the second-turn answer-only marker.";

pub struct Gateway {
    pub url: String,
    pub source_id: Arc<Mutex<String>>,
    pub requests: Arc<Mutex<Vec<Value>>>,
    handle: actix_web::dev::ServerHandle,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

pub async fn start() -> Gateway {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let source_id = Arc::new(Mutex::new(String::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let (seen, source) = (requests.clone(), source_id.clone());
    let server = actix_web::HttpServer::new(move || {
        let (seen, source, count) = (seen.clone(), source.clone(), count.clone());
        actix_web::App::new().route("/v1/chat/completions", actix_web::web::post().to(move |body: actix_web::web::Json<Value>| {
            let body = body.into_inner();
            seen.lock().unwrap().push(body.clone());
            let delta = if body["messages"][0]["content"].as_str().is_some_and(|text| text.contains("Create a short conversation title")) {
                json!({"content":r#"{"title":"Find a result"}"#})
            } else {
                match count.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert!(body["messages"].to_string().contains("INTERACTIVE USER HOME"));
                        json!({"content":ANSWER})
                    }
                    1 => tool("search-reply", "search_conversations", json!({"queries":["Cobaltarchive"],"limit":5})),
                    2 => {
                        assert!(body["messages"].as_array().unwrap().iter().any(|message| message["role"] == "tool"
                            && message["tool_call_id"] == "search-reply" && message["content"].to_string().contains("Cobaltarchive")),
                            "search tool messages: {:?}", body["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "tool").collect::<Vec<_>>());
                        tool("read-reply", "read_conversation", json!({"source_id":source.lock().unwrap().clone()}))
                    }
                    3 => {
                        assert!(body["messages"].as_array().unwrap().iter().any(|message| message["role"] == "tool"
                            && message["tool_call_id"] == "read-reply" && message["content"].to_string().contains(ANSWER)),
                            "read tool messages: {:?}", body["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "tool").collect::<Vec<_>>());
                        json!({"content":"Found the previous reply."})
                    }
                    4 => json!({"content":FOLLOWUP}),
                    _ => panic!("unexpected provider retry"),
                }
            };
            let reason = if delta.get("tool_calls").is_some() { "tool_calls" } else { "stop" };
            let first = json!({"choices":[{"delta":delta,"finish_reason":null}]});
            let last = json!({"choices":[{"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":4,"completion_tokens":3}});
            std::future::ready(actix_web::HttpResponse::Ok().content_type("text/event-stream")
                .body(format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")))
        }))
    }).workers(1).disable_signals().listen(listener).unwrap().run();
    let handle = server.handle();
    let task = actix_web::rt::spawn(server);
    Gateway {
        url: format!("http://{address}/v1"),
        source_id,
        requests,
        handle,
        task,
    }
}

/// Keep gateway I/O on the fixture runtime while the complete production call
/// is constructed and polled on the explicitly bounded caller thread.
pub async fn run_on_stack<F, Fut, T>(bytes: Option<usize>, run: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    let Some(bytes) = bytes else {
        return run().await;
    };
    let (sent, done) = tokio::sync::oneshot::channel();
    let child = std::thread::Builder::new()
        .stack_size(bytes)
        .spawn(move || {
            let result = actix_web::rt::System::new().block_on(run());
            assert!(sent.send(result).is_ok());
        })
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), done)
        .await
        .unwrap()
        .unwrap();
    child.join().unwrap();
    result
}

fn tool(id: &str, name: &str, arguments: Value) -> Value {
    json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]})
}

impl Gateway {
    pub async fn stop(self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.handle.stop(false))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
