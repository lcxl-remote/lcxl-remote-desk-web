use super::*;
use std::time::Duration;

fn approval_req(req_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        req_id: req_id.to_string(),
        permission_type: SecurityPermissionType::RemoteControl,
        from_connection_id: Some("conn-1".to_string()),
    }
}

// U-3: Local mode without ws subscribers denies immediately.
#[tokio::test]
async fn u3_local_no_subscriber_denies_immediately() {
    let hub = HostControlHub::new_local();
    let started = std::time::Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_millis(200),
        hub.request_approval(approval_req("r1"), None),
    )
    .await
    .expect("must not block");
    assert!(!resp.approved);
    assert!(!resp.remember);
    assert!(started.elapsed() < Duration::from_millis(100));
    // No replay entry created.
    assert_eq!(hub.pending_replay_count(), 0);
}

// U-3b: Forwarder mode with offline upstream denies immediately.
#[tokio::test]
async fn u3b_forwarder_offline_denies_immediately() {
    let upstream = UpstreamForwarder::new_for_test(false);
    let hub = HostControlHub::new_forwarder(upstream);
    let resp = tokio::time::timeout(
        Duration::from_millis(200),
        hub.request_approval(approval_req("r1"), None),
    )
    .await
    .expect("must not block");
    assert!(!resp.approved);
}

// U-3c: Local mode with at least one subscriber pends until submit.
#[tokio::test]
async fn u3c_local_with_subscriber_pends_until_submit() {
    let hub = HostControlHub::new_local();
    // Pretend Tauri is connected.
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task =
        tokio::spawn(async move { hub_clone.request_approval(approval_req("r1"), None).await });

    // Give the task time to enter pending state.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(hub.pending_replay_count(), 1);

    let solved = hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: false,
        },
    );
    assert!(solved, "submit should find the pending entry");

    let resp = tokio::time::timeout(Duration::from_millis(200), task)
        .await
        .expect("oneshot must resolve")
        .expect("task ok");
    assert!(resp.approved);
    assert!(!resp.remember);
    assert_eq!(hub.pending_replay_count(), 0);
}

// Regression: Local submit_approval must broadcast a SecurityApprovalFinished
// so the Tauri shell can release always-on-top once the dialog closes.
#[tokio::test]
async fn local_submit_broadcasts_finished_to_tauri() {
    let hub = HostControlHub::new_local();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task =
        tokio::spawn(async move { hub_clone.request_approval(approval_req("r1"), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Drain the original Request so the next recv() observes Finished cleanly.
    match tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .expect("Request must be broadcast")
        .expect("channel ok")
    {
        HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
        other => panic!("expected SecurityApprovalRequest, got {other:?}"),
    }

    assert!(hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: false,
        }
    ));
    let resp = tokio::time::timeout(Duration::from_millis(200), task)
        .await
        .expect("oneshot must resolve")
        .unwrap();
    assert!(resp.approved);

    let finished = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .expect("Finished must be broadcast")
        .expect("channel ok");
    match finished {
        HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
        other => panic!("expected SecurityApprovalFinished, got {other:?}"),
    }
}

// Local submit_approval for an unknown req_id must NOT broadcast Finished —
// otherwise a duplicate user click could spuriously release always-on-top
// while another dialog is still up.
#[tokio::test]
async fn local_unknown_submit_does_not_broadcast_finished() {
    let hub = HostControlHub::new_local();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    assert!(!hub.submit_approval("ghost", ApprovalResponse::deny()));
    let bcast = tokio::time::timeout(Duration::from_millis(50), outbound_rx.recv()).await;
    assert!(
        bcast.is_err(),
        "no message expected when no pending entry matched"
    );
}

// Aggregator submit_approval also notifies Tauri so the shell can drop
// always-on-top symmetrically with the Local path.
#[tokio::test]
async fn aggregator_submit_broadcasts_finished_to_tauri() {
    let hub = HostControlHub::new_aggregator();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();
    let (tx, mut rx_fwd) = mpsc::unbounded_channel();
    hub.register_forwarder_session(1, tx);

    // Simulate the upstream worker registering an in-flight approval.
    hub.register_upstream_request(
        "r1".to_string(),
        1,
        SecurityPermissionType::RemoteControl,
        None,
    );

    assert!(hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: false,
        }
    ));

    // Forwarder gets the directional Submit.
    match tokio::time::timeout(Duration::from_millis(100), rx_fwd.recv())
        .await
        .expect("forwarder must receive submit")
        .expect("mpsc ok")
    {
        HostControlMessage::SecurityApprovalSubmit { req_id, .. } => assert_eq!(req_id, "r1"),
        other => panic!("expected SecurityApprovalSubmit, got {other:?}"),
    }

    // Tauri broadcast carries Finished.
    match tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .expect("Finished must be broadcast")
        .expect("channel ok")
    {
        HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
        other => panic!("expected SecurityApprovalFinished, got {other:?}"),
    }
}

// U-4: Submit with mismatched req_id is no-op; existing pending unaffected.
#[tokio::test]
async fn u4_submit_unknown_req_id_is_noop() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task =
        tokio::spawn(async move { hub_clone.request_approval(approval_req("r1"), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Wrong id — should not resolve r1.
    let solved = hub.submit_approval("r-other", ApprovalResponse::deny());
    assert!(!solved);

    // Correct id resolves the original.
    let solved = hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: true,
        },
    );
    assert!(solved);

    let resp = task.await.unwrap();
    assert!(resp.approved && resp.remember);
}

// U-7: 100 concurrent approvals resolved in shuffled order — no deadlock, no loss.
#[tokio::test]
async fn u7_concurrent_approvals_no_deadlock() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let mut tasks = Vec::new();
    for i in 0..100 {
        let id = format!("r{i}");
        let hub_clone = hub.clone();
        tasks.push(tokio::spawn(async move {
            let id_inner = id.clone();
            let resp = hub_clone
                .request_approval(approval_req(&id_inner), None)
                .await;
            (id, resp)
        }));
    }

    // Wait until all entered pending state.
    for _ in 0..50 {
        if hub.pending_replay_count() == 100 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(hub.pending_replay_count(), 100);

    // Submit in shuffled order.
    let mut order: Vec<usize> = (0..100).collect();
    order.swap(0, 99);
    order.swap(20, 50);
    order.swap(33, 66);
    for i in order {
        let approved = i % 2 == 0;
        hub.submit_approval(
            &format!("r{i}"),
            ApprovalResponse {
                approved,
                remember: false,
            },
        );
    }

    for t in tasks {
        let (id, resp) = t.await.unwrap();
        let i: usize = id[1..].parse().unwrap();
        assert_eq!(resp.approved, i.is_multiple_of(2));
    }
    assert_eq!(hub.pending_replay_count(), 0);
}

// U-8: state broadcast reaches multiple subscribers.
#[tokio::test]
async fn u8_state_broadcast_multi_subscriber() {
    let hub = HostControlHub::new_local();
    let mut rx_a = hub.subscribe_state();
    let mut rx_b = hub.subscribe_state();
    hub.publish_state(HostControlEvent::PrivateScreenVisibilityChanged {
        connection_id: "c1".to_string(),
        visible: true,
    });
    let a = tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(Duration::from_millis(100), rx_b.recv())
        .await
        .unwrap()
        .unwrap();
    for ev in [a, b] {
        match ev {
            HostControlEvent::PrivateScreenVisibilityChanged {
                connection_id,
                visible,
            } => {
                assert_eq!(connection_id, "c1");
                assert!(visible);
            }
        }
    }
}

// U-9: send_command returns Ok(0) when nobody is listening.
#[test]
fn u9_send_command_zero_subscribers_is_ok() {
    let hub = HostControlHub::new_local();
    let n = hub
        .send_command(HostControlMessage::PrivateScreenShow {
            connection_id: "c1".to_string(),
        })
        .expect("send_command should not error on no subscribers");
    assert_eq!(n, 0);
}

// U-10: Forwarder send_command with online upstream forwards to upstream queue.
#[tokio::test]
async fn u10_forwarder_send_command_forwards_to_upstream() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let mut outbound_rx = upstream.test_outbound_rx();
    let hub = HostControlHub::new_forwarder(upstream);

    let cmd = HostControlMessage::PrivateScreenShow {
        connection_id: "c1".to_string(),
    };
    let n = hub.send_command(cmd.clone()).unwrap();
    assert_eq!(n, 1);

    let received = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .unwrap()
        .expect("upstream must receive");
    assert_eq!(received, cmd);
}

// U-11: Forwarder send_command with offline upstream is silent (no panic, returns 0).
#[tokio::test]
async fn u11_forwarder_offline_send_command_silent() {
    let upstream = UpstreamForwarder::new_for_test(false);
    let hub = HostControlHub::new_forwarder(upstream);
    let n = hub
        .send_command(HostControlMessage::PrivateScreenShow {
            connection_id: "c1".to_string(),
        })
        .unwrap();
    assert_eq!(n, 0);
}

// U-12: Forwarder receiving SubmitApproval from upstream resolves the local oneshot.
#[tokio::test]
async fn u12_forwarder_receives_submit_resolves_pending() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let upstream_clone = Arc::clone(&upstream);
    let hub = HostControlHub::new_forwarder(upstream);

    let hub_clone = hub.clone();
    let task =
        tokio::spawn(async move { hub_clone.request_approval(approval_req("r1"), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    upstream_clone.test_inject_inbound(HostControlMessage::SecurityApprovalSubmit {
        req_id: "r1".to_string(),
        approved: true,
        remember: true,
    });

    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(resp.approved && resp.remember);
}

// U-13: Forwarder receiving Cancel resolves with deny.
#[tokio::test]
async fn u13_forwarder_receives_cancel_resolves_deny() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let upstream_clone = Arc::clone(&upstream);
    let hub = HostControlHub::new_forwarder(upstream);

    let hub_clone = hub.clone();
    let task =
        tokio::spawn(async move { hub_clone.request_approval(approval_req("r1"), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    upstream_clone.test_inject_inbound(HostControlMessage::SecurityApprovalCancel {
        req_id: "r1".to_string(),
    });

    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(!resp.approved);
}

// U-14: Aggregator routing — upstream registers + drains correctly.
#[test]
fn u14_aggregator_pending_routes_lifecycle() {
    let hub = HostControlHub::new_aggregator();
    hub.register_upstream_request(
        "r1".to_string(),
        42,
        SecurityPermissionType::RemoteControl,
        None,
    );
    hub.register_upstream_request(
        "r2".to_string(),
        42,
        SecurityPermissionType::PrivateScreen,
        Some("c2".to_string()),
    );
    hub.register_upstream_request(
        "r3".to_string(),
        999,
        SecurityPermissionType::Whiteboard,
        None,
    );

    // Replay snapshot present for all 3.
    assert_eq!(hub.pending_replay_count(), 3);
    let snaps = hub.replay_messages_for_tauri();
    assert_eq!(snaps.len(), 3);

    // pop_upstream_for_req removes the entry.
    assert_eq!(hub.pop_upstream_for_req("r1"), Some(42));
    assert_eq!(hub.pop_upstream_for_req("r1"), None);
    assert_eq!(hub.pending_replay_count(), 2);

    // drain_upstream_pending strips remaining 42-owned entries.
    let drained = hub.drain_upstream_pending(42);
    assert_eq!(drained, vec!["r2".to_string()]);
    assert_eq!(hub.pending_replay_count(), 1);

    // r3 still owned by 999.
    let drained = hub.drain_upstream_pending(999);
    assert_eq!(drained, vec!["r3".to_string()]);
    assert_eq!(hub.pending_replay_count(), 0);
}

// U-14c: Aggregator replay snapshot reflects pending requests.
#[test]
fn u14c_aggregator_replay_snapshot() {
    let hub = HostControlHub::new_aggregator();
    hub.register_upstream_request("r1".to_string(), 7, SecurityPermissionType::Terminal, None);
    let msgs = hub.replay_messages_for_tauri();
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        HostControlMessage::SecurityApprovalRequest {
            req_id,
            permission_type,
            ..
        } => {
            assert_eq!(req_id, "r1");
            assert!(matches!(permission_type, SecurityPermissionType::Terminal));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// Aggregator originates daemon-self approvals (the daemon owns
// the WebRTC PC and runs `check_security_permission` for RequireControl).
// Without a Tauri shell connected the request denies fast — same shape as
// the Local-no-subscriber path.
#[tokio::test]
async fn aggregator_request_approval_no_tauri_denies_fast() {
    let hub = HostControlHub::new_aggregator();
    let started = std::time::Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_millis(200),
        hub.request_approval(approval_req("r1"), None),
    )
    .await
    .expect("must not block");
    assert!(!resp.approved);
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(hub.pending_replay_count(), 0);
}

// Regression: Aggregator with a Tauri shell present must broadcast
// the SecurityApprovalRequest and pend until submit_approval resolves the
// oneshot. This is the exact path RequireControl takes now that the
// PC lives in the daemon — before the fix it hit the old "router does not
// request" hard-deny and the Tauri shell never saw a dialog.
#[tokio::test]
async fn aggregator_request_approval_pends_until_submit() {
    let hub = HostControlHub::new_aggregator();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task =
        tokio::spawn(async move { hub_clone.request_approval(approval_req("r1"), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Replay snapshot recorded so a reconnecting Tauri can resume the dialog.
    assert_eq!(hub.pending_replay_count(), 1);
    let replay = hub.replay_messages_for_tauri();
    assert_eq!(replay.len(), 1);
    match &replay[0] {
        HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
        other => panic!("unexpected replay frame: {other:?}"),
    }

    // Tauri saw the broadcast.
    let bcast = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .expect("Request must be broadcast")
        .expect("channel ok");
    match bcast {
        HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
        other => panic!("expected SecurityApprovalRequest, got {other:?}"),
    }

    let solved = hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: false,
        },
    );
    assert!(solved, "daemon-self submit must resolve the local oneshot");

    let resp = tokio::time::timeout(Duration::from_millis(200), task)
        .await
        .expect("oneshot must resolve")
        .expect("task ok");
    assert!(resp.approved);
    assert_eq!(hub.pending_replay_count(), 0);

    // Finished frame is broadcast too so the Tauri shell drops always-on-top.
    let finished = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .expect("Finished must be broadcast")
        .expect("channel ok");
    match finished {
        HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
        other => panic!("expected SecurityApprovalFinished, got {other:?}"),
    }
}

// Mixed sources: Aggregator handles a daemon-self request and a worker-
// originated request concurrently. Each submit must reach exactly the right
// resolver — the daemon-self oneshot for the daemon req, the originating
// forwarder's mpsc for the worker req. They must NOT cross-contaminate.
#[tokio::test]
async fn aggregator_mixed_daemon_self_and_worker_routes_correctly() {
    let hub = HostControlHub::new_aggregator();
    hub.mark_tauri_connected();
    let _outbound_rx = hub.subscribe_outbound();

    // Worker-originated request via upstream registration.
    let (tx_w, mut rx_w) = mpsc::unbounded_channel();
    hub.register_forwarder_session(7, tx_w);
    hub.register_upstream_request(
        "r-worker".to_string(),
        7,
        SecurityPermissionType::Terminal,
        None,
    );

    // Daemon-self request via request_approval.
    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval(approval_req("r-daemon"), None)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(hub.pending_replay_count(), 2);

    // Submit the worker req — forwarder mpsc must get the directional Submit.
    assert!(hub.submit_approval(
        "r-worker",
        ApprovalResponse {
            approved: true,
            remember: false,
        }
    ));
    match tokio::time::timeout(Duration::from_millis(100), rx_w.recv())
        .await
        .expect("forwarder must receive")
        .expect("mpsc ok")
    {
        HostControlMessage::SecurityApprovalSubmit { req_id, .. } => {
            assert_eq!(req_id, "r-worker")
        }
        other => panic!("unexpected: {other:?}"),
    }
    // Daemon-self task must still be pending — the worker submit must not
    // accidentally resolve it.
    assert!(!task.is_finished());

    // Submit the daemon req — local oneshot resolves.
    assert!(hub.submit_approval(
        "r-daemon",
        ApprovalResponse {
            approved: false,
            remember: true,
        }
    ));
    let resp = tokio::time::timeout(Duration::from_millis(200), task)
        .await
        .expect("oneshot must resolve")
        .expect("task ok");
    assert!(!resp.approved);
    assert!(resp.remember);

    // Forwarder must NOT have received anything else.
    let stray = tokio::time::timeout(Duration::from_millis(50), rx_w.recv()).await;
    assert!(stray.is_err(), "forwarder must not see r-daemon submit");

    assert_eq!(hub.pending_replay_count(), 0);
}

// When the last Tauri shell drops, daemon-self pending approvals must be
// resolved as deny (so request_approval callers unblock) in addition to the
// existing forwarder-cancel path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregator_tauri_loss_denies_daemon_self_pending() {
    let hub = HostControlHub::new_aggregator();
    hub.mark_tauri_connected();
    let _outbound_rx = hub.subscribe_outbound();

    // One worker req + one daemon-self req in flight.
    let (tx_w, mut rx_w) = mpsc::unbounded_channel();
    hub.register_forwarder_session(3, tx_w);
    hub.register_upstream_request(
        "r-worker".to_string(),
        3,
        SecurityPermissionType::FileTransfer,
        None,
    );
    let h = hub.clone();
    let task =
        tokio::spawn(async move { h.request_approval(approval_req("r-daemon"), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(hub.pending_replay_count(), 2);

    // Tauri lost.
    let cancelled = hub.cancel_all_for_tauri_loss();
    assert_eq!(cancelled, vec!["r-worker".to_string()]);

    // Daemon-self oneshot resolved as deny (does not appear in `cancelled`
    // because that list reports worker-originated cancels by contract).
    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("daemon oneshot must resolve")
        .expect("task ok");
    assert!(!resp.approved);

    // Worker forwarder received the directional Cancel.
    match tokio::time::timeout(Duration::from_millis(100), rx_w.recv())
        .await
        .expect("forwarder must receive")
        .expect("mpsc ok")
    {
        HostControlMessage::SecurityApprovalCancel { req_id } => assert_eq!(req_id, "r-worker"),
        other => panic!("unexpected: {other:?}"),
    }

    assert_eq!(hub.pending_replay_count(), 0);
}

// U-14b: Aggregator submit_approval routes the response directionally to
// the originating forwarder session via its registered mpsc — never the
// outbound broadcast (a second forwarder session must not see the message).
#[tokio::test]
async fn u14b_aggregator_submit_directional_only() {
    let hub = HostControlHub::new_aggregator();

    // Two forwarder sessions registered with their own mpsc receivers.
    let (tx_a, mut rx_a) = mpsc::unbounded_channel();
    let (tx_b, mut rx_b) = mpsc::unbounded_channel();
    hub.register_forwarder_session(1, tx_a);
    hub.register_forwarder_session(2, tx_b);

    // Track outbound broadcast — submit must not appear here.
    let mut outbound_rx = hub.subscribe_outbound();

    hub.register_upstream_request(
        "r1".to_string(),
        1,
        SecurityPermissionType::RemoteControl,
        None,
    );
    hub.register_upstream_request("r2".to_string(), 2, SecurityPermissionType::Terminal, None);

    let dispatched = hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: false,
        },
    );
    assert!(dispatched, "directional submit must succeed");

    // Forwarder #1 receives the SubmitApproval.
    let got = tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
        .await
        .expect("session 1 must receive")
        .expect("mpsc closed");
    match got {
        HostControlMessage::SecurityApprovalSubmit {
            req_id,
            approved,
            remember,
        } => {
            assert_eq!(req_id, "r1");
            assert!(approved);
            assert!(!remember);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Forwarder #2 must NOT have received it.
    let other = tokio::time::timeout(Duration::from_millis(50), rx_b.recv()).await;
    assert!(other.is_err(), "session 2 must not receive r1's submit");

    // Outbound broadcast must NOT carry SubmitApproval; it carries the
    // Tauri-bound Finished notification instead so the shell can release
    // its dialog UI affordances.
    let bcast = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .expect("Finished broadcast expected")
        .expect("channel ok");
    match bcast {
        HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
        other @ HostControlMessage::SecurityApprovalSubmit { .. } => {
            panic!("Submit must not appear on broadcast: {other:?}")
        }
        other => panic!("unexpected broadcast frame: {other:?}"),
    }

    // Replay/route entries for r1 are removed.
    assert_eq!(hub.pending_replay_count(), 1);
}

// U-14d: Aggregator submit for an unknown req_id returns false.
#[tokio::test]
async fn u14d_aggregator_submit_unknown_returns_false() {
    let hub = HostControlHub::new_aggregator();
    let (tx, _rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(7, tx);

    let dispatched = hub.submit_approval("does-not-exist", ApprovalResponse::deny());
    assert!(!dispatched);
}

// Aggregator immediately denies an upstream approval request when no Tauri
// shell is connected — prevents the worker from blocking until the heartbeat
// watchdog kills it.
#[tokio::test]
async fn aggregator_handle_upstream_request_denies_without_tauri() {
    let hub = HostControlHub::new_aggregator();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(1, tx);

    // No mark_tauri_connected — UI is offline.
    let accepted = hub.handle_upstream_approval_request(
        "r1".to_string(),
        1,
        SecurityPermissionType::RemoteControl,
        None,
    );
    assert!(!accepted, "must report denied");
    assert_eq!(
        hub.pending_replay_count(),
        0,
        "denied request must not be registered for replay"
    );

    let msg = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .expect("forwarder must receive a deny submit")
        .expect("mpsc closed");
    match msg {
        HostControlMessage::SecurityApprovalSubmit {
            req_id,
            approved,
            remember,
        } => {
            assert_eq!(req_id, "r1");
            assert!(!approved);
            assert!(!remember);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// Aggregator registers the request and broadcasts it when a Tauri shell is
// connected. No deny is routed back to the forwarder.
#[tokio::test]
async fn aggregator_handle_upstream_request_broadcasts_when_tauri_present() {
    let hub = HostControlHub::new_aggregator();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(1, tx);

    let accepted = hub.handle_upstream_approval_request(
        "r1".to_string(),
        1,
        SecurityPermissionType::RemoteControl,
        None,
    );
    assert!(accepted);
    assert_eq!(hub.pending_replay_count(), 1);

    let bcast = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
        .await
        .expect("broadcast must fire")
        .expect("channel closed");
    match bcast {
        HostControlMessage::SecurityApprovalRequest { req_id, .. } => {
            assert_eq!(req_id, "r1");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Forwarder must NOT receive an immediate deny.
    let nothing = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
    assert!(nothing.is_err(), "forwarder must not get a deny submit");
}

// Aggregator drain_upstream_pending also removes the forwarder session entry.
#[tokio::test]
async fn aggregator_drain_unregisters_session() {
    let hub = HostControlHub::new_aggregator();
    let (tx, _rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(42, tx);
    hub.register_upstream_request(
        "r1".to_string(),
        42,
        SecurityPermissionType::RemoteControl,
        None,
    );

    let drained = hub.drain_upstream_pending(42);
    assert_eq!(drained, vec!["r1".to_string()]);

    // After drain, route_to_forwarder fails for the same session_id.
    let routed = hub.route_to_forwarder(
        42,
        HostControlMessage::SecurityApprovalCancel {
            req_id: "r1".to_string(),
        },
    );
    assert!(!routed, "drained session must be unregistered");
}

#[tokio::test]
async fn cancel_pending_for_connection_denies_only_matching_local_requests() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let matching_hub = hub.clone();
    let matching = tokio::spawn(async move {
        matching_hub
            .request_approval(approval_req("matching"), None)
            .await
    });
    let other_hub = hub.clone();
    let mut other_request = approval_req("other");
    other_request.from_connection_id = Some("conn-2".to_string());
    let other = tokio::spawn(async move { other_hub.request_approval(other_request, None).await });

    for _ in 0..50 {
        if hub.inner.pending_approvals.lock().unwrap().len() == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(hub.inner.pending_approvals.lock().unwrap().len(), 2);

    assert_eq!(
        hub.cancel_pending_for_connection("conn-1"),
        vec!["matching".to_string()]
    );
    let response = matching.await.expect("matching request task");
    assert!(!response.approved);
    assert!(
        hub.inner
            .pending_approvals
            .lock()
            .unwrap()
            .contains_key("other")
    );

    hub.cancel_pending_for_connection("conn-2");
    assert!(!other.await.expect("other request task").approved);
}

#[tokio::test]
async fn cancel_pending_for_connection_routes_worker_cancel_directionally() {
    let hub = HostControlHub::new_aggregator();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(42, tx);
    hub.register_upstream_request(
        "matching".to_string(),
        42,
        SecurityPermissionType::RemoteControl,
        Some("conn-1".to_string()),
    );
    hub.register_upstream_request(
        "other".to_string(),
        42,
        SecurityPermissionType::RemoteControl,
        Some("conn-2".to_string()),
    );

    assert_eq!(
        hub.cancel_pending_for_connection("conn-1"),
        vec!["matching".to_string()]
    );
    assert_eq!(hub.pending_replay_count(), 1);
    assert!(
        hub.inner
            .pending_routes
            .lock()
            .unwrap()
            .contains_key("other")
    );
    assert_eq!(
        rx.recv().await,
        Some(HostControlMessage::SecurityApprovalCancel {
            req_id: "matching".to_string(),
        })
    );
}

#[tokio::test]
async fn security_lock_cancels_pending_request_without_connection_id() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();
    let mut request = approval_req("unbound");
    request.from_connection_id = None;
    let request_hub = hub.clone();
    let task = tokio::spawn(async move { request_hub.request_approval(request, None).await });
    for _ in 0..50 {
        if hub.inner.pending_approvals.lock().unwrap().len() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(
        hub.cancel_all_pending_for_security_lock(),
        vec!["unbound".to_string()]
    );
    assert!(!task.await.expect("approval task").approved);
    assert_eq!(hub.pending_replay_count(), 0);
}

#[tokio::test]
async fn security_lock_routes_unbound_worker_approval_cancel() {
    let hub = HostControlHub::new_aggregator();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(9, tx);
    hub.register_upstream_request(
        "worker-unbound".to_string(),
        9,
        SecurityPermissionType::RemoteControl,
        None,
    );

    assert_eq!(
        hub.cancel_all_pending_for_security_lock(),
        vec!["worker-unbound".to_string()]
    );
    assert_eq!(
        rx.recv().await,
        Some(HostControlMessage::SecurityApprovalCancel {
            req_id: "worker-unbound".to_string(),
        })
    );
    assert_eq!(hub.pending_replay_count(), 0);
}

// route_to_forwarder fails silently when the receiver was already dropped.
#[tokio::test]
async fn route_to_forwarder_handles_closed_receiver() {
    let hub = HostControlHub::new_aggregator();
    let (tx, rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(5, tx);
    drop(rx); // simulate ws task gone

    let routed = hub.route_to_forwarder(
        5,
        HostControlMessage::SecurityApprovalCancel {
            req_id: "r-x".to_string(),
        },
    );
    assert!(!routed);
}

// mark_tauri_disconnected returns the post-decrement count and saturates at
// zero so a stray double-disconnect never wraps around.
#[test]
fn tauri_client_count_saturates_at_zero() {
    let hub = HostControlHub::new_local();
    assert_eq!(hub.tauri_client_count(), 0);
    hub.mark_tauri_connected();
    hub.mark_tauri_connected();
    assert_eq!(hub.tauri_client_count(), 2);
    assert_eq!(hub.mark_tauri_disconnected(), 1);
    assert_eq!(hub.mark_tauri_disconnected(), 0);
    // Saturating: an extra decrement must not underflow.
    assert_eq!(hub.mark_tauri_disconnected(), 0);
    assert_eq!(hub.tauri_client_count(), 0);
}

// Plan §6 兜底: Forwarder upstream lost — every in-flight approval is
// resolved as deny without business code observing a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarder_upstream_disconnect_denies_pending() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let upstream_clone = Arc::clone(&upstream);
    let hub = HostControlHub::new_forwarder(upstream);

    let h1 = hub.clone();
    let h2 = hub.clone();
    let t1 = tokio::spawn(async move { h1.request_approval(approval_req("a"), None).await });
    let t2 = tokio::spawn(async move { h2.request_approval(approval_req("b"), None).await });
    // Wait until both requests parked in pending_approvals.
    for _ in 0..50 {
        if hub.inner.pending_approvals.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(hub.inner.pending_approvals.lock().unwrap().len(), 2);

    upstream_clone.mark_disconnected();

    let r1 = tokio::time::timeout(Duration::from_millis(2000), t1)
        .await
        .expect("must resolve")
        .unwrap();
    let r2 = tokio::time::timeout(Duration::from_millis(2000), t2)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(!r1.approved && !r2.approved);
    assert!(hub.inner.pending_approvals.lock().unwrap().is_empty());
}

// Plan §6 兜底: Aggregator's cancel_all_for_tauri_loss routes a
// SecurityApprovalCancel to each owning forwarder and clears the tables.
#[tokio::test]
async fn aggregator_cancel_all_for_tauri_loss_routes_directionally() {
    let hub = HostControlHub::new_aggregator();
    let (tx_a, mut rx_a) = mpsc::unbounded_channel();
    let (tx_b, mut rx_b) = mpsc::unbounded_channel();
    hub.register_forwarder_session(1, tx_a);
    hub.register_forwarder_session(2, tx_b);

    hub.register_upstream_request(
        "r1".to_string(),
        1,
        SecurityPermissionType::RemoteControl,
        None,
    );
    hub.register_upstream_request("r2".to_string(), 1, SecurityPermissionType::Terminal, None);
    hub.register_upstream_request(
        "r3".to_string(),
        2,
        SecurityPermissionType::Whiteboard,
        None,
    );

    let mut cancelled = hub.cancel_all_for_tauri_loss();
    cancelled.sort();
    assert_eq!(
        cancelled,
        vec!["r1".to_string(), "r2".to_string(), "r3".to_string()]
    );
    assert_eq!(hub.pending_replay_count(), 0);

    // Forwarder 1 receives Cancel for r1 and r2 (in some order).
    let mut got_a = Vec::new();
    for _ in 0..2 {
        let m = tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
            .await
            .expect("session 1 must receive")
            .expect("mpsc closed");
        match m {
            HostControlMessage::SecurityApprovalCancel { req_id } => got_a.push(req_id),
            other => panic!("unexpected: {other:?}"),
        }
    }
    got_a.sort();
    assert_eq!(got_a, vec!["r1".to_string(), "r2".to_string()]);

    // Forwarder 2 receives Cancel for r3 only.
    let m = tokio::time::timeout(Duration::from_millis(100), rx_b.recv())
        .await
        .expect("session 2 must receive")
        .expect("mpsc closed");
    match m {
        HostControlMessage::SecurityApprovalCancel { req_id } => assert_eq!(req_id, "r3"),
        other => panic!("unexpected: {other:?}"),
    }

    // Idempotent: a second call has nothing to do.
    assert!(hub.cancel_all_for_tauri_loss().is_empty());
}

// cancel_all_for_tauri_loss is a no-op on Local/Forwarder hubs.
#[test]
fn cancel_all_for_tauri_loss_only_aggregator() {
    let hub = HostControlHub::new_local();
    assert!(hub.cancel_all_for_tauri_loss().is_empty());
}

// deny_all_pending resolves every outstanding oneshot with deny.
#[tokio::test]
async fn deny_all_pending_resolves_everything() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let h1 = hub.clone();
    let h2 = hub.clone();
    let t1 = tokio::spawn(async move { h1.request_approval(approval_req("a"), None).await });
    let t2 = tokio::spawn(async move { h2.request_approval(approval_req("b"), None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    hub.deny_all_pending();
    let r1 = tokio::time::timeout(Duration::from_millis(200), t1)
        .await
        .unwrap()
        .unwrap();
    let r2 = tokio::time::timeout(Duration::from_millis(200), t2)
        .await
        .unwrap()
        .unwrap();
    assert!(!r1.approved && !r2.approved);
    assert_eq!(hub.pending_replay_count(), 0);
}

// P2: helper to wait until the readiness-probe entry for `req_id` exists.
async fn wait_for_pending_ack(hub: &HostControlHub, req_id: &str) {
    for _ in 0..200 {
        if hub.inner.pending_acks.lock().unwrap().contains_key(req_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("pending_acks never registered for {req_id}");
}

// P2: an ack within the probe window advances to phase 2 (unbounded wait),
// where a later submit resolves the request normally.
#[tokio::test]
async fn local_ack_enters_wait_then_submit_resolves() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval_inner(approval_req("r1"), Duration::from_secs(2), None)
            .await
    });
    wait_for_pending_ack(&hub, "r1").await;

    assert!(hub.notify_approval_ack("r1"), "ack must hit the probe");
    // Now in phase 2 (no timeout). Submit after a short delay.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: false,
        }
    ));
    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(resp.approved);
    assert_eq!(hub.pending_replay_count(), 0);
    assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
}

// P2 (codex #1): zero ack within the probe window denies, clears all
// daemon-self bookkeeping, and broadcasts Finished so any created windows die.
#[tokio::test]
async fn local_probe_timeout_denies_and_broadcasts_finished() {
    let hub = HostControlHub::new_local();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval_inner(approval_req("r1"), Duration::from_millis(50), None)
            .await
    });

    // The initial Request is broadcast.
    match tokio::time::timeout(Duration::from_millis(200), outbound_rx.recv())
        .await
        .expect("Request must broadcast")
        .expect("channel ok")
    {
        HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
        other => panic!("expected Request, got {other:?}"),
    }

    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(!resp.approved, "probe timeout must deny");

    // Finished is broadcast so per-monitor windows get destroyed.
    match tokio::time::timeout(Duration::from_millis(200), outbound_rx.recv())
        .await
        .expect("Finished must broadcast")
        .expect("channel ok")
    {
        HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
        other => panic!("expected Finished, got {other:?}"),
    }

    assert_eq!(hub.pending_replay_count(), 0);
    assert!(hub.inner.pending_approvals.lock().unwrap().is_empty());
    assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
}

// P2 (codex #2): ack is idempotent. The first ack fires the probe oneshot;
// a replayed ack after the request has entered phase 2 still reports ready
// (pending_approvals still holds it).
#[tokio::test]
async fn notify_approval_ack_idempotent_in_phase2() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval_inner(approval_req("r1"), Duration::from_secs(2), None)
            .await
    });
    wait_for_pending_ack(&hub, "r1").await;

    assert!(hub.notify_approval_ack("r1"), "first ack fires the probe");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        hub.notify_approval_ack("r1"),
        "replayed ack in phase 2 must still be ready"
    );

    assert!(hub.submit_approval("r1", ApprovalResponse::deny()));
    let _ = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
}

// P2 (codex #3): worker-originated requests are "ready" without creating a
// probe, so the shared approval page does not break them; the directional
// route remains intact for submit.
#[test]
fn notify_approval_ack_worker_originated_is_ready_without_probe() {
    let hub = HostControlHub::new_aggregator();
    let (tx, _rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(9, tx);
    hub.register_upstream_request("r-w".to_string(), 9, SecurityPermissionType::Terminal, None);

    assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
    assert!(
        hub.notify_approval_ack("r-w"),
        "worker req must report ready"
    );
    // No probe was created.
    assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
    // Directional route still resolves.
    assert_eq!(hub.pop_upstream_for_req("r-w"), Some(9));
}

// P2: a truly unknown req_id is not ready.
#[test]
fn notify_approval_ack_unknown_returns_false() {
    let hub = HostControlHub::new_local();
    assert!(!hub.notify_approval_ack("ghost"));
}

// P2 (codex #2/four-round): a direct submit inside the probe window must win
// over the probe deny. submit_approval never touches pending_acks, so the
// select! `rx` arm is the only ready arm. Looped to shake out select!
// randomness.
#[tokio::test]
async fn direct_submit_during_probe_wins_over_deny() {
    for _ in 0..20 {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task = tokio::spawn(async move {
            hub_clone
                .request_approval_inner(approval_req("r1"), Duration::from_secs(2), None)
                .await
        });
        wait_for_pending_ack(&hub, "r1").await;

        // Direct submit, no ack.
        assert!(hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            }
        ));
        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(resp.approved, "direct submit result must win over deny");
        assert!(
            hub.inner.pending_acks.lock().unwrap().is_empty(),
            "pending_acks must not leak"
        );
    }
}

// P2 (codex #1, three-round): Forwarder never registers a readiness probe and
// resolves via the upstream-delivered submit (the worker is authoritative).
#[tokio::test]
async fn forwarder_request_registers_no_pending_acks() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let upstream_clone = Arc::clone(&upstream);
    let hub = HostControlHub::new_forwarder(upstream);

    let hub_clone = hub.clone();
    let task =
        tokio::spawn(async move { hub_clone.request_approval(approval_req("r1"), None).await });
    for _ in 0..50 {
        if hub
            .inner
            .pending_approvals
            .lock()
            .unwrap()
            .contains_key("r1")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        hub.inner.pending_acks.lock().unwrap().is_empty(),
        "Forwarder must not create a readiness probe"
    );

    upstream_clone.test_inject_inbound(HostControlMessage::SecurityApprovalSubmit {
        req_id: "r1".to_string(),
        approved: true,
        remember: false,
    });
    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(resp.approved);
}

// P2: deny_all_pending also drops any in-flight readiness probes.
#[tokio::test]
async fn deny_all_pending_clears_pending_acks() {
    let hub = HostControlHub::new_local();
    let _outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval_inner(approval_req("r1"), Duration::from_secs(5), None)
            .await
    });
    wait_for_pending_ack(&hub, "r1").await;

    hub.deny_all_pending();
    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(!resp.approved);
    assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
}

// Timeout translation: Some(0) = never; Some(n) = n + grace; None must
// collapse to the finite default, NEVER fail open to an unbounded wait.
#[test]
fn server_approval_timeout_translation() {
    assert_eq!(server_approval_timeout(Some(0)), None, "0 = never");
    assert_eq!(
        server_approval_timeout(Some(5)),
        Some(Duration::from_secs(5) + APPROVAL_SERVER_GRACE),
        "n>0 = n + grace"
    );
    assert_eq!(
        server_approval_timeout(None),
        Some(Duration::from_secs(DEFAULT_APPROVAL_TIMEOUT_SECS as u64) + APPROVAL_SERVER_GRACE),
        "None = default + grace"
    );
    assert!(
        server_approval_timeout(None).is_some(),
        "None must never fail open to an unbounded wait"
    );
}

// Daemon-self Phase 2 authoritative timeout: no user response → fail-closed
// deny, all bookkeeping cleared, exactly one Finished broadcast.
#[tokio::test]
async fn daemon_self_phase2_timeout_denies_cleans_and_finishes() {
    let hub = HostControlHub::new_local();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval_inner(
                approval_req("r1"),
                Duration::from_secs(2),
                Some(Duration::from_millis(60)),
            )
            .await
    });
    wait_for_pending_ack(&hub, "r1").await;
    assert!(hub.notify_approval_ack("r1"), "ack advances to phase 2");

    // No submit → the authoritative timeout must fire.
    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(!resp.approved, "phase 2 timeout must deny");

    let mut finished = 0;
    while let Ok(Ok(msg)) =
        tokio::time::timeout(Duration::from_millis(50), outbound_rx.recv()).await
    {
        if let HostControlMessage::SecurityApprovalFinished { req_id } = msg {
            assert_eq!(req_id, "r1");
            finished += 1;
        }
    }
    assert_eq!(finished, 1, "exactly one Finished on timeout");
    assert_eq!(hub.pending_replay_count(), 0);
    assert!(hub.inner.pending_approvals.lock().unwrap().is_empty());
    assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
}

// Timeout/submit arbitration: a submit before the timeout wins (biased
// select), and the timer arm must NOT also fire a second Finished.
#[tokio::test]
async fn daemon_self_submit_before_timeout_wins_single_finished() {
    let hub = HostControlHub::new_local();
    let mut outbound_rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval_inner(
                approval_req("r1"),
                Duration::from_secs(2),
                Some(Duration::from_millis(300)),
            )
            .await
    });
    wait_for_pending_ack(&hub, "r1").await;
    assert!(hub.notify_approval_ack("r1"));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(hub.submit_approval(
        "r1",
        ApprovalResponse {
            approved: true,
            remember: false,
        }
    ));
    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(
        resp.approved,
        "submit before timeout must win over a later deny"
    );

    // Wait past the configured timeout to prove the timer arm stayed dormant.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut finished = 0;
    while let Ok(Ok(msg)) =
        tokio::time::timeout(Duration::from_millis(50), outbound_rx.recv()).await
    {
        if matches!(msg, HostControlMessage::SecurityApprovalFinished { .. }) {
            finished += 1;
        }
    }
    assert_eq!(
        finished, 1,
        "submit emits one Finished; the timer must not double it"
    );
    assert!(hub.inner.pending_approvals.lock().unwrap().is_empty());
}

// Aggregator worker-originated cleanup: a forwarder's SecurityApprovalResolved
// tears down routing/replay + closes the dialog, but only when the sending
// session owns the req_id.
#[test]
fn resolve_upstream_request_enforces_ownership_and_finishes() {
    let hub = HostControlHub::new_aggregator();
    let mut outbound_rx = hub.subscribe_outbound();
    let (tx, _rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(9, tx);
    hub.register_upstream_request("r-w".to_string(), 9, SecurityPermissionType::Terminal, None);
    assert_eq!(hub.pending_replay_count(), 1);

    // A non-owning session must not resolve it.
    assert!(
        !hub.resolve_upstream_request("r-w", 8),
        "non-owner must not resolve another session's request"
    );
    assert_eq!(hub.pending_replay_count(), 1, "route/replay preserved");

    // The owning session resolves: routes/replay cleared + Finished broadcast.
    assert!(hub.resolve_upstream_request("r-w", 9));
    assert_eq!(hub.pending_replay_count(), 0);
    assert!(hub.pop_upstream_for_req("r-w").is_none(), "route removed");
    let mut saw_finished = false;
    while let Ok(msg) = outbound_rx.try_recv() {
        if let HostControlMessage::SecurityApprovalFinished { req_id } = msg {
            assert_eq!(req_id, "r-w");
            saw_finished = true;
        }
    }
    assert!(
        saw_finished,
        "owner-resolve must broadcast Finished to close the dialog"
    );
}

// Forwarder (worker) authoritative timeout: denies fail-closed and tells the
// aggregator to clean up via SecurityApprovalResolved.
#[tokio::test]
async fn forwarder_timeout_denies_and_notifies_aggregator() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let mut outbound_rx = upstream.test_outbound_rx();
    let hub = HostControlHub::new_forwarder(upstream);

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move {
        hub_clone
            .request_approval(approval_req("r1"), Some(Duration::from_millis(40)))
            .await
    });
    // No inbound submit → the worker's own timeout must fire.
    let resp = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(!resp.approved, "forwarder timeout must deny");

    // Worker sent the Request upstream, then SecurityApprovalResolved on timeout.
    let mut saw_resolved = false;
    while let Ok(msg) = outbound_rx.try_recv() {
        if let HostControlMessage::SecurityApprovalResolved { req_id } = msg {
            assert_eq!(req_id, "r1");
            saw_resolved = true;
        }
    }
    assert!(
        saw_resolved,
        "worker must tell the aggregator to clean up on timeout"
    );
    assert!(
        hub.inner.pending_approvals.lock().unwrap().is_empty(),
        "worker-local pending must be cleared"
    );
}

// A forwarder disconnect must close the Tauri dialogs for its drained
// requests: the originating worker is gone and its SecurityApprovalResolved
// may never arrive, so the aggregator broadcasts Finished itself.
#[test]
fn drain_upstream_pending_broadcasts_finished() {
    let hub = HostControlHub::new_aggregator();
    let mut outbound_rx = hub.subscribe_outbound();
    let (tx, _rx) = mpsc::unbounded_channel();
    hub.register_forwarder_session(7, tx);
    hub.register_upstream_request("r-w".to_string(), 7, SecurityPermissionType::Terminal, None);
    assert_eq!(hub.pending_replay_count(), 1);

    let drained = hub.drain_upstream_pending(7);
    assert_eq!(drained, vec!["r-w".to_string()]);
    assert_eq!(hub.pending_replay_count(), 0, "routes/replay cleared");

    let mut saw_finished = false;
    while let Ok(msg) = outbound_rx.try_recv() {
        if let HostControlMessage::SecurityApprovalFinished { req_id } = msg {
            assert_eq!(req_id, "r-w");
            saw_finished = true;
        }
    }
    assert!(saw_finished, "disconnect drain must close the Tauri dialog");
}

// Exactly-once decision invariant: a submit that reports success must never be
// silently downgraded to a timeout deny (the remove→send window). The timeout
// arm, on finding the entry already claimed, awaits the submit's decision
// instead of reading a still-empty channel. Loops to exercise both orderings.
#[tokio::test]
async fn daemon_self_successful_submit_is_never_downgraded_by_timeout() {
    for _ in 0..40 {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task = tokio::spawn(async move {
            hub_clone
                .request_approval_inner(
                    approval_req("r1"),
                    Duration::from_secs(2),
                    Some(Duration::from_millis(15)),
                )
                .await
        });
        wait_for_pending_ack(&hub, "r1").await;
        assert!(hub.notify_approval_ack("r1"));
        // Submit right around the timeout to race the claim and the timer.
        tokio::time::sleep(Duration::from_millis(15)).await;
        let submitted = hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            },
        );
        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        // If the submit was accepted, the decision must be the user's approve —
        // never a spurious timeout deny.
        if submitted {
            assert!(
                resp.approved,
                "an accepted submit must not be downgraded to deny by the timeout"
            );
        }
    }
}
