use super::*;

// ---- Diagnose routing ----

use desk_agent_protocol::diagnose::{DiagnoseEventKind, DiagnoseRequestData};

fn diagnose_model(raw: serde_json::Value) -> SignalingModel {
    SignalingModel::new(
        "req-diag-1",
        SignalingType::DiagnoseDevice,
        Some("conn-1".to_string()),
        None,
        Some(raw),
        None,
    )
}

/// classify: both halves of the diagnose pair are daemon-owned. `Diagnose`
/// is handled inline by the orchestrator (not worker-bound like
/// `InvokeAgentCapability`); `DiagnoseEvent` is host → control-end only, so a stray
/// inbound copy is swallowed.
#[test]
fn classify_diagnose_pair_is_daemon_owned() {
    assert_eq!(
        classify(SignalingType::DiagnoseDevice),
        RouteOwnership::Daemon
    );
    assert_eq!(
        classify(SignalingType::DiagnosisUpdated),
        RouteOwnership::Daemon
    );
    // The start-over cancellation is handled inline by the daemon too.
    assert_eq!(
        classify(SignalingType::CancelDiagnosis),
        RouteOwnership::Daemon
    );
}

/// classify: the terminal-copilot frames are daemon-owned, mirroring the
/// diagnose pair. The ask drives the daemon-side copilot; the event is
/// daemon-emitted toward the control end and a stray inbound copy is
/// swallowed; the cancel is handled inline.
#[test]
fn classify_terminal_copilot_frames_are_daemon_owned() {
    assert_eq!(
        classify(SignalingType::AskTerminalCopilot),
        RouteOwnership::Daemon
    );
    assert_eq!(
        classify(SignalingType::TerminalCopilotUpdated),
        RouteOwnership::Daemon
    );
    assert_eq!(
        classify(SignalingType::CancelTerminalCopilot),
        RouteOwnership::Daemon
    );
}

/// classify: the command-completion frames are daemon-owned. The ask drives
/// the daemon-side single-shot completion; the result is daemon-emitted toward
/// the control end and a stray inbound copy is swallowed.
#[test]
fn classify_terminal_complete_frames_are_daemon_owned() {
    assert_eq!(
        classify(SignalingType::GenerateTerminalCompletions),
        RouteOwnership::Daemon
    );
    assert_eq!(
        classify(SignalingType::TerminalCompletionsGenerated),
        RouteOwnership::Daemon
    );
}

/// classify: the remote-collect pair is daemon-owned. The request drives the
/// daemon's collectors; the response is daemon-emitted toward the manager and
/// a stray inbound copy is swallowed.
#[test]
fn classify_collect_pair_is_daemon_owned() {
    assert_eq!(
        classify(SignalingType::CollectEvidence),
        RouteOwnership::Daemon
    );
    assert_eq!(
        classify(SignalingType::EvidenceCollectionUpdated),
        RouteOwnership::Daemon
    );
}

fn collect_request_model(request: CollectRequest) -> SignalingModel {
    let raw = serde_json::to_value(&request).unwrap();
    SignalingModel::new(
        "sig-collect-1",
        SignalingType::CollectEvidence,
        Some("manager".to_string()),
        None,
        Some(raw),
        None,
    )
}

fn collect_request(request_id: &str) -> CollectRequest {
    CollectRequest {
        request_id: request_id.to_string(),
        request: DiagnoseRequestData {
            question: "why is the host slow?".into(),
            include_screen: false,
            context_kinds: vec![],
            locale: None,
            conversation_id: None,
            model_id: None,
            org_id: None,
        },
    }
}

/// Drain every queued `CollectResponse` frame off the outbound lane.
fn drain_collect_responses(rx: &mut broadcast::Receiver<String>) -> Vec<CollectResponse> {
    let mut out = Vec::new();
    while let Ok(text) = rx.try_recv() {
        let model: SignalingModel = serde_json::from_str(&text).expect("valid signaling json");
        assert!(matches!(
            model.signaling_type,
            SignalingType::EvidenceCollectionUpdated
        ));
        out.push(
            model
                .get_data::<CollectResponse>()
                .expect("CollectResponse"),
        );
    }
    out
}

fn test_orchestrator(ctx: &RouterContext) -> Arc<DiagnoseOrchestrator> {
    let collector = Arc::new(crate::diagnose::collector::AgentContextCollector::new(
        Arc::new(crate::worker::agent::LocalDeviceAgent::new()),
        ctx.settings.clone().into_inner(),
    ));
    Arc::new(DiagnoseOrchestrator::new(
        collector,
        Arc::new(crate::diagnose::redaction::RegexRedactor::new()),
    ))
}

/// With no in-process collector, a remote-collect request replies with a
/// wholesale error correlated to the request_id (never hangs the manager).
#[tokio::test]
async fn collect_request_without_orchestrator_replies_error() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    handle_collect_request_inbound(&ctx, &collect_request_model(collect_request("rc-1")))
        .await
        .unwrap();
    let responses = drain_collect_responses(&mut rx);
    assert_eq!(responses.len(), 1);
    match &responses[0] {
        CollectResponse::Error(e) => assert_eq!(e.request_id, "rc-1"),
        other => panic!("expected an error response, got {other:?}"),
    }
}

/// A remote-collect request runs the in-process collectors and streams the
/// evidence back as chunks that reassemble into a snapshot carrying the
/// default read set (system.info is collected on every CI host).
#[tokio::test]
async fn collect_request_streams_reassemblable_snapshot() {
    let mut ctx = make_ctx_with_rx().await.0;
    ctx.diagnose_orchestrator = Some(test_orchestrator(&ctx));
    // Subscribe after installing the orchestrator so the receiver is fresh.
    let mut rx = ctx.outbound_tx.subscribe();

    handle_collect_request_inbound(&ctx, &collect_request_model(collect_request("rc-2")))
        .await
        .unwrap();

    let responses = drain_collect_responses(&mut rx);
    assert!(!responses.is_empty(), "expected at least one chunk");
    let mut reassembler = desk_diagnose_core::chunk::SnapshotReassembler::new();
    for resp in &responses {
        match resp {
            CollectResponse::Chunk(c) => reassembler.push(c).expect("chunk accepted"),
            CollectResponse::Error(e) => panic!("unexpected error: {}", e.reason),
        }
    }
    let snapshot = reassembler.finish().expect("snapshot reassembles");
    assert!(
        snapshot
            .contexts
            .iter()
            .any(|c| c.capability == "system.info"),
        "snapshot should carry the default read set"
    );
}

/// AI diagnosis is centralized: a `Diagnose` frame that reaches the edge
/// router (a link without a central signaling brain) is answered with one
/// terminal `DiagnoseEvent::error` (notification-style, not a one-shot
/// response) telling the control end the central server owns diagnosis. The
/// edge only serves evidence collection (`CollectRequest`); it never runs a
/// browser-facing diagnosis locally, so there is no gateway / PDP / agentic
/// path to drive here.
#[tokio::test]
async fn diagnose_at_edge_replies_centralized_unavailable() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let raw = serde_json::to_value(DiagnoseRequestData {
        question: "why?".into(),
        include_screen: false,
        context_kinds: vec![],
        locale: None,
        conversation_id: None,
        model_id: None,
        org_id: None,
    })
    .unwrap();
    handle_diagnose_inbound(&ctx, &diagnose_model(raw))
        .await
        .unwrap();
    let frame = read_response(&mut rx);
    assert_eq!(frame.signaling_type, SignalingType::DiagnosisUpdated);
    // Notification, not a one-shot response.
    assert!(frame.response_state.is_none());
    let event = frame.get_data::<DiagnoseEvent>().expect("DiagnoseEvent");
    assert_eq!(event.kind, DiagnoseEventKind::Error);
    let err = event.error.unwrap();
    assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
    assert!(err.message.contains("central signaling server"));
}

/// The terminal copilot is centralized: a `TerminalCopilotAsk` reaching the
/// edge router is answered with one terminal `TerminalCopilotEvent::error`
/// pointing at the central server (the edge runs no local copilot).
#[tokio::test]
async fn terminal_copilot_at_edge_replies_centralized_unavailable() {
    use desk_agent_protocol::terminal_copilot::{TerminalCopilotEvent, TerminalCopilotEventKind};
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let ask = SignalingModel::new(
        "req-cop-1",
        SignalingType::AskTerminalCopilot,
        Some("conn-1".to_string()),
        None,
        None,
        None,
    );
    handle_terminal_copilot_inbound(&ctx, &ask).await.unwrap();
    let frame = read_response(&mut rx);
    assert_eq!(frame.signaling_type, SignalingType::TerminalCopilotUpdated);
    let event = frame
        .get_data::<TerminalCopilotEvent>()
        .expect("TerminalCopilotEvent");
    assert_eq!(event.kind, TerminalCopilotEventKind::Error);
    let err = event.error.unwrap();
    assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
    assert!(err.message.contains("central signaling server"));
}

/// Inline command completion is centralized: a `TerminalCompleteAsk` reaching
/// the edge router is answered with one error `TerminalCompleteResult`.
#[tokio::test]
async fn terminal_complete_at_edge_replies_centralized_unavailable() {
    use desk_agent_protocol::terminal_complete::TerminalCompleteResult;
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let ask = SignalingModel::new(
        "req-comp-1",
        SignalingType::GenerateTerminalCompletions,
        Some("conn-1".to_string()),
        None,
        None,
        None,
    );
    handle_terminal_complete_inbound(&ctx, &ask).await.unwrap();
    let frame = read_response(&mut rx);
    assert_eq!(
        frame.signaling_type,
        SignalingType::TerminalCompletionsGenerated
    );
    let result = frame
        .get_data::<TerminalCompleteResult>()
        .expect("TerminalCompleteResult");
    assert!(result.is_error());
    assert!(
        result
            .error
            .unwrap()
            .message
            .contains("central signaling server")
    );
}

fn diagnose_cancel_model() -> SignalingModel {
    SignalingModel::new(
        "req-diag-1",
        SignalingType::CancelDiagnosis,
        Some("conn-1".to_string()),
        None,
        None,
        None,
    )
}

/// A start-over cancel aborts the in-flight orchestrator task so
/// a slow model call does not keep running, and clears the registry entry.
#[actix_web::test]
async fn diagnose_cancel_aborts_inflight_task() {
    let ctx = make_ctx().await;
    // Register a never-completing task under the cancel model's request_id,
    // standing in for an orchestrator run blocked on a slow model.
    let handle = actix_web::rt::spawn(async {
        std::future::pending::<()>().await;
    });
    ctx.diagnose_tasks
        .lock()
        .unwrap()
        .insert("req-diag-1".to_string(), handle);

    handle_diagnose_cancel_inbound(&ctx, &diagnose_cancel_model())
        .await
        .unwrap();

    // The entry is removed (and the task aborted) by the cancel.
    assert!(
        ctx.diagnose_tasks.lock().unwrap().is_empty(),
        "cancel must abort and drop the in-flight task"
    );
}

/// Cancellation with no orchestrator injected (ServiceDaemon-like) is a no-op: no
/// audit, no frame.
#[tokio::test]
async fn diagnose_cancel_without_orchestrator_is_noop() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    // No orchestrator injected; cancel has nothing to audit.
    handle_diagnose_cancel_inbound(&ctx, &diagnose_cancel_model())
        .await
        .unwrap();
    assert!(rx.try_recv().is_err());
}
