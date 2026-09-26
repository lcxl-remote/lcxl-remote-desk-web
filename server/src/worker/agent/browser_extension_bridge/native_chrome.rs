//! Explicit native Chrome test using the production WebSocket handler and broker.
use super::*;
use desk_agent_protocol::browser_control::{BrowserActionRequest, BrowserOriginKind};
use futures_util::FutureExt;
use std::{panic::AssertUnwindSafe, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[actix_web::test]
#[ignore = "requires native Chrome, Node 22 and LCXL_BROWSER_RUNTIME_PEER; run exact test with external timeout"]
async fn native_chrome_authenticates_and_round_trips_form_actions() {
    let started = std::time::Instant::now();
    let phase = |name: &str| {
        println!(
            "native_chrome phase={name} elapsed_ms={}",
            started.elapsed().as_millis()
        )
    };
    let script = std::env::var_os("LCXL_BROWSER_RUNTIME_PEER")
        .expect("set LCXL_BROWSER_RUNTIME_PEER to the explicit native Chrome peer script");
    let root = private_test_directory();
    let bound = endpoint::bind(root.path(), "native-fixture").unwrap();
    let broker = Arc::new(BrowserExtensionBroker::default());
    let identity = tokio::time::timeout(
        Duration::from_secs(5),
        crate::worker::agent::linux_desktop::resolve(),
    )
    .await
    .expect("GNOME identity resolution timeout")
    .expect("requires a trusted logged-in GNOME Wayland user session");
    let binding = identity.binding();
    broker.set_linux_session_binding(Some(binding.clone()));
    let state = BrowserExtensionEndpointState {
        broker: Arc::clone(&broker),
        pairing_token: Arc::new(load_or_create_pairing_token(root.path()).unwrap()),
        device_id: Arc::new("native-fixture".into()),
        os_session_id: Arc::new(binding),
    };
    let secret = state.pairing_token.as_ref().clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .route("/browser-extension/v2", web::get().to(extension_ws_handler))
    })
    .workers(1)
    .disable_signals()
    .listen(bound.listener)
    .unwrap()
    .run();
    let handle = server.handle();
    let running = actix_web::rt::spawn(server);
    let mut child = tokio::process::Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let outcome = AssertUnwindSafe(async {
        let configuration = serde_json::json!({
            "endpoint": pairing_endpoint(root.path(), "native-fixture").unwrap(),
            "secret": secret,
            "extension_path": Path::new(env!("CARGO_MANIFEST_DIR")).join("../browser-extension")
        });
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{configuration}\n").as_bytes())
            .await
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(20), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ready["ready"], true);
        phase("peer_ready");
        let port = u16::try_from(ready["port"].as_u64().unwrap()).unwrap();
        let surface = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(surface) = broker.surface_ref() {
                    break surface;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        phase("authenticated");
        let request = |id: &str, action| BrowserActionRequest {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: id.into(),
            action,
        };
        let opened = broker
            .execute(
                &surface,
                &request(
                    "native-open",
                    BrowserAction::OpenPage {
                        target: BrowserNavigationTarget {
                            url: format!("http://127.0.0.1:{port}/form"),
                            origin: BrowserOrigin {
                                kind: BrowserOriginKind::HttpLoopback,
                                host_ascii: "127.0.0.1".into(),
                                port,
                            },
                        },
                    },
                ),
            )
            .await
            .unwrap();
        assert_eq!(opened.outcome, BrowserActionOutcome::PageOpened);
        phase("page_opened");
        let captured = broker
            .execute(
                &surface,
                &request(
                    "native-snapshot",
                    BrowserAction::TakeSnapshot {
                        page: opened.page,
                        max_elements: 64,
                    },
                ),
            )
            .await
            .unwrap();
        let snapshot = captured.snapshot.unwrap();
        let element = snapshot
            .elements
            .into_iter()
            .find(|element| element.accessible_name == "Bridge field")
            .unwrap();
        let filled = broker
            .execute(
                &surface,
                &request(
                    "native-fill",
                    BrowserAction::FillForm {
                        page: captured.page,
                        fields: vec![BrowserFormField {
                            element,
                            value: "encrypted-fixture-value".into(),
                        }],
                        mutation_class: BrowserMutationClass::InputFallback,
                    },
                ),
            )
            .await
            .unwrap();
        assert_eq!(filled.outcome, BrowserActionOutcome::FormFilled);
        assert_eq!(filled.form_readback.len(), 1);
        assert_eq!(filled.form_readback[0].value, "encrypted-fixture-value");
        filled.validate().unwrap();
        phase("form_verified");
        let previous_page = filled.page;
        let pending_broker = Arc::clone(&broker);
        let pending_surface = surface.clone();
        let pending_request = request(
            "native-pending-navigation",
            BrowserAction::OpenPage {
                target: BrowserNavigationTarget {
                    url: format!("http://127.0.0.1:{port}/hold"),
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::HttpLoopback,
                        host_ascii: "127.0.0.1".into(),
                        port,
                    },
                },
            },
        );
        let pending = tokio::spawn(async move {
            pending_broker
                .execute(&pending_surface, &pending_request)
                .await
        });
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let observed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(observed["pendingNavigation"], true);
        assert_eq!(observed["visits"], 1);
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"{\"command\":\"switch_profile\"}\n")
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(Duration::from_secs(20), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let switched: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(switched["profileSwitched"], true);
        assert_eq!(switched["pendingVisits"], 1, "old navigation was replayed");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            BrowserExtensionBridgeError::Disconnected,
            "a submitted navigation cannot be classified as NotSubmitted"
        );
        let new_surface = broker.surface_ref().unwrap();
        assert_ne!(surface.token, new_surface.token);
        assert_ne!(
            broker.readiness().unwrap().adapter.profile_incarnation,
            previous_page.adapter.profile_incarnation
        );
        let stale = request(
            "native-stale-page",
            BrowserAction::TakeSnapshot {
                page: previous_page,
                max_elements: 64,
            },
        );
        for candidate in [&surface, &new_surface] {
            assert_eq!(
                broker.execute(candidate, &stale).await.unwrap_err(),
                BrowserExtensionBridgeError::StaleSurface
            );
        }
        assert!(broker.state.lock().unwrap().pending.is_empty());
        let reopened = broker
            .execute(
                &new_surface,
                &request(
                    "native-new-profile",
                    BrowserAction::OpenPage {
                        target: BrowserNavigationTarget {
                            url: format!("http://127.0.0.1:{port}/form"),
                            origin: BrowserOrigin {
                                kind: BrowserOriginKind::HttpLoopback,
                                host_ascii: "127.0.0.1".into(),
                                port,
                            },
                        },
                    },
                ),
            )
            .await
            .unwrap();
        assert_eq!(reopened.outcome, BrowserActionOutcome::PageOpened);
        phase("profile_switch_verified");
    })
    .catch_unwind()
    .await;
    drop(child.stdin.take());
    let peer_exit = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    if peer_exit.is_err() {
        child.kill().await.unwrap();
    }
    phase("peer_stopped");
    handle.stop(true).await;
    running.await.unwrap().unwrap();
    drop(bound.lock);
    phase("bridge_stopped");
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
    assert!(
        matches!(peer_exit, Ok(Ok(status)) if status.success()),
        "native peer cleanup failed or timed out"
    );
}
