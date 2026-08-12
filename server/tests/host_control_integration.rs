//! Integration tests for the Host Control Hub.
//!
//! These exercise the hub through its public API only — protocol round-trip,
//! aggregator routing across two upstreams, and the failure-mode contracts.
//! They do not stand up a real ws server (those are unit-tested in
//! `host_control::endpoint`); the goal here is to lock down behaviour that
//! emerges from the hub + upstream combination across mode boundaries.

use std::sync::Arc;
use std::time::Duration;

use lcxl_remote_desk_server::host_control::{
    ApprovalRequest, ApprovalResponse, HostControlHub, HostControlMessage, UpstreamForwarder,
};
use lcxl_remote_desk_server::model::security_approval::SecurityPermissionType;

fn req(id: &str) -> ApprovalRequest {
    ApprovalRequest {
        req_id: id.to_string(),
        permission_type: SecurityPermissionType::RemoteControl,
        from_connection_id: Some("conn-1".to_string()),
    }
}

/// I-2 (adapted): Local hub + simulated Tauri responder — request_approval
/// completes when submit_approval is invoked.
#[tokio::test]
async fn integration_local_request_approval_resolved_via_submit() {
    let hub = Arc::new(HostControlHub::new_local());
    let mut rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let hub_clone = Arc::clone(&hub);
    tokio::spawn(async move {
        if let Ok(HostControlMessage::SecurityApprovalRequest { req_id, .. }) = rx.recv().await {
            hub_clone.submit_approval(
                &req_id,
                ApprovalResponse {
                    approved: true,
                    remember: false,
                },
            );
        }
    });

    let resp = tokio::time::timeout(
        Duration::from_secs(2),
        hub.request_approval(req("r1"), None),
    )
    .await
    .expect("must resolve");
    assert!(resp.approved);
    assert!(!resp.remember);
}

/// I-6 (adapted): five concurrent approvals on a Local hub, submit them out of
/// order — each task receives its own response.
#[tokio::test]
async fn integration_concurrent_local_approvals_out_of_order_submit() {
    let hub = Arc::new(HostControlHub::new_local());
    let _rx = hub.subscribe_outbound();
    hub.mark_tauri_connected();

    let mut tasks = Vec::new();
    for i in 0u32..5 {
        let hub_c = Arc::clone(&hub);
        tasks.push(tokio::spawn(async move {
            (i, hub_c.request_approval(req(&format!("r{i}")), None).await)
        }));
    }
    // Wait for all to enter pending.
    for _ in 0..50 {
        if hub.pending_replay_count() == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Out-of-order submits.
    for i in [3u32, 0, 4, 1, 2] {
        hub.submit_approval(
            &format!("r{i}"),
            ApprovalResponse {
                approved: i.is_multiple_of(2),
                remember: false,
            },
        );
    }

    for t in tasks {
        let (i, resp) = t.await.unwrap();
        assert_eq!(resp.approved, i.is_multiple_of(2));
    }
    assert_eq!(hub.pending_replay_count(), 0);
}

/// I-10 (adapted): Forwarder hub with offline upstream — request_approval
/// returns deny without blocking, send_command returns 0.
#[tokio::test]
async fn integration_forwarder_offline_fails_fast() {
    let upstream = UpstreamForwarder::new_for_test(false);
    let hub = HostControlHub::new_forwarder(upstream);

    let started = std::time::Instant::now();
    let resp = hub.request_approval(req("r1"), None).await;
    assert!(!resp.approved);
    assert!(started.elapsed() < Duration::from_millis(100));

    let n = hub
        .send_command(HostControlMessage::PrivateScreenShow {
            connection_id: "c1".to_string(),
            request_id: "r-show".to_string(),
        })
        .expect("send_command should not error");
    assert_eq!(n, 0);
}

/// I-17 (adapted): Aggregator with two forwarders; cancel-on-tauri-loss routes
/// SecurityApprovalCancel to the originating forwarder only — the other
/// forwarder must not see the unrelated cancel.
#[tokio::test]
async fn integration_aggregator_cancel_routes_per_forwarder() {
    use tokio::sync::mpsc;

    let hub = HostControlHub::new_aggregator();
    let (tx_a, mut rx_a) = mpsc::unbounded_channel();
    let (tx_b, mut rx_b) = mpsc::unbounded_channel();
    hub.register_forwarder_session(10, tx_a);
    hub.register_forwarder_session(20, tx_b);

    hub.register_upstream_request(
        "alpha".to_string(),
        10,
        SecurityPermissionType::Whiteboard,
        None,
    );
    hub.register_upstream_request(
        "beta".to_string(),
        20,
        SecurityPermissionType::FileTransfer,
        None,
    );

    // Tauri lost — both forwarders should be cancelled.
    let cancelled = hub.cancel_all_for_tauri_loss();
    assert_eq!(cancelled.len(), 2);

    // Each forwarder sees only its own cancel.
    let m_a = tokio::time::timeout(Duration::from_millis(200), rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    match m_a {
        HostControlMessage::SecurityApprovalCancel { req_id } => assert_eq!(req_id, "alpha"),
        other => panic!("unexpected: {other:?}"),
    }
    let m_b = tokio::time::timeout(Duration::from_millis(200), rx_b.recv())
        .await
        .unwrap()
        .unwrap();
    match m_b {
        HostControlMessage::SecurityApprovalCancel { req_id } => assert_eq!(req_id, "beta"),
        other => panic!("unexpected: {other:?}"),
    }

    // Forwarder a must NOT have received beta's cancel and vice-versa.
    assert!(rx_a.try_recv().is_err());
    assert!(rx_b.try_recv().is_err());
}

/// Full forwarder loop: Tauri-side responder over the upstream channel resolves
/// a worker-side request_approval. Exercises the inbound dispatcher inside the
/// Forwarder hub end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_forwarder_inbound_resolves_request() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let upstream_clone = Arc::clone(&upstream);
    let hub = HostControlHub::new_forwarder(upstream);

    let hub_clone = hub.clone();
    let task = tokio::spawn(async move { hub_clone.request_approval(req("u1"), None).await });
    // Wait for the request to enter pending.
    tokio::time::sleep(Duration::from_millis(50)).await;

    upstream_clone.test_inject_inbound(HostControlMessage::SecurityApprovalSubmit {
        req_id: "u1".to_string(),
        approved: true,
        remember: true,
    });

    let resp = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("must resolve")
        .unwrap();
    assert!(resp.approved && resp.remember);
}

/// Forwarder upstream goes offline mid-flight — every pending oneshot is
/// resolved as deny via the disconnect watcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_forwarder_disconnect_denies_all_pending() {
    let upstream = UpstreamForwarder::new_for_test(true);
    let upstream_clone = Arc::clone(&upstream);
    let hub = HostControlHub::new_forwarder(upstream);

    let hub_a = hub.clone();
    let hub_b = hub.clone();
    let t_a = tokio::spawn(async move { hub_a.request_approval(req("a"), None).await });
    let t_b = tokio::spawn(async move { hub_b.request_approval(req("b"), None).await });
    // Give both requests time to park.
    tokio::time::sleep(Duration::from_millis(50)).await;

    upstream_clone.mark_disconnected();

    let r_a = tokio::time::timeout(Duration::from_secs(2), t_a)
        .await
        .unwrap()
        .unwrap();
    let r_b = tokio::time::timeout(Duration::from_secs(2), t_b)
        .await
        .unwrap()
        .unwrap();
    assert!(!r_a.approved && !r_b.approved);
}
