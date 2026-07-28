use super::*;
use desk_ipc_protocol::dual_transport::inprocess;
use desk_ipc_protocol::message::{
    ForceKeyframePayload, HeartbeatPayload, ServiceToWorker, WorkerToService,
};

/// `bridge_event_transport` shuttles a daemon command (cmd_rx →
/// EventSender) onto the worker's event transport. Verifies the
/// happy path before going on to lifecycle tests below.
#[tokio::test]
async fn bridge_forwards_cmd_to_worker() {
    let (s2w_tx, mut s2w_rx) = inprocess::make_event::<ServiceToWorker>();
    let (_w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
    let (raw_msg_tx, _msg_rx) = mpsc::unbounded_channel::<WorkerMessage>();
    let msg_tx = WorkerMessageSink::for_test(7, raw_msg_tx);

    let handle = tokio::spawn(async move {
        bridge_event_transport(w2s_rx, s2w_tx, &mut cmd_rx, &msg_tx, "bridge-test").await
    });

    cmd_tx
        .send(ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
            connection_id: "c1".to_string(),
        }))
        .expect("cmd send");

    let received = tokio::time::timeout(tokio::time::Duration::from_secs(1), s2w_rx.recv())
        .await
        .expect("worker should receive cmd quickly")
        .expect("transport open");
    assert!(matches!(received, ServiceToWorker::ForceKeyframe(_)));

    // Drop cmd_tx → bridge observes None on cmd channel and exits
    // (daemon-initiated shutdown).
    drop(cmd_tx);
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
        .await
        .expect("bridge must exit on cmd channel close")
        .expect("task did not panic");
    assert!(result, "cmd channel close counts as daemon-initiated");
}

/// `bridge_event_transport` forwards worker → daemon messages (worker
/// EventSender → daemon msg_tx). Daemon-side msg_rx must observe the
/// payload in order without re-encoding.
#[tokio::test]
async fn bridge_forwards_worker_msg_to_daemon() {
    let (s2w_tx, _s2w_rx) = inprocess::make_event::<ServiceToWorker>();
    let (w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
    let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
    let (raw_msg_tx, mut msg_rx) = mpsc::unbounded_channel::<WorkerMessage>();
    let msg_tx = WorkerMessageSink::for_test(7, raw_msg_tx);
    let stamp = msg_tx.incarnation();

    let handle = tokio::spawn(async move {
        bridge_event_transport(w2s_rx, s2w_tx, &mut cmd_rx, &msg_tx, "bridge-test").await
    });

    w2s_tx
        .send(WorkerToService::Heartbeat(HeartbeatPayload {
            timestamp_ms: 42,
            active_connections: 0,
            cpu_usage: None,
            memory_usage: None,
        }))
        .await
        .expect("worker send");

    let observed = tokio::time::timeout(tokio::time::Duration::from_secs(1), msg_rx.recv())
        .await
        .expect("daemon should observe worker msg")
        .expect("daemon msg channel open");
    assert_eq!(
        observed.incarnation, stamp,
        "the bridge stamps every message with the worker it serves"
    );
    match observed.message {
        WorkerToService::Heartbeat(p) => assert_eq!(p.timestamp_ms, 42),
        other => panic!("expected Heartbeat, got {other:?}"),
    }

    // Drop the worker EventSender (mpsc closes) → bridge observes
    // None on the worker side and exits with `daemon_initiated=false`
    // (worker disconnected first; outer caller would trigger
    // crash-recovery in the named-pipe path).
    drop(w2s_tx);
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
        .await
        .expect("bridge must exit on worker close")
        .expect("task did not panic");
    assert!(
        !result,
        "worker close means worker initiated; daemon should treat as crash"
    );
}

/// `Shutdown` command sent by the daemon must mark `daemon_initiated`
/// even when the worker side is still alive — that's the signal the
/// named-pipe `run_pipe_server` uses to skip crash-recovery.
#[tokio::test]
async fn bridge_shutdown_cmd_marks_daemon_initiated() {
    let (s2w_tx, mut s2w_rx) = inprocess::make_event::<ServiceToWorker>();
    let (_w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
    let (raw_msg_tx, _msg_rx) = mpsc::unbounded_channel::<WorkerMessage>();
    let msg_tx = WorkerMessageSink::for_test(7, raw_msg_tx);

    let handle = tokio::spawn(async move {
        bridge_event_transport(w2s_rx, s2w_tx, &mut cmd_rx, &msg_tx, "bridge-test").await
    });

    cmd_tx
        .send(ServiceToWorker::Shutdown)
        .expect("send Shutdown");

    // Worker side must observe the Shutdown.
    let observed = tokio::time::timeout(tokio::time::Duration::from_secs(1), s2w_rx.recv())
        .await
        .expect("worker should receive Shutdown")
        .expect("transport open");
    assert!(matches!(observed, ServiceToWorker::Shutdown));

    // Drop cmd_tx so the bridge exits (Shutdown doesn't itself break
    // the loop — it just flips the flag; the loop ends on cmd close
    // or worker close).
    drop(cmd_tx);
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
        .await
        .expect("bridge must exit")
        .expect("task did not panic");
    assert!(result, "Shutdown cmd must mark daemon-initiated");
}
