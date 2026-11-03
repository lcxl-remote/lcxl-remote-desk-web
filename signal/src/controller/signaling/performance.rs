//! Explicit measurement of the real HTTP authentication boundary, without a
//! WebSocket upgrade or device registration. This is not a media benchmark.
use super::*;
use desk_signal_facade::model::peer::{
    HostExecCapabilities, PeerInfo, ServerRegistration, SignalingPeer,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Validator(Arc<AtomicUsize>);
impl NodeTokenValidator for Validator {
    fn validate_node_token<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { token == "benchmark-fictional-credential" })
    }
}

#[actix_web::test]
#[ignore = "Explicit release performance comparison; no network or real credentials"]
async fn signaling_authentication_performance() {
    const WARMUP: usize = 100_000;
    const SAMPLES: usize = 100_000;
    let calls = Arc::new(AtomicUsize::new(0));
    let validator: Arc<dyn NodeTokenValidator> = Arc::new(Validator(calls.clone()));
    let connections = web::Data::new(SharedConnectionMap::new());
    let app = actix_web::test::init_service(
        actix_web::App::new()
            .app_data(connections.clone())
            .app_data(web::Data::new(validator))
            .service(open_signaling_handle),
    )
    .await;
    let vi = SignalingPeer {
        version_info: Some(VersionInfo {
            api_version: 1,
            build_number: 0,
            commit_hash: "benchmark".into(),
            debug_build: cfg!(debug_assertions),
        }),
        peer: PeerInfo {
            remote_desk_type: RemoteDeskTypeEnum::Server,
            operation_system: Default::default(),
            display_name: Some("fixture".into()),
        },
        server: (Some("benchmark-host".into())).map(|client_id| ServerRegistration {
            client_id,
            exec_capabilities: HostExecCapabilities {
                available_exec_shells: Vec::new(),
                exec_pty: false,
                exec_pty_elevation: false,
                max_ai_command_runtime_ms: Some(
                    desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS,
                ),
            },
            ai_assistant_settings: desk_agent_protocol::ai_assistant::AiAssistantSettings {
                revision: 0,
                enabled: false,
            },
        }),
    };
    let query = format!(
        "{}&token=benchmark-fictional-credential",
        vi.to_query_string().unwrap()
    );
    let uri = format!("/api/desk/signaling?{query}");
    let mut nanos = Vec::with_capacity(SAMPLES);
    let start = std::time::Instant::now();
    for i in 0..WARMUP + SAMPLES {
        let request = actix_web::test::TestRequest::get().uri(&uri).to_request();
        let began = std::time::Instant::now();
        let response = actix_web::test::call_service(&app, request).await;
        assert_eq!(response.status(), actix_web::http::StatusCode::BAD_REQUEST);
        actix_web::test::read_body(response).await;
        if i >= WARMUP {
            nanos.push(began.elapsed().as_nanos() as u64);
        }
    }
    let elapsed = start.elapsed().as_micros();
    nanos.sort_unstable();
    assert_eq!(calls.load(Ordering::Relaxed), WARMUP + SAMPLES);
    assert!(connections.read().await.is_empty());
    println!(
        "SIGNALING_AUTH_BENCH {}",
        serde_json::json!({"samples": SAMPLES, "warmup": WARMUP, "p50_ns": nanos[SAMPLES/2], "p95_ns": nanos[SAMPLES*95/100], "p99_ns": nanos[SAMPLES*99/100], "mean_ns": nanos.iter().sum::<u64>() / SAMPLES as u64, "total_us_including_warmup": elapsed, "validator_calls": WARMUP + SAMPLES, "registered_connections": 0})
    );
}
