use super::*;
use crate::worker::file_transfer_dispatcher::download::DownloadPlan;
use desk_ipc_protocol::dual_transport::{EventReceiver, inprocess};
use desk_ipc_protocol::message::MediaCodec;
use desk_signal_facade::model::security_settings::SecuritySettings;
use tempfile::TempDir;

// ============== ft-metrics helpers ==============

/// `throughput_mbps` returns 0 when wall time is zero, matching the
/// "no samples yet" case. Without this guard the first emit on a
/// freshly-reset window would compute 0/0 and print `NaN MB/s`,
/// which is meaningless and triggers a downstream log-parsing
/// surprise.
#[test]
fn throughput_mbps_zero_wall_returns_zero() {
    assert_eq!(throughput_mbps(0, 0), 0.0);
    assert_eq!(throughput_mbps(60 * 1024, 0), 0.0);
}

/// `throughput_mbps` against a known synthetic sample:
/// 1 MB transferred in exactly 1 second = 1.048576 MB/s in
/// binary-megabyte terms, but we use *decimal* MB (matches the
/// browser UI in `use-file-transfer.ts`), so 1 048 576 bytes /
/// 1 s = 1.048576 MB/s. Pick a round-number sample so the test
/// is obviously correct on inspection: 10 MB (decimal) in 1 s
/// should be exactly 10 MB/s.
#[test]
fn throughput_mbps_known_sample() {
    let bytes = 10 * 1_000_000;
    let wall_ns = 1_000_000_000; // 1 s
    let result = throughput_mbps(bytes, wall_ns);
    assert!(
        (result - 10.0).abs() < 1e-9,
        "expected 10 MB/s, got {result}"
    );
}

/// `duration_ns` saturates rather than overflowing on absurd
/// inputs — guards against an inadvertent `u128 → u64` panic if
/// a future caller passes a `Duration::MAX`.
#[test]
fn duration_ns_saturates() {
    assert_eq!(duration_ns(Duration::from_nanos(1)), 1);
    assert_eq!(duration_ns(Duration::ZERO), 0);
    // Duration::MAX > u64::MAX nanos — must saturate, not panic.
    assert_eq!(duration_ns(Duration::MAX), u64::MAX);
}

// ============== DownloadWindow ==============

/// A fresh window is empty: `chunks == 0`, `is_full() == false`,
/// `flush_line` returns `None`. The `None` flush is what protects
/// the trailing-flush call sites from emitting a useless empty
/// log line when a download has exactly 0 chunks or the loop
/// breaks before the first iteration.
#[test]
fn download_window_empty_flush_is_none() {
    let w = DownloadWindow::default();
    assert_eq!(w.chunks, 0);
    assert!(!w.is_full());
    assert!(w.flush_line("tid", "ft-metrics").is_none());
}

/// Recording one sample updates all stage accumulators and bumps
/// `chunks` / `bytes`. A single chunk is well below the window
/// boundary, so `is_full()` remains `false`.
#[test]
fn download_window_records_single_sample() {
    let mut w = DownloadWindow::default();
    w.record(
        60 * 1024,
        Duration::from_micros(100),
        Duration::from_micros(50),
        Duration::from_millis(2),
        Duration::from_millis(3),
    );
    assert_eq!(w.chunks, 1);
    assert_eq!(w.bytes, 60 * 1024);
    assert_eq!(w.disk_read_ns, 100_000);
    assert_eq!(w.build_chunk_ns, 50_000);
    assert_eq!(w.emit_await_ns, 2_000_000);
    assert_eq!(w.wall_ns, 3_000_000);
    assert!(!w.is_full());
    let line = w
        .flush_line("tid", "ft-metrics")
        .expect("non-empty window must produce a line");
    assert!(line.starts_with("[ft-metrics] tid=tid"));
    assert!(line.contains("chunks=1"));
    assert!(line.contains("bytes=61440"));
}

/// `is_full()` flips at exactly `FT_METRICS_WINDOW_CHUNKS` —
/// guards the inverse off-by-one (firing one chunk too early or
/// too late would shift every metric line by ~60 KB on a 60 KB
/// chunk pipeline).
#[test]
fn download_window_boundary_is_full() {
    let mut w = DownloadWindow::default();
    for _ in 0..(FT_METRICS_WINDOW_CHUNKS - 1) {
        w.record(
            1024,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            Duration::from_nanos(1),
        );
    }
    assert!(!w.is_full(), "one short of the boundary must NOT be full");
    w.record(
        1024,
        Duration::from_nanos(1),
        Duration::from_nanos(1),
        Duration::from_nanos(1),
        Duration::from_nanos(1),
    );
    assert!(w.is_full(), "exactly at the boundary must be full");
}

/// `reset()` clears every field back to the `Default::default()`
/// state so a second window starts clean. Required for the
/// `is_full → flush → reset` cadence in `serve_download` to not
/// accumulate stale totals across windows.
#[test]
fn download_window_reset_clears_state() {
    let mut w = DownloadWindow::default();
    w.record(
        512,
        Duration::from_nanos(10),
        Duration::from_nanos(10),
        Duration::from_nanos(10),
        Duration::from_nanos(10),
    );
    assert!(w.chunks > 0);
    w.reset();
    assert_eq!(w, DownloadWindow::default());
}

// ============== UploadWindow ==============

/// Mirror of `download_window_empty_flush_is_none` for the upload
/// accumulator: the trailing-flush path on `TransferComplete` must
/// be a no-op when the window has never recorded a sample (e.g.
/// an upload that gets cancelled before any chunk arrives).
#[test]
fn upload_window_empty_flush_is_none() {
    let w = UploadWindow::default();
    assert!(w.flush_line("tid", "ft-metrics").is_none());
}

/// A single recorded sample produces a line containing the bytes
/// and lock-wait/disk-write breakdown. The format is asserted
/// loosely (substring) because the exact `.2f` formatting can
/// shift under different locales (we want the metric, not a
/// pixel-perfect format spec).
#[test]
fn upload_window_records_and_flushes() {
    let mut w = UploadWindow::default();
    w.record(
        60 * 1024,
        Duration::from_micros(20),
        Duration::from_millis(1),
        Duration::from_millis(2),
    );
    assert_eq!(w.chunks, 1);
    assert_eq!(w.bytes, 60 * 1024);
    assert_eq!(w.lock_ns, 20_000);
    assert_eq!(w.disk_write_ns, 1_000_000);
    assert_eq!(w.wall_ns, 2_000_000);
    let line = w.flush_line("tid", "ft-metrics").unwrap();
    assert!(line.contains("tid=tid"));
    assert!(line.contains("bytes=61440"));
    assert!(line.contains("lock_wait="));
    assert!(line.contains("disk_write="));
}

/// Upload-side `is_full()` shares the same `FT_METRICS_WINDOW_CHUNKS`
/// boundary as the download side — protects against a refactor that
/// might accidentally diverge the two windows' cadences (which
/// would make the worker / daemon logs harder to correlate by
/// chunk count).
#[test]
fn upload_window_boundary_is_full() {
    let mut w = UploadWindow::default();
    for _ in 0..FT_METRICS_WINDOW_CHUNKS {
        w.record(
            1,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            Duration::from_nanos(1),
        );
    }
    assert!(w.is_full());
    w.reset();
    assert_eq!(w, UploadWindow::default());
}

/// Build a dispatcher whose permission gate auto-passes
/// (`allow_file_transfer = Some(true)`) so tests focus on the
/// dispatch / IO logic rather than the Tauri approval prompt.
/// Tests that need to assert the permission deny path can build a
/// dispatcher with `allow_file_transfer = Some(false)` via
/// `dispatcher_with_setting` instead.
fn dispatcher() -> (
    FileTransferDispatcher,
    Box<dyn EventReceiver<FileTransferPayload>>,
) {
    dispatcher_with_setting(Some(true))
}

/// A dispatcher plus the mirror the daemon would publish to, so a test can move
/// the policy underneath a running gate.
fn dispatcher_with_mirror(
    allow_file_transfer: Option<bool>,
) -> (
    FileTransferDispatcher,
    Arc<crate::worker::policy_mirror::PolicyMirror>,
) {
    let (policy, mirror, _upstream) =
        crate::model::policy_access::PolicyAccess::for_test(SecuritySettings {
            allow_file_transfer,
            ..SecuritySettings::default()
        });
    let (file_tx, _file_rx) = inprocess::make_event_inprocess_with_cap::<FileTransferPayload>(16);
    (
        FileTransferDispatcher::new(
            file_tx,
            policy,
            Arc::new(HostControlHub::new_local()),
            ConnectionCeilingStore::new(),
            mpsc::unbounded_channel().0,
        ),
        mirror,
    )
}

/// The symptom this whole distribution exists for: a capability set to "always
/// deny" has to take effect on a worker that already cached an approval for it.
#[tokio::test]
async fn a_published_denial_expires_a_cached_approval() {
    let (d, mirror) = dispatcher_with_mirror(Some(true));
    d.start_connection(&start_payload("c1")).await;
    let payload = FileTransferPayload {
        connection_id: "c1".into(),
        data: br#"{"type":"transfer_complete","transfer_id":"t"}"#.to_vec(),
        is_text: true,
        transfer_id: None,
    };
    d.handle_command(payload.clone()).await;
    assert_eq!(
        d.inner
            .lock()
            .await
            .permission_cache
            .get("c1")
            .map(|c| c.approved),
        Some(true),
        "the allow is cached to begin with"
    );

    let mut published = mirror.snapshot();
    published.set_capability(SecurityPermissionType::FileTransfer, Some(false));
    mirror.apply(published);

    d.handle_command(payload).await;
    assert_eq!(
        d.inner
            .lock()
            .await
            .permission_cache
            .get("c1")
            .map(|c| c.approved),
        Some(false),
        "the cached approval must not survive the operator's denial"
    );
}

/// Changing one capability must leave answers about the others alone, or every
/// unrelated settings edit would re-prompt the user across the board.
#[tokio::test]
async fn a_change_to_another_capability_leaves_the_cache_alone() {
    let (d, mirror) = dispatcher_with_mirror(Some(true));
    d.start_connection(&start_payload("c1")).await;
    let payload = FileTransferPayload {
        connection_id: "c1".into(),
        data: br#"{"type":"transfer_complete","transfer_id":"t"}"#.to_vec(),
        is_text: true,
        transfer_id: None,
    };
    d.handle_command(payload).await;
    let cached = *d.inner.lock().await.permission_cache.get("c1").unwrap();

    let mut published = mirror.snapshot();
    published.set_capability(SecurityPermissionType::Whiteboard, Some(false));
    mirror.apply(published);

    assert!(
        cached.is_current(
            d.policy
                .capability(SecurityPermissionType::FileTransfer)
                .generation
        ),
        "a whiteboard change must not invalidate a file-transfer answer"
    );
}

fn dispatcher_with_setting(
    allow_file_transfer: Option<bool>,
) -> (
    FileTransferDispatcher,
    Box<dyn EventReceiver<FileTransferPayload>>,
) {
    // Use the default file-lane capacity for general-purpose tests.
    // Tests that need to stress backpressure paths construct their
    // own pair via `dispatcher_with_file_cap`.
    dispatcher_with_file_cap(allow_file_transfer, FILE_QUEUE_CAP_FOR_TESTS_DEFAULT)
}

/// Default cap used by `dispatcher_with_setting`. Mirrors the
/// production `FILE_QUEUE_CAP = 32` — large enough that no test
/// in this module accidentally trips backpressure.
const FILE_QUEUE_CAP_FOR_TESTS_DEFAULT: usize = 256;

fn dispatcher_with_file_cap(
    allow_file_transfer: Option<bool>,
    file_cap: usize,
) -> (
    FileTransferDispatcher,
    Box<dyn EventReceiver<FileTransferPayload>>,
) {
    let (policy, _mirror, _upstream) =
        crate::model::policy_access::PolicyAccess::for_test(SecuritySettings {
            allow_file_transfer,
            ..SecuritySettings::default()
        });
    let hub = Arc::new(HostControlHub::new_local());
    let (file_tx, file_rx) =
        inprocess::make_event_inprocess_with_cap::<FileTransferPayload>(file_cap);
    (
        FileTransferDispatcher::new(
            file_tx,
            policy,
            hub,
            ConnectionCeilingStore::new(),
            mpsc::unbounded_channel().0,
        ),
        file_rx,
    )
}

fn dispatcher_with_activity_sender() -> (
    FileTransferDispatcher,
    mpsc::UnboundedReceiver<WorkerToService>,
) {
    let (policy, _mirror, _upstream) =
        crate::model::policy_access::PolicyAccess::for_test(SecuritySettings {
            allow_file_transfer: Some(true),
            ..SecuritySettings::default()
        });
    let hub = Arc::new(HostControlHub::new_local());
    let (file_tx, _file_rx) = inprocess::make_event_inprocess_with_cap::<FileTransferPayload>(16);
    let (activity_tx, activity_rx) = mpsc::unbounded_channel();
    (
        FileTransferDispatcher::new(
            file_tx,
            policy,
            hub,
            ConnectionCeilingStore::new(),
            activity_tx,
        ),
        activity_rx,
    )
}

#[test]
fn activity_file_name_is_minimized() {
    assert_eq!(
        sanitized_file_name("/private/secret/report.txt"),
        "report.txt"
    );
    assert_eq!(sanitized_file_name("C:\\private\\photo.png"), "photo.png");
    assert_eq!(sanitized_file_name("bad\nname.txt"), "badname.txt");
    assert_eq!(sanitized_file_name("\n"), "unnamed");
    assert_eq!(sanitized_file_name(&"x".repeat(300)).chars().count(), 255);
}

#[tokio::test]
async fn activity_events_are_idempotent_and_ignore_unknown_finish() {
    let (dispatcher, mut events) = dispatcher_with_activity_sender();
    assert!(
        !dispatcher
            .finish_activity("conn", "unknown", FileTransferOutcome::Failed)
            .await
    );
    assert!(events.try_recv().is_err());

    assert!(
        dispatcher
            .start_activity(
                "conn",
                "transfer",
                FileTransferDirection::Download,
                "/secret/report.txt",
                42,
            )
            .await
    );
    assert!(
        !dispatcher
            .start_activity(
                "conn",
                "transfer",
                FileTransferDirection::Download,
                "ignored.txt",
                7,
            )
            .await
    );
    match events.recv().await.unwrap() {
        WorkerToService::FileTransferStarted(payload) => {
            assert_eq!(payload.file_name, "report.txt");
            assert_eq!(payload.total_bytes, 42);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(events.try_recv().is_err());

    assert!(
        dispatcher
            .finish_activity("conn", "transfer", FileTransferOutcome::Completed)
            .await
    );
    assert!(
        !dispatcher
            .finish_activity("conn", "transfer", FileTransferOutcome::Failed)
            .await
    );
    match events.recv().await.unwrap() {
        WorkerToService::FileTransferFinished(payload) => {
            assert_eq!(payload.outcome, FileTransferOutcome::Completed);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn identical_transfer_ids_are_isolated_by_connection() {
    let (dispatcher, mut events) = dispatcher_with_activity_sender();
    assert!(
        dispatcher
            .start_activity(
                "conn-a",
                "shared",
                FileTransferDirection::Upload,
                "a.bin",
                1,
            )
            .await
    );
    assert!(
        dispatcher
            .start_activity(
                "conn-b",
                "shared",
                FileTransferDirection::Download,
                "b.bin",
                2,
            )
            .await
    );
    let _ = events.recv().await.unwrap();
    let _ = events.recv().await.unwrap();

    assert!(
        dispatcher
            .finish_activity("conn-a", "shared", FileTransferOutcome::Completed)
            .await
    );
    match events.recv().await.unwrap() {
        WorkerToService::FileTransferFinished(payload) => {
            assert_eq!(payload.connection_id, "conn-a");
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(
        dispatcher
            .finish_activity("conn-b", "shared", FileTransferOutcome::Cancelled)
            .await
    );
    match events.recv().await.unwrap() {
        WorkerToService::FileTransferFinished(payload) => {
            assert_eq!(payload.connection_id, "conn-b");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn stop_connection_finishes_active_transfer_once() {
    let (dispatcher, mut events) = dispatcher_with_activity_sender();
    dispatcher.start_connection(&start_payload("conn")).await;
    dispatcher
        .start_activity(
            "conn",
            "transfer",
            FileTransferDirection::Upload,
            "photo.png",
            5,
        )
        .await;
    let _ = events.recv().await.unwrap();

    dispatcher
        .stop_connection(&StopMediaPayload {
            connection_id: "conn".to_string(),
        })
        .await;
    match events.recv().await.unwrap() {
        WorkerToService::FileTransferFinished(payload) => {
            assert_eq!(payload.transfer_id, "transfer");
            assert_eq!(payload.outcome, FileTransferOutcome::Cancelled);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    dispatcher
        .stop_connection(&StopMediaPayload {
            connection_id: "conn".to_string(),
        })
        .await;
    assert!(events.try_recv().is_err());
}

/// Helper: assert the file lane has no pending payload within a
/// short window. `EventReceiver` is async-only (`recv -> Option<M>`)
/// so we approximate `try_recv` with a tiny timeout.
async fn assert_no_message(rx: &mut Box<dyn EventReceiver<FileTransferPayload>>) {
    let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(
        res.is_err(),
        "expected file lane to be empty but got a message"
    );
}

fn start_payload(connection_id: &str) -> StartMediaPayload {
    StartMediaPayload {
        connection_id: connection_id.to_string(),
        video_codec: MediaCodec::H264,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 30,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: None,
        enable_dirty_rect: None,
    }
}

/// A command for an unknown connection_id (never received
/// StartMedia) is silently dropped; no IPC emitted.
#[tokio::test]
async fn handle_command_drops_for_inactive_connection() {
    let (d, mut rx) = dispatcher();
    let payload = FileTransferPayload {
        connection_id: "ghost".into(),
        data: br#"{"type":"download_request","transfer_id":"t","file_path":"x"}"#.to_vec(),
        is_text: true,
        transfer_id: None,
    };
    d.handle_command(payload).await;
    // Yield then assert nothing arrived: the spawned download
    // task (if any) had a window to emit; an empty file lane
    // means the liveness gate held.
    tokio::task::yield_now().await;
    assert_no_message(&mut rx).await;
}

/// `start_connection` then `stop_connection` flips active_connections.
#[tokio::test]
async fn start_then_stop_releases_state() {
    let (d, _rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    {
        let g = d.inner.lock().await;
        assert!(g.active_connections.contains("c1"));
    }
    d.stop_connection(&StopMediaPayload {
        connection_id: "c1".into(),
    })
    .await;
    let g = d.inner.lock().await;
    assert!(!g.active_connections.contains("c1"));
}

/// `shutdown` clears active_connections and any in-flight upload state.
#[tokio::test]
async fn shutdown_clears_state() {
    let (d, _rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    d.shutdown().await;
    let g = d.inner.lock().await;
    assert!(g.active_connections.is_empty());
    assert!(g.upload_states.is_empty());
    assert!(g.cancelled_transfers.is_empty());
    assert!(g.permission_cache.is_empty());
}

/// Read the one message waiting on the file lane, or fail.
async fn expect_message(
    rx: &mut Box<dyn EventReceiver<FileTransferPayload>>,
) -> FileTransferMessage {
    let payload = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("file lane produced no message")
        .expect("file lane closed");
    serde_json::from_slice(&payload.data).expect("emitted payload is a file-transfer message")
}

/// Assert the lane carries exactly one `TransferError` for `transfer_id`
/// with `error_code`, and nothing after it.
async fn expect_only_transfer_error(
    rx: &mut Box<dyn EventReceiver<FileTransferPayload>>,
    transfer_id: &str,
    error_code: DeskErrorCode,
) {
    match expect_message(rx).await {
        FileTransferMessage::TransferError(error) => {
            assert_eq!(error.transfer_id, transfer_id);
            assert_eq!(error.error_code, error_code);
            assert!(!error.message.is_empty(), "the reason must be reported");
        }
        other => panic!("expected TransferError, got {other:?}"),
    }
    assert_no_message(rx).await;
}

/// Permission gate: when `allow_file_transfer` is `Some(false)`, a command on
/// an active connection is refused rather than dropped. The browser has
/// already drawn the transfer and learns of the refusal only from this reply —
/// dropping it silently pinned the progress bar at 0% forever.
#[tokio::test]
async fn handle_command_refuses_when_permission_denied() {
    let (d, mut rx) = dispatcher_with_setting(Some(false));
    d.start_connection(&start_payload("c1")).await;
    let payload = FileTransferPayload {
        connection_id: "c1".into(),
        data: br#"{"type":"download_request","transfer_id":"t","file_path":"x"}"#.to_vec(),
        is_text: true,
        transfer_id: None,
    };
    d.handle_command(payload).await;
    tokio::task::yield_now().await;
    expect_only_transfer_error(&mut rx, "t", DeskErrorCode::PERMISSION_ERROR).await;
    // Refusing must not open the file or register any transfer state.
    {
        let g = d.inner.lock().await;
        assert!(g.activities.is_empty(), "a refused command starts nothing");
        assert!(g.upload_states.is_empty());
        // Cache is populated so subsequent commands short-circuit.
        assert_eq!(
            g.permission_cache.get("c1").map(|c| c.approved),
            Some(false)
        );
    }
}

/// A binary chunk carries its transfer id in the header, so a denied upload
/// chunk is refused by id just like a control frame.
#[tokio::test]
async fn handle_command_refuses_denied_binary_chunk_by_header_id() {
    let (d, mut rx) = dispatcher_with_setting(Some(false));
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "01234567-89ab-cdef-0123-456789abcdef";
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: build_binary_chunk(transfer_id, 0, b"payload"),
        is_text: false,
        transfer_id: None,
    })
    .await;
    tokio::task::yield_now().await;
    expect_only_transfer_error(&mut rx, transfer_id, DeskErrorCode::PERMISSION_ERROR).await;
}

/// The refusal repeats for every command, not just the first: the denial is
/// cached, and a browser that retries must keep getting an answer.
#[tokio::test]
async fn every_denied_command_is_refused_not_just_the_first() {
    let (d, mut rx) = dispatcher_with_setting(Some(false));
    d.start_connection(&start_payload("c1")).await;
    for transfer_id in ["t1", "t2"] {
        d.handle_command(FileTransferPayload {
            connection_id: "c1".into(),
            data: format!(
                r#"{{"type":"download_request","transfer_id":"{transfer_id}","file_path":"x"}}"#
            )
            .into_bytes(),
            is_text: true,
            transfer_id: None,
        })
        .await;
        tokio::task::yield_now().await;
        expect_only_transfer_error(&mut rx, transfer_id, DeskErrorCode::PERMISSION_ERROR).await;
    }
}

/// A denied frame whose transfer id cannot be recovered cannot be answered on
/// its own, so every transfer the connection has open is failed instead —
/// leaving them unanswered is the same silent drop in another disguise.
#[tokio::test]
async fn denied_unattributable_frame_fails_the_connection_transfers() {
    let (d, mut rx) = dispatcher_with_setting(Some(false));
    d.start_connection(&start_payload("c1")).await;
    d.start_activity(
        "c1",
        "in-flight",
        FileTransferDirection::Download,
        "photo.png",
        10,
    )
    .await;

    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: b"{ not json".to_vec(),
        is_text: true,
        transfer_id: None,
    })
    .await;
    tokio::task::yield_now().await;
    expect_only_transfer_error(&mut rx, "in-flight", DeskErrorCode::INVALID_PARAMS).await;
    assert!(
        d.inner.lock().await.activities.is_empty(),
        "the failed transfer is no longer active"
    );
}

/// The same protocol error with nothing in flight has nothing to answer, and
/// must not invent a transfer to fail.
#[tokio::test]
async fn unattributable_frame_with_no_transfers_emits_nothing() {
    let (d, mut rx) = dispatcher_with_setting(Some(true));
    d.start_connection(&start_payload("c1")).await;
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: vec![0x00, 0x01],
        is_text: false,
        transfer_id: None,
    })
    .await;
    tokio::task::yield_now().await;
    assert_no_message(&mut rx).await;
}

/// Capability gate: a redeemed-grant connection whose ceiling denies
/// file transfer is refused even when the host global allows it — the ceiling
/// meets the global and can only tighten. Closes the file-transfer
/// second-connection escape for a capped grant.
#[tokio::test]
async fn handle_command_refuses_when_ceiling_denies_file_transfer() {
    use desk_signal_facade::model::security_settings::SecuritySettings;
    let (d, mut rx) = dispatcher_with_setting(Some(true));
    d.connection_ceilings
        .set(
            "c1",
            Some(SecuritySettings {
                allow_file_transfer: Some(false),
                ..Default::default()
            }),
        )
        .await;
    d.start_connection(&start_payload("c1")).await;
    let payload = FileTransferPayload {
        connection_id: "c1".into(),
        data: br#"{"type":"download_request","transfer_id":"t","file_path":"x"}"#.to_vec(),
        is_text: true,
        transfer_id: None,
    };
    d.handle_command(payload).await;
    tokio::task::yield_now().await;
    expect_only_transfer_error(&mut rx, "t", DeskErrorCode::PERMISSION_ERROR).await;
    let g = d.inner.lock().await;
    assert_eq!(
        g.permission_cache.get("c1").map(|c| c.approved),
        Some(false)
    );
}

/// Permission gate: when `allow_file_transfer` is `Some(true)`, the
/// dispatcher routes commands normally and caches the decision.
#[tokio::test]
async fn handle_command_caches_allowed_permission() {
    let (d, _rx) = dispatcher_with_setting(Some(true));
    d.start_connection(&start_payload("c1")).await;
    let payload = FileTransferPayload {
        connection_id: "c1".into(),
        data: br#"{"type":"transfer_complete","transfer_id":"t"}"#.to_vec(),
        is_text: true,
        transfer_id: None,
    };
    d.handle_command(payload).await;
    let g = d.inner.lock().await;
    assert_eq!(g.permission_cache.get("c1").map(|c| c.approved), Some(true));
}

/// Permission cache is wiped on `stop_connection` so a future
/// connection reuse with the same id re-prompts (or re-checks
/// settings).
#[tokio::test]
async fn stop_connection_clears_permission_cache() {
    let (d, _rx) = dispatcher_with_setting(Some(true));
    d.start_connection(&start_payload("c1")).await;
    let payload = FileTransferPayload {
        connection_id: "c1".into(),
        data: br#"{"type":"transfer_complete","transfer_id":"t"}"#.to_vec(),
        is_text: true,
        transfer_id: None,
    };
    d.handle_command(payload).await;
    d.stop_connection(&StopMediaPayload {
        connection_id: "c1".into(),
    })
    .await;
    let g = d.inner.lock().await;
    assert!(g.permission_cache.get("c1").is_none());
}

/// Download path: serve_download reads file from disk, emits a
/// DownloadResponse (text), per-chunk binary frames, then
/// TransferComplete (text).
#[tokio::test]
async fn download_emits_response_chunks_and_complete() {
    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("hello.txt");
    // Build a file > 1 chunk so we get at least two chunks.
    let payload_size = FILE_TRANSFER_CHUNK_SIZE_TX + 100;
    let body = vec![b'x'; payload_size];
    tokio::fs::write(&file_path, &body).await.unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let req = DownloadRequest {
        transfer_id: "00000000-0000-0000-0000-000000000001".into(),
        file_path: file_path.to_string_lossy().to_string(),
    };
    d.serve_download("c1".into(), req).await.expect("serve ok");
    // First message: DownloadResponse (text)
    let p = rx.recv().await.expect("download response");
    assert!(p.is_text);
    let s = String::from_utf8(p.data).unwrap();
    let msg: FileTransferMessage = serde_json::from_str(&s).unwrap();
    match msg {
        FileTransferMessage::DownloadResponse(r) => {
            assert_eq!(r.file_size as usize, payload_size);
            assert!(r.total_chunks >= 2);
        }
        other => panic!("expected DownloadResponse, got {other:?}"),
    }
    // Followed by ≥2 binary chunks then a TransferComplete text.
    let mut binary_count = 0;
    let mut saw_complete = false;
    while let Some(p) = rx.recv().await {
        if !p.is_text {
            binary_count += 1;
            let (tid, _idx, body) = parse_binary_chunk(&p.data).expect("chunk parse");
            assert_eq!(tid, "00000000-0000-0000-0000-000000000001");
            assert!(!body.is_empty());
        } else {
            let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
            if matches!(m, FileTransferMessage::TransferComplete(_)) {
                saw_complete = true;
                break;
            }
        }
    }
    assert!(binary_count >= 2, "expected ≥2 chunks, got {binary_count}");
    assert!(saw_complete, "expected TransferComplete");
}

/// Regression: pin the on-the-wire chunk size at 240 KiB AND
/// guarantee `chunk_size + BINARY_HEADER_SIZE ≤ 262144` (Chrome's
/// typical `a=max-message-size:262144` SDP advertise). This is the
/// invariant that the first 256 KiB attempt violated — a 256 KiB
/// payload + 40-byte header = 262184-byte SCTP message just barely
/// exceeded the limit, and every binary chunk was silently rejected
/// at the daemon with `ErrOutboundPacketTooLarge` while the
/// TransferComplete control frame still got through, producing
/// false-positive "download complete, 0 bytes" in the browser.
///
/// Three failure modes this guards against:
///
/// 1. Someone silently shrinks `FILE_TRANSFER_CHUNK_SIZE_TX` back
///    toward 60 KB after seeing the windowed metrics improve — the
///    whole point of the 2026-05-11 bump was to amortize the
///    per-`dc.send` SCTP overhead, so a regression here re-tanks
///    LAN throughput.
/// 2. Someone raises the chunk size back toward 256 KiB without
///    accounting for the 40-byte header — the SCTP-limit assertion
///    will fail at test time instead of silently in production.
/// 3. The browser-side `FILE_TRANSFER_CHUNK_SIZE` in
///    `use-file-transfer.ts` drifts out of sync with the server-side
///    constant. The browser uses its own constant to chunk uploads,
///    but it reads `chunk_size` from the server's
///    `DownloadResponse` for download reassembly metadata, so the
///    value travelling on the wire IS the contract.
#[tokio::test]
async fn download_response_advertises_240kib_chunk_size() {
    const EXPECTED_CHUNK_SIZE: usize = 240 * 1024;
    /// Chrome's typical SDP-advertised `a=max-message-size`. Lower
    /// in some older Chromium forks and not formally guaranteed by
    /// any spec — RFC 8841 only says "default 65536 when absent".
    /// We use Chrome's value as the practical ceiling because
    /// it's the most common deployment target and any browser
    /// advertising higher (e.g. Firefox at ~1 GB) is by definition
    /// more permissive.
    const CHROME_MAX_MESSAGE_SIZE: usize = 262144;
    assert_eq!(
        FILE_TRANSFER_CHUNK_SIZE_TX, EXPECTED_CHUNK_SIZE,
        "chunk size constant regressed: see 2026-05-11 ft-metrics archive"
    );
    assert!(
        FILE_TRANSFER_CHUNK_SIZE_TX + BINARY_HEADER_SIZE <= CHROME_MAX_MESSAGE_SIZE,
        "wire-level SCTP message ({} payload + {} header = {} bytes) \
             must not exceed Chrome's typical max-message-size advertise \
             ({} bytes) — exceeding it silently drops every binary chunk \
             at the daemon (regression fixed 2026-05-11)",
        FILE_TRANSFER_CHUNK_SIZE_TX,
        BINARY_HEADER_SIZE,
        FILE_TRANSFER_CHUNK_SIZE_TX + BINARY_HEADER_SIZE,
        CHROME_MAX_MESSAGE_SIZE,
    );

    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("big.bin");
    // 1 byte past one chunk so total_chunks = 2 exactly. This pins
    // the `div_ceil(file_size, chunk_size)` math at the boundary
    // where an off-by-one would surface (e.g. someone switching to
    // `file_size / chunk_size` would compute 1 here, drop the
    // tail byte, and only the regression test would catch it).
    let payload_size = EXPECTED_CHUNK_SIZE + 1;
    let body = vec![b'a'; payload_size];
    tokio::fs::write(&file_path, &body).await.unwrap();

    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let req = DownloadRequest {
        transfer_id: "00000000-0000-0000-0000-000000000020".into(),
        file_path: file_path.to_string_lossy().to_string(),
    };
    d.serve_download("c1".into(), req).await.expect("serve ok");

    let p = rx.recv().await.expect("download response");
    assert!(p.is_text);
    let msg: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
    match msg {
        FileTransferMessage::DownloadResponse(r) => {
            assert_eq!(
                r.chunk_size, EXPECTED_CHUNK_SIZE,
                "DownloadResponse.chunk_size must match server constant"
            );
            assert_eq!(
                r.file_size as usize, payload_size,
                "DownloadResponse.file_size must equal source file size"
            );
            assert_eq!(
                r.total_chunks, 2,
                "boundary math: {} bytes / {} chunk = 2 chunks via div_ceil",
                payload_size, EXPECTED_CHUNK_SIZE
            );
        }
        other => panic!("expected DownloadResponse, got {other:?}"),
    }

    // Drain the rest so the spawned download task doesn't leave
    // dangling state when the test exits.
    let mut total_body = 0usize;
    let mut saw_complete = false;
    while let Some(p) = rx.recv().await {
        if !p.is_text {
            let (_tid, _idx, body) = parse_binary_chunk(&p.data).expect("chunk parse");
            total_body += body.len();
            // Each emitted chunk must respect the advertised chunk_size cap.
            assert!(
                body.len() <= EXPECTED_CHUNK_SIZE,
                "chunk body {} > advertised chunk_size {}",
                body.len(),
                EXPECTED_CHUNK_SIZE
            );
        } else {
            let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
            if matches!(m, FileTransferMessage::TransferComplete(_)) {
                saw_complete = true;
                break;
            }
        }
    }
    assert!(saw_complete, "expected TransferComplete");
    assert_eq!(
        total_body, payload_size,
        "concatenated chunk bodies must equal source file size"
    );
}

/// Upload happy path: UploadRequest creates the file and emits
/// UploadResponse{accepted:true}; subsequent chunks write to disk;
/// final chunk yields TransferComplete on its own.
#[tokio::test]
async fn upload_creates_file_and_completes_on_last_chunk() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-000000000002".to_string();
    let total_chunks = 2u64;
    let chunk_size = 8usize;
    let req = UploadRequest {
        transfer_id: transfer_id.clone(),
        target_dir: tmp.path().to_string_lossy().to_string(),
        file_name: "uploaded.bin".to_string(),
        file_size: (chunk_size as u64) * total_chunks,
        chunk_size,
        total_chunks,
    };
    let req_msg = FileTransferMessage::UploadRequest(req);
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&req_msg).unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;
    // Expect UploadResponse text first
    let p = rx.recv().await.unwrap();
    assert!(p.is_text);
    let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
    assert!(matches!(m, FileTransferMessage::UploadResponse(_)));
    // Send 2 chunks
    for i in 0..total_chunks as u32 {
        let chunk_bytes = build_binary_chunk(&transfer_id, i, &vec![b'A' + i as u8; chunk_size]);
        d.handle_command(FileTransferPayload {
            connection_id: "c1".into(),
            data: chunk_bytes,
            is_text: false,
            transfer_id: None,
        })
        .await;
    }
    // Expect TransferComplete text
    let p = rx.recv().await.unwrap();
    assert!(p.is_text);
    let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
    assert!(
        matches!(m, FileTransferMessage::TransferComplete(_)),
        "expected TransferComplete, got {m:?}"
    );
    // File on disk has the merged contents
    let written = tokio::fs::read(tmp.path().join("uploaded.bin"))
        .await
        .unwrap();
    assert_eq!(written.len(), chunk_size * total_chunks as usize);
    assert_eq!(written[0], b'A');
    assert_eq!(written[chunk_size], b'B');
}

/// An empty file is a normal file: it declares zero chunks, sends none, and
/// must land on disk as an empty file rather than being rejected as an upload
/// that never delivered its bytes.
#[tokio::test]
async fn upload_of_an_empty_file_completes_with_no_chunks() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-000000000009".to_string();
    let req_msg = FileTransferMessage::UploadRequest(UploadRequest {
        transfer_id: transfer_id.clone(),
        target_dir: tmp.path().to_string_lossy().to_string(),
        file_name: "empty.txt".to_string(),
        file_size: 0,
        chunk_size: 8,
        total_chunks: 0,
    });
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&req_msg).unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;
    match expect_message(&mut rx).await {
        FileTransferMessage::UploadResponse(response) => assert!(response.accepted),
        other => panic!("expected UploadResponse, got {other:?}"),
    }

    // The controller sends no chunks at all, then says it is done.
    let complete = FileTransferMessage::TransferComplete(TransferComplete {
        transfer_id: transfer_id.clone(),
    });
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&complete).unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;

    // No failure is reported, and the empty file survives.
    assert_no_message(&mut rx).await;
    let written = tokio::fs::read(tmp.path().join("empty.txt")).await.unwrap();
    assert!(written.is_empty());
}

#[tokio::test]
async fn upload_rejects_controller_declared_size_mismatch() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000f1".to_string();
    let req = UploadRequest {
        transfer_id: transfer_id.clone(),
        target_dir: tmp.path().to_string_lossy().to_string(),
        file_name: "mismatch.bin".to_string(),
        file_size: 9,
        chunk_size: 8,
        total_chunks: 1,
    };
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::UploadRequest(req)).unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;
    let response = rx.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<FileTransferMessage>(&response.data).unwrap(),
        FileTransferMessage::UploadResponse(_)
    ));

    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: build_binary_chunk(&transfer_id, 0, b"12345678"),
        is_text: false,
        transfer_id: None,
    })
    .await;

    let error = rx.recv().await.unwrap();
    match serde_json::from_slice::<FileTransferMessage>(&error.data).unwrap() {
        FileTransferMessage::TransferError(error) => {
            assert_eq!(error.transfer_id, transfer_id);
            assert!(error.message.contains("expected 9 bytes"));
            assert!(error.message.contains("received 8 bytes"));
        }
        other => panic!("expected TransferError, got {other:?}"),
    }
    assert!(!tmp.path().join("mismatch.bin").exists());
}

#[tokio::test]
async fn upload_states_with_identical_ids_are_isolated_by_connection() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    let transfer_id = "00000000-0000-0000-0000-0000000000f2";
    for (connection_id, file_name) in [("conn-a", "a.bin"), ("conn-b", "b.bin")] {
        d.start_connection(&start_payload(connection_id)).await;
        let req = UploadRequest {
            transfer_id: transfer_id.to_string(),
            target_dir: tmp.path().to_string_lossy().to_string(),
            file_name: file_name.to_string(),
            file_size: 4,
            chunk_size: 4,
            total_chunks: 1,
        };
        d.handle_command(FileTransferPayload {
            connection_id: connection_id.to_string(),
            data: serde_json::to_vec(&FileTransferMessage::UploadRequest(req)).unwrap(),
            is_text: true,
            transfer_id: None,
        })
        .await;
        let _ = rx.recv().await.expect("UploadResponse");
    }
    {
        let inner = d.inner.lock().await;
        assert!(
            inner
                .upload_states
                .contains_key(&TransferKey::new("conn-a", transfer_id))
        );
        assert!(
            inner
                .upload_states
                .contains_key(&TransferKey::new("conn-b", transfer_id))
        );
    }

    d.stop_connection(&StopMediaPayload {
        connection_id: "conn-a".to_string(),
    })
    .await;
    let inner = d.inner.lock().await;
    assert!(
        !inner
            .upload_states
            .contains_key(&TransferKey::new("conn-a", transfer_id))
    );
    assert!(
        inner
            .upload_states
            .contains_key(&TransferKey::new("conn-b", transfer_id))
    );
}

#[tokio::test]
async fn upload_rejects_duplicate_id_within_one_connection() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000f3";
    for file_name in ["first.bin", "second.bin"] {
        let req = UploadRequest {
            transfer_id: transfer_id.to_string(),
            target_dir: tmp.path().to_string_lossy().to_string(),
            file_name: file_name.to_string(),
            file_size: 4,
            chunk_size: 4,
            total_chunks: 1,
        };
        d.handle_command(FileTransferPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&FileTransferMessage::UploadRequest(req)).unwrap(),
            is_text: true,
            transfer_id: None,
        })
        .await;
    }

    let accepted = rx.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<FileTransferMessage>(&accepted.data).unwrap(),
        FileTransferMessage::UploadResponse(_)
    ));
    let rejected = rx.recv().await.unwrap();
    match serde_json::from_slice::<FileTransferMessage>(&rejected.data).unwrap() {
        FileTransferMessage::TransferError(error) => {
            assert_eq!(error.transfer_id, transfer_id);
            assert!(error.message.contains("already active"));
        }
        other => panic!("expected TransferError, got {other:?}"),
    }
    let inner = d.inner.lock().await;
    assert_eq!(
        inner.upload_states.len(),
        1,
        "the first transfer is the only one registered",
    );
    assert!(
        inner
            .upload_destinations
            .contains(&tmp.path().canonicalize().unwrap().join("first.bin")),
        "the accepted upload holds its destination",
    );
    assert!(
        !tmp.path().join("first.bin").exists(),
        "an accepted upload has not created its destination yet — the bytes are staged",
    );
    assert!(!tmp.path().join("second.bin").exists());
}

/// Cancelling a download mid-flight stops the loop and returns
/// without emitting TransferComplete. We trigger by spawning the
/// download then immediately marking the transfer cancelled.
#[tokio::test]
async fn cancel_download_stops_emitting_chunks() {
    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("big.bin");
    // Multi-MB so the loop iterates a while.
    let body = vec![b'x'; FILE_TRANSFER_CHUNK_SIZE_TX * 50];
    tokio::fs::write(&file_path, &body).await.unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-000000000003".to_string();
    // Pre-mark as cancelled before download starts: the loop's
    // cancel-check will fire on first iteration and return early.
    {
        let mut inner = d.inner.lock().await;
        inner
            .cancelled_transfers
            .insert(TransferKey::new("c1", &transfer_id));
    }
    let req = DownloadRequest {
        transfer_id: transfer_id.clone(),
        file_path: file_path.to_string_lossy().to_string(),
    };
    d.serve_download("c1".into(), req).await.unwrap();
    // Should have emitted only the DownloadResponse, no chunks,
    // no TransferComplete.
    let p = rx.recv().await.expect("DownloadResponse");
    assert!(p.is_text);
    let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
    assert!(matches!(m, FileTransferMessage::DownloadResponse(_)));
    // Nothing else should follow on the lane: cancel-check fires
    // on the first loop iteration and returns before any chunk
    // or TransferComplete is emitted.
    assert_no_message(&mut rx).await;
}

/// A cancel can be the very next frame after the request that started the
/// download, and the download runs on a task of its own that has not
/// necessarily begun.
///
/// Both frames go through the real command path here. A download that is only
/// registered once its task gets as far as opening the file would leave this
/// cancel with nothing to mark, and the peer that asked for the transfer to stop
/// would receive the whole file and be told it completed.
#[tokio::test]
async fn a_cancel_arriving_before_the_stream_starts_is_still_honoured() {
    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("big.bin");
    tokio::fs::write(&file_path, vec![b'x'; FILE_TRANSFER_CHUNK_SIZE_TX * 20])
        .await
        .unwrap();
    // A short lane so the download parks on backpressure instead of running to
    // completion while the test is still sending the cancel.
    let (d, mut rx) = dispatcher_with_file_cap(Some(true), 4);
    d.start_connection(&start_payload("c1")).await;

    let transfer_id = "00000000-0000-0000-0000-0000000000c1".to_string();
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::DownloadRequest(DownloadRequest {
            transfer_id: transfer_id.clone(),
            file_path: file_path.to_string_lossy().to_string(),
        }))
        .unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::TransferCancel(TransferCancel {
            transfer_id: transfer_id.clone(),
        }))
        .unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;

    // Whatever chunks escaped before the cancel took effect are fine; being told
    // the transfer finished is not.
    while let Ok(Some(payload)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await
    {
        if !payload.is_text {
            continue;
        }
        let message: FileTransferMessage = serde_json::from_slice(&payload.data).unwrap();
        assert!(
            !matches!(message, FileTransferMessage::TransferComplete(_)),
            "a cancelled download must not report completion"
        );
    }
}

/// A source that yields `ok_reads` chunks and then fails, standing in for a file
/// that becomes unreadable partway — a disconnected mount, a failing disk.
struct FailingReader {
    ok_reads: usize,
}

impl tokio::io::AsyncRead for FailingReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.ok_reads == 0 {
            return std::task::Poll::Ready(Err(std::io::Error::other("injected read failure")));
        }
        self.ok_reads -= 1;
        let filled = buf.remaining().min(64);
        buf.put_slice(&vec![b'x'; filled]);
        std::task::Poll::Ready(Ok(()))
    }
}

/// A sink that accepts `ok_writes` writes and then fails, and whose flush fails
/// once `flush_fails` is set. Stands in for a target that runs out of room or
/// goes away mid-upload.
struct FailingWriter {
    ok_writes: usize,
    flush_fails: bool,
}

impl tokio::io::AsyncWrite for FailingWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.ok_writes == 0 {
            return std::task::Poll::Ready(Err(std::io::Error::other("injected write failure")));
        }
        self.ok_writes -= 1;
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.flush_fails {
            return std::task::Poll::Ready(Err(std::io::Error::other("injected flush failure")));
        }
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[async_trait::async_trait]
impl UploadSink for FailingWriter {
    async fn sync(&mut self) -> std::io::Result<()> {
        if self.flush_fails {
            return Err(std::io::Error::other("injected sync failure"));
        }
        Ok(())
    }
}

/// Register an upload the dispatcher will write into, with a writer of the
/// test's choosing.
#[allow(clippy::too_many_arguments)]
async fn arm_upload(
    d: &FileTransferDispatcher,
    connection_id: &str,
    transfer_id: &str,
    destination: PathBuf,
    staging: PathBuf,
    file: Box<dyn UploadSink>,
    total_chunks: u64,
    expected_bytes: u64,
) {
    d.start_activity(
        connection_id,
        transfer_id,
        FileTransferDirection::Upload,
        "payload.bin",
        expected_bytes,
    )
    .await;
    let mut inner = d.inner.lock().await;
    inner.upload_destinations.insert(destination.clone());
    inner.upload_states.insert(
        TransferKey::new(connection_id, transfer_id),
        Arc::new(Upload {
            cancelled: AtomicBool::new(false),
            destination,
            staging,
            state: TokioMutex::new(UploadState {
                file,
                total_chunks,
                received_chunks: 0,
                expected_bytes,
                received_bytes: 0,
                metrics: Default::default(),
            }),
        }),
    );
}

/// A read that fails after the transfer has already been announced. The browser
/// is holding a half-written file and still waiting for the rest, so the failure
/// has to reach it rather than end the loop quietly.
#[tokio::test]
async fn a_download_whose_read_fails_mid_stream_reports_why() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000e1";
    d.start_activity(
        "c1",
        transfer_id,
        FileTransferDirection::Download,
        "payload.bin",
        4096,
    )
    .await;

    let result = d
        .stream_download(
            "c1",
            transfer_id,
            FailingReader { ok_reads: 1 },
            DownloadPlan {
                file_name: "payload.bin".into(),
                file_size: 4096,
                chunk_size: FILE_TRANSFER_CHUNK_SIZE_TX,
                total_chunks: 2,
            },
        )
        .await;

    assert!(
        result.is_err(),
        "the read failure is surfaced to the caller"
    );
    let reported = drain_transfer_errors(&mut rx).await;
    assert_eq!(
        reported,
        vec![DeskErrorCode::SYSTEM_ERROR],
        "the browser is told the read failed, exactly once"
    );
    assert!(
        d.inner.lock().await.activities.is_empty(),
        "the transfer is over"
    );
}

/// A write that fails partway through an upload. Without an answer the browser
/// keeps streaming chunks at a host that has already given up on the file.
#[tokio::test]
async fn an_upload_whose_write_fails_mid_stream_reports_why() {
    let tmp = TempDir::new().unwrap();
    let destination = tmp.path().join("report.pdf");
    tokio::fs::write(&destination, b"the file the user already had")
        .await
        .unwrap();
    let staging = tmp.path().join(".report.pdf.part");
    tokio::fs::write(&staging, b"partial").await.unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000e2";
    arm_upload(
        &d,
        "c1",
        transfer_id,
        destination.clone(),
        staging.clone(),
        Box::new(FailingWriter {
            ok_writes: 0,
            flush_fails: false,
        }),
        2,
        8,
    )
    .await;

    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: build_binary_chunk(transfer_id, 0, b"abcd"),
        is_text: false,
        transfer_id: None,
    })
    .await;

    let reported = drain_transfer_errors(&mut rx).await;
    assert_eq!(reported, vec![DeskErrorCode::SYSTEM_ERROR]);
    assert!(
        !staging.exists(),
        "the staging file is removed with the transfer"
    );
    assert_eq!(
        tokio::fs::read(&destination).await.unwrap(),
        b"the file the user already had",
        "an upload that failed must not have touched the file it was replacing",
    );
    assert!(
        d.inner.lock().await.activities.is_empty(),
        "the transfer is over"
    );
}

/// The last chunk lands but the flush fails, so nothing is durable. Reporting
/// success here would tell the user a file arrived that is not on disk.
#[tokio::test]
async fn an_upload_whose_final_flush_fails_reports_why() {
    let tmp = TempDir::new().unwrap();
    let destination = tmp.path().join("report.pdf");
    tokio::fs::write(&destination, b"the file the user already had")
        .await
        .unwrap();
    let staging = tmp.path().join(".report.pdf.part");
    tokio::fs::write(&staging, b"unflushed").await.unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000e3";
    arm_upload(
        &d,
        "c1",
        transfer_id,
        destination.clone(),
        staging.clone(),
        Box::new(FailingWriter {
            ok_writes: 1,
            flush_fails: true,
        }),
        1,
        4,
    )
    .await;

    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: build_binary_chunk(transfer_id, 0, b"abcd"),
        is_text: false,
        transfer_id: None,
    })
    .await;

    let reported = drain_transfer_errors(&mut rx).await;
    assert_eq!(reported, vec![DeskErrorCode::SYSTEM_ERROR]);
    assert!(
        !staging.exists(),
        "a file that was never flushed is not left behind"
    );
    assert_eq!(
        tokio::fs::read(&destination).await.unwrap(),
        b"the file the user already had",
        "the last step failing must not cost the user the file being replaced",
    );
}

/// A sink whose writes park until the test lets them through, counting what it
/// accepted. Stands in for a mount that has stopped answering — the case the
/// dispatcher has no control over and cannot be allowed to wait on.
struct StalledWriter {
    /// Fires the first time a write is attempted, so a test can tell "the write
    /// is in flight" from "the task has not got there yet".
    started: Option<tokio::sync::oneshot::Sender<()>>,
    /// Resolves when the write is allowed to complete. `None` once it has.
    release: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
    writes: Arc<std::sync::atomic::AtomicUsize>,
}

impl StalledWriter {
    /// A writer that never completes its first write.
    fn stuck(started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            started: Some(started),
            release: Some(Box::pin(std::future::pending())),
            writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// A writer whose first write completes once `gate` is signalled.
    fn gated(
        started: tokio::sync::oneshot::Sender<()>,
        gate: tokio::sync::oneshot::Receiver<()>,
        writes: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            started: Some(started),
            release: Some(Box::pin(async move {
                let _ = gate.await;
            })),
            writes,
        }
    }
}

#[async_trait::async_trait]
impl UploadSink for StalledWriter {
    async fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tokio::io::AsyncWrite for StalledWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if let Some(started) = this.started.take() {
            let _ = started.send(());
        }
        if let Some(release) = this.release.as_mut() {
            match std::future::Future::poll(release.as_mut(), cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(()) => this.release = None,
            }
        }
        this.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Drive one whole upload through the real command path and return its
/// destination.
async fn upload_one_chunk(
    d: &FileTransferDispatcher,
    rx: &mut Box<dyn EventReceiver<FileTransferPayload>>,
    target_dir: &std::path::Path,
    transfer_id: &str,
    file_name: &str,
    body: &[u8],
) {
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::UploadRequest(UploadRequest {
            transfer_id: transfer_id.to_string(),
            target_dir: target_dir.to_string_lossy().to_string(),
            file_name: file_name.to_string(),
            file_size: body.len() as u64,
            chunk_size: body.len(),
            total_chunks: 1,
        }))
        .unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;
    let _ = rx.recv().await.expect("UploadResponse");
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: build_binary_chunk(transfer_id, 0, body),
        is_text: false,
        transfer_id: None,
    })
    .await;
}

/// Uploading over a file the user already has is what the browser's file
/// manager offers, so it has to work — and it has to be the whole new file or
/// the whole old one, never a half-written mixture of the two.
#[tokio::test]
async fn an_upload_replaces_an_existing_file_in_one_step() {
    let tmp = TempDir::new().unwrap();
    let destination = tmp.path().join("report.pdf");
    tokio::fs::write(&destination, b"old contents")
        .await
        .unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;

    upload_one_chunk(
        &d,
        &mut rx,
        tmp.path(),
        "00000000-0000-0000-0000-0000000000c1",
        "report.pdf",
        b"new contents",
    )
    .await;

    assert_eq!(
        tokio::fs::read(&destination).await.unwrap(),
        b"new contents"
    );
    let mut left_behind = tokio::fs::read_dir(tmp.path()).await.unwrap();
    let mut names = Vec::new();
    while let Some(entry) = left_behind.next_entry().await.unwrap() {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    assert_eq!(
        names,
        vec!["report.pdf".to_string()],
        "the staging file is gone once it has taken the destination's name",
    );
    assert!(d.inner.lock().await.upload_destinations.is_empty());
}

/// Two transfers writing one file would each stage a copy and then rename over
/// each other, leaving whichever finished last while both peers are told they
/// succeeded. The second one to ask is refused instead.
#[tokio::test]
async fn a_second_upload_to_the_same_file_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let request = |transfer_id: &str| FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::UploadRequest(UploadRequest {
            transfer_id: transfer_id.to_string(),
            target_dir: tmp.path().to_string_lossy().to_string(),
            file_name: "contested.bin".to_string(),
            file_size: 4,
            chunk_size: 4,
            total_chunks: 1,
        }))
        .unwrap(),
        is_text: true,
        transfer_id: None,
    };

    d.handle_command(request("00000000-0000-0000-0000-0000000000c2"))
        .await;
    let accepted = rx.recv().await.expect("UploadResponse");
    assert!(matches!(
        serde_json::from_slice::<FileTransferMessage>(&accepted.data).unwrap(),
        FileTransferMessage::UploadResponse(_)
    ));

    d.handle_command(request("00000000-0000-0000-0000-0000000000c3"))
        .await;
    match serde_json::from_slice::<FileTransferMessage>(&rx.recv().await.expect("an answer").data)
        .unwrap()
    {
        FileTransferMessage::TransferError(e) => {
            assert_eq!(e.error_code, DeskErrorCode::INVALID_STATE);
            assert!(
                e.message.contains("Another upload"),
                "the refusal says why, got {:?}",
                e.message
            );
        }
        other => panic!("expected the second to be refused, got {other:?}"),
    }
    assert_eq!(
        d.inner.lock().await.upload_states.len(),
        1,
        "only the first transfer is registered",
    );
}

/// A destination that cannot be replaced is worth saying up front. The bytes go
/// to a staging file, so nothing would fail until the rename at the very end,
/// and by then the browser has streamed the whole file at a host that was never
/// going to accept it.
#[tokio::test]
async fn an_upload_onto_a_directory_is_refused_before_any_bytes_arrive() {
    let tmp = TempDir::new().unwrap();
    tokio::fs::create_dir(tmp.path().join("occupied"))
        .await
        .unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000c4";

    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::UploadRequest(UploadRequest {
            transfer_id: transfer_id.to_string(),
            target_dir: tmp.path().to_string_lossy().to_string(),
            file_name: "occupied".to_string(),
            file_size: 4,
            chunk_size: 4,
            total_chunks: 1,
        }))
        .unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;

    match serde_json::from_slice::<FileTransferMessage>(&rx.recv().await.expect("an answer").data)
        .unwrap()
    {
        FileTransferMessage::TransferError(e) => {
            assert_eq!(e.transfer_id, transfer_id);
            assert_eq!(e.error_code, DeskErrorCode::SYSTEM_ERROR);
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(
        d.inner.lock().await.activities.is_empty(),
        "a refused request opens no transfer",
    );
}

/// Arm an upload whose first chunk write never returns, and hand back the
/// dispatcher once that write is actually in flight.
async fn upload_stuck_mid_write(
    d: &FileTransferDispatcher,
    connection_id: &str,
    transfer_id: &str,
    destination: PathBuf,
    staging: PathBuf,
) {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    arm_upload(
        d,
        connection_id,
        transfer_id,
        destination,
        staging,
        Box::new(StalledWriter::stuck(started_tx)),
        2,
        8,
    )
    .await;
    let writer = d.clone();
    let connection_id = connection_id.to_string();
    let chunk = build_binary_chunk(transfer_id, 0, b"abcd");
    tokio::spawn(async move {
        writer
            .handle_binary(FileTransferPayload {
                connection_id,
                data: chunk,
                is_text: false,
                transfer_id: None,
            })
            .await;
    });
    started_rx.await.expect("the write must reach the writer");
}

/// A peer's access is withdrawn by the paths that end a connection, and none of
/// them may be made to wait on a disk. A write can park for as long as the
/// device feels like — if it were holding the lock those paths need, a stalled
/// mount would be enough to keep a session that is supposed to be over alive.
#[tokio::test]
async fn a_connection_can_end_while_an_upload_write_is_stuck() {
    let tmp = TempDir::new().unwrap();
    let staging = tmp.path().join(".stuck.bin.part");
    tokio::fs::write(&staging, b"partial").await.unwrap();
    let (d, _rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000e4";
    upload_stuck_mid_write(
        &d,
        "c1",
        transfer_id,
        tmp.path().join("stuck.bin"),
        staging.clone(),
    )
    .await;

    tokio::time::timeout(
        Duration::from_secs(2),
        d.stop_connection(&StopMediaPayload {
            connection_id: "c1".to_string(),
        }),
    )
    .await
    .expect("ending a connection must not wait for a write that is stuck");

    assert!(
        d.inner.lock().await.upload_states.is_empty(),
        "the transfer is over even though its write never returned",
    );
}

/// Same for the shutdown path, which runs when the worker is going away and has
/// even less business waiting on a device.
#[tokio::test]
async fn a_shutdown_does_not_wait_for_a_stuck_upload_write() {
    let tmp = TempDir::new().unwrap();
    let staging = tmp.path().join(".stuck.bin.part");
    tokio::fs::write(&staging, b"partial").await.unwrap();
    let (d, _rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000e5";
    upload_stuck_mid_write(
        &d,
        "c1",
        transfer_id,
        tmp.path().join("stuck.bin"),
        staging.clone(),
    )
    .await;

    tokio::time::timeout(Duration::from_secs(2), d.shutdown())
        .await
        .expect("shutdown must not wait for a write that is stuck");

    assert!(d.inner.lock().await.upload_states.is_empty());
}

/// A cancel no longer waits for a write, so a chunk that queued behind one can
/// still be holding a live handle on the transfer when the cancel lands — the
/// handle it took before the transfer went out of reach. Once the transfer is
/// called off nothing more may reach the file: it has already been reported
/// gone, and a peer that asked to stop is entitled to have stopped.
///
/// Two chunks are driven concurrently here on purpose. The worker's file lane
/// awaits one command before reading the next, so today they cannot overlap in
/// production — but that is the drain loop's property, not the dispatcher's, and
/// the guarantee under test belongs to the dispatcher.
#[tokio::test]
async fn a_chunk_queued_behind_a_cancelled_write_is_not_written() {
    let tmp = TempDir::new().unwrap();
    let destination = tmp.path().join("queued.bin");
    let staging = tmp.path().join(".queued.bin.part");
    tokio::fs::write(&staging, b"").await.unwrap();
    let (d, _rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000e6";
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    arm_upload(
        &d,
        "c1",
        transfer_id,
        destination.clone(),
        staging.clone(),
        Box::new(StalledWriter::gated(started_tx, release_rx, writes.clone())),
        3,
        12,
    )
    .await;

    let first = tokio::spawn({
        let d = d.clone();
        let chunk = build_binary_chunk(transfer_id, 0, b"abcd");
        async move {
            d.handle_binary(FileTransferPayload {
                connection_id: "c1".into(),
                data: chunk,
                is_text: false,
                transfer_id: None,
            })
            .await;
        }
    });
    started_rx.await.expect("the first write is in flight");

    // The second chunk takes a handle on the transfer and then waits its turn
    // behind the first — which is the state it has to be in when the cancel
    // arrives for this to test anything.
    let second = tokio::spawn({
        let d = d.clone();
        let chunk = build_binary_chunk(transfer_id, 1, b"efgh");
        async move {
            d.handle_binary(FileTransferPayload {
                connection_id: "c1".into(),
                data: chunk,
                is_text: false,
                transfer_id: None,
            })
            .await;
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    d.handle_text(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::TransferCancel(TransferCancel {
            transfer_id: transfer_id.to_string(),
        }))
        .unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;

    let _ = release_tx.send(());
    first.await.unwrap();
    second.await.unwrap();

    assert_eq!(
        writes.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "only the write that was already under way may reach the file",
    );
    assert!(
        !staging.exists(),
        "a cancelled upload leaves nothing behind, whichever side removes it",
    );
    assert!(
        !destination.exists(),
        "a cancelled upload never reaches the file it was going to become",
    );
}

/// Every `TransferError` the lane carries, in order. Chunks and progress
/// messages are ignored; what matters is what the browser is told went wrong.
async fn drain_transfer_errors(
    rx: &mut Box<dyn EventReceiver<FileTransferPayload>>,
) -> Vec<DeskErrorCode> {
    let mut codes = Vec::new();
    while let Ok(Some(payload)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await
    {
        if !payload.is_text {
            continue;
        }
        if let Ok(FileTransferMessage::TransferError(e)) =
            serde_json::from_slice::<FileTransferMessage>(&payload.data)
        {
            codes.push(e.error_code);
        }
    }
    codes
}

/// Download for a non-existent file emits TransferError, not panic.
#[tokio::test]
async fn download_missing_file_emits_transfer_error() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let req = DownloadRequest {
        transfer_id: "00000000-0000-0000-0000-000000000004".into(),
        file_path: "/definitely/not/here.txt".into(),
    };
    d.serve_download("c1".into(), req).await.unwrap();
    let p = rx.recv().await.unwrap();
    assert!(p.is_text);
    let parsed: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
    assert!(matches!(parsed, FileTransferMessage::TransferError(_)));
}

/// A binary chunk shorter than the 40-byte header carries no transfer id, so it
/// cannot be answered on its own. Every transfer the connection has open is
/// failed instead: the sender has already lost the guarantee that the rest of
/// its stream arrives intact, and leaving those transfers unanswered strands
/// them on the browser's progress bar until the watchdog fires.
#[tokio::test]
async fn a_truncated_binary_chunk_fails_the_connection_transfers() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    d.start_activity(
        "c1",
        "in-flight",
        FileTransferDirection::Download,
        "photo.png",
        10,
    )
    .await;

    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: vec![0u8; 10],
        is_text: false,
        transfer_id: None,
    })
    .await;

    tokio::task::yield_now().await;
    expect_only_transfer_error(&mut rx, "in-flight", DeskErrorCode::INVALID_PARAMS).await;
    assert!(
        d.inner.lock().await.activities.is_empty(),
        "the failed transfer is no longer active"
    );
}

/// Binary chunk for an unknown transfer_id is dropped without
/// touching disk or panicking.
#[tokio::test]
async fn binary_chunk_unknown_transfer_drops_silently() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let chunk = build_binary_chunk("00000000-0000-0000-0000-000000000099", 0, b"abc");
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: chunk,
        is_text: false,
        transfer_id: None,
    })
    .await;
    assert_no_message(&mut rx).await;
}

/// Backpressure regression: when the file lane is saturated, the
/// download loop must park on `emit_binary().await` instead of
/// reading the rest of the file into memory. Exercises the
/// end-to-end backpressure chain that fix #2026-05-10 was supposed
/// to restore — pre-fix the daemon's unbounded mpsc swallowed
/// every chunk and let the worker scan a 989 MB file straight
/// into the IPC queue.
#[tokio::test]
async fn download_blocks_when_file_lane_full() {
    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("blocked.bin");
    // Multi-chunk file so the loop has work to do beyond the
    // initial DownloadResponse.
    let payload_size = FILE_TRANSFER_CHUNK_SIZE_TX * 5;
    let body = vec![b'x'; payload_size];
    tokio::fs::write(&file_path, &body).await.unwrap();
    // cap = 2: DownloadResponse + the very first chunk fill the
    // lane. The second chunk's `emit_binary` must block.
    let (d, mut rx) = dispatcher_with_file_cap(Some(true), 2);
    d.start_connection(&start_payload("c1")).await;
    let req = DownloadRequest {
        transfer_id: "00000000-0000-0000-0000-000000000010".into(),
        file_path: file_path.to_string_lossy().to_string(),
    };
    let dispatcher_clone = d.clone();
    let download_handle =
        tokio::spawn(async move { dispatcher_clone.serve_download("c1".into(), req).await });
    // Drain the first two emits so the spawn has time to push
    // them. Each must arrive promptly.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("first emit (DownloadResponse) timed out — dispatcher stuck before lane fill")
        .expect("file lane closed unexpectedly");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("second emit (chunk 0) timed out")
        .expect("file lane closed unexpectedly");
    // Stop draining. The download should still be running —
    // parked on `emit_binary().await` for chunk 1. Awaiting the
    // join handle must time out: a completed serve_download here
    // would prove the backpressure chain is broken.
    let still_running =
        tokio::time::timeout(std::time::Duration::from_millis(300), download_handle).await;
    assert!(
        still_running.is_err(),
        "serve_download completed while file lane was saturated; \
             backpressure chain is broken: {still_running:?}"
    );
}

// ============== handle_send_failed ==============

/// Targeted abort: with `transfer_id = Some(...)`, only that
/// upload's state is removed; an unrelated upload on the same
/// connection survives. Mirrors the daemon's fine-grained
/// `dc.send` failure attribution. Regression guard against a
/// future refactor that accidentally widens the abort scope.
#[tokio::test]
async fn handle_send_failed_aborts_only_targeted_upload() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    // Stage two in-flight uploads so we can prove the failure
    // notification scoped on `transfer_id_a` doesn't take `_b`
    // with it.
    let id_a = "00000000-0000-0000-0000-0000000000aa".to_string();
    let id_b = "00000000-0000-0000-0000-0000000000bb".to_string();
    for tid in [&id_a, &id_b] {
        let req = UploadRequest {
            transfer_id: tid.clone(),
            target_dir: tmp.path().to_string_lossy().to_string(),
            file_name: format!("up-{tid}.bin"),
            file_size: 4,
            chunk_size: 4,
            total_chunks: 1,
        };
        d.handle_command(FileTransferPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&FileTransferMessage::UploadRequest(req)).unwrap(),
            is_text: true,
            transfer_id: None,
        })
        .await;
        // Drain the UploadResponse so the lane stays clean for the
        // TransferError we assert on below.
        let _ = rx.recv().await.expect("UploadResponse");
    }
    d.handle_send_failed(FileTransferSendFailedPayload {
        connection_id: "c1".into(),
        transfer_id: Some(id_a.clone()),
        chunk_index: Some(7),
        kind: FileTransferSendErrorKind::PacketTooLarge,
        error: "outbound packet too large".to_string(),
    })
    .await;
    // Targeted abort: id_a is gone, id_b survives.
    {
        let inner = d.inner.lock().await;
        assert!(
            !inner
                .upload_states
                .contains_key(&TransferKey::new("c1", &id_a))
        );
        assert!(
            inner
                .upload_states
                .contains_key(&TransferKey::new("c1", &id_b))
        );
        assert!(
            inner.cancelled_transfers.is_empty(),
            "an upload has no streaming loop to read a cancel, so recording one \
             would leave an entry nothing ever collects"
        );
    }
    // TransferError emitted for id_a only.
    let p = rx.recv().await.expect("TransferError emit");
    assert!(p.is_text);
    let parsed: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
    match parsed {
        FileTransferMessage::TransferError(e) => {
            assert_eq!(e.transfer_id, id_a);
            assert!(
                e.message.contains("PacketTooLarge"),
                "expected kind in message, got {:?}",
                e.message
            );
            assert!(
                e.message.contains("chunk 7"),
                "expected chunk index in message, got {:?}",
                e.message
            );
        }
        other => panic!("expected TransferError, got {other:?}"),
    }
    assert_no_message(&mut rx).await;
}

/// Coarse abort: with `transfer_id = None`, every in-flight upload
/// on the connection is dropped + a TransferError is emitted per
/// transfer. This is the fallback when the daemon could not
/// attribute the failure (legacy payload without `transfer_id`).
#[tokio::test]
async fn handle_send_failed_without_transfer_id_aborts_all_uploads() {
    let tmp = TempDir::new().unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let id_a = "00000000-0000-0000-0000-0000000000a1".to_string();
    let id_b = "00000000-0000-0000-0000-0000000000b2".to_string();
    for tid in [&id_a, &id_b] {
        let req = UploadRequest {
            transfer_id: tid.clone(),
            target_dir: tmp.path().to_string_lossy().to_string(),
            file_name: format!("up-{tid}.bin"),
            file_size: 4,
            chunk_size: 4,
            total_chunks: 1,
        };
        d.handle_command(FileTransferPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&FileTransferMessage::UploadRequest(req)).unwrap(),
            is_text: true,
            transfer_id: None,
        })
        .await;
        let _ = rx.recv().await.expect("UploadResponse");
    }
    d.handle_send_failed(FileTransferSendFailedPayload {
        connection_id: "c1".into(),
        transfer_id: None,
        chunk_index: None,
        kind: FileTransferSendErrorKind::TransportClosed,
        error: "channel closed".to_string(),
    })
    .await;
    {
        let inner = d.inner.lock().await;
        assert!(
            inner.upload_states.is_empty(),
            "all uploads must be cleared"
        );
        assert!(
            inner.cancelled_transfers.is_empty(),
            "uploads have no streaming loop to read a cancel"
        );
    }
    // Two TransferError messages, one per aborted transfer.
    // Order is HashMap-iteration-dependent so collect into a set.
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2 {
        let p = rx.recv().await.expect("TransferError emit");
        assert!(p.is_text);
        let parsed: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
        match parsed {
            FileTransferMessage::TransferError(e) => {
                seen.insert(e.transfer_id);
            }
            other => panic!("expected TransferError, got {other:?}"),
        }
    }
    assert!(seen.contains(&id_a));
    assert!(seen.contains(&id_b));
    assert_no_message(&mut rx).await;
}

/// Cancel flag is set when the targeted transfer is a download already in
/// flight. `serve_download` polls the flag on each loop iteration, so this is
/// how a daemon-side send failure aborts one.
#[tokio::test]
async fn handle_send_failed_for_download_sets_cancel_flag() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let tid = "00000000-0000-0000-0000-0000000000dd".to_string();
    // Stands in for a streaming loop: `serve_download` registers itself here
    // for exactly as long as it can still act on a cancel.
    {
        let mut inner = d.inner.lock().await;
        inner.live_downloads.insert(TransferKey::new("c1", &tid));
    }
    d.handle_send_failed(FileTransferSendFailedPayload {
        connection_id: "c1".into(),
        transfer_id: Some(tid.clone()),
        chunk_index: Some(0),
        kind: FileTransferSendErrorKind::Other,
        error: "boom".to_string(),
    })
    .await;
    {
        let inner = d.inner.lock().await;
        assert!(
            inner
                .cancelled_transfers
                .contains(&TransferKey::new("c1", &tid))
        );
    }
    let p = rx.recv().await.expect("TransferError emit");
    let parsed: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
    assert!(matches!(parsed, FileTransferMessage::TransferError(_)));
}

/// A host that cannot create the file has to say so. The browser is about to
/// stream a whole file at it, and without a reply it keeps that transfer on
/// screen until the watchdog turns a precise filesystem error into a timeout.
#[tokio::test]
async fn an_upload_that_cannot_open_its_target_reports_why() {
    let tmp = TempDir::new().unwrap();
    // A directory where the upload wants to put a file: creating over it fails
    // on every platform, without depending on who the test runs as.
    tokio::fs::create_dir(tmp.path().join("taken"))
        .await
        .unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000f1".to_string();

    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: serde_json::to_vec(&FileTransferMessage::UploadRequest(UploadRequest {
            transfer_id: transfer_id.clone(),
            target_dir: tmp.path().to_string_lossy().to_string(),
            file_name: "taken".to_string(),
            file_size: 4,
            chunk_size: 4,
            total_chunks: 1,
        }))
        .unwrap(),
        is_text: true,
        transfer_id: None,
    })
    .await;

    let payload = rx.recv().await.expect("an answer");
    match serde_json::from_slice::<FileTransferMessage>(&payload.data).unwrap() {
        FileTransferMessage::TransferError(e) => {
            assert_eq!(e.transfer_id, transfer_id);
            assert_eq!(e.error_code, DeskErrorCode::SYSTEM_ERROR);
        }
        other => panic!("expected the failure to be reported, got {other:?}"),
    }
    let inner = d.inner.lock().await;
    assert!(inner.activities.is_empty(), "the transfer is over");
}

/// The same for the other direction: a file that passes the existence check and
/// then refuses to open leaves the browser with nothing to show unless the host
/// says what happened.
#[cfg(unix)]
#[tokio::test]
async fn a_download_that_cannot_open_its_file_reports_why() {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("locked.bin");
    tokio::fs::write(&file_path, b"body").await.unwrap();
    tokio::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o000))
        .await
        .unwrap();
    if tokio::fs::File::open(&file_path).await.is_ok() {
        // Running as a user that ignores the mode (root in a container); the
        // path this test is about cannot be reached here.
        return;
    }
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000f2".to_string();

    d.serve_download(
        "c1".into(),
        DownloadRequest {
            transfer_id: transfer_id.clone(),
            file_path: file_path.to_string_lossy().to_string(),
        },
    )
    .await
    .unwrap();

    let payload = rx.recv().await.expect("an answer");
    match serde_json::from_slice::<FileTransferMessage>(&payload.data).unwrap() {
        FileTransferMessage::TransferError(e) => {
            assert_eq!(e.transfer_id, transfer_id);
            assert_eq!(e.error_code, DeskErrorCode::SYSTEM_ERROR);
        }
        other => panic!("expected the failure to be reported, got {other:?}"),
    }
}

/// A peer picks the ids it cancels, so a cancel that names nothing must cost
/// nothing. Recording one per frame would let a connection that is allowed to
/// transfer files grow the worker's memory for as long as it stays open.
#[tokio::test]
async fn cancels_for_transfers_that_are_not_streaming_leave_nothing_behind() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;

    for i in 0..500u32 {
        let cancel = FileTransferMessage::TransferCancel(TransferCancel {
            transfer_id: format!("00000000-0000-0000-0000-{i:012}"),
        });
        d.handle_command(FileTransferPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&cancel).unwrap(),
            is_text: true,
            transfer_id: None,
        })
        .await;
    }

    let inner = d.inner.lock().await;
    assert!(
        inner.cancelled_transfers.is_empty(),
        "500 cancels for transfers that never existed left {} entries behind",
        inner.cancelled_transfers.len()
    );
    drop(inner);
    assert_no_message(&mut rx).await;
}

/// A download that ends on its own must not leave its cancel behind either: the
/// marker outliving its only reader is the same leak arriving by a slower route.
#[tokio::test]
async fn a_download_clears_its_own_cancel_when_it_stops() {
    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("small.bin");
    tokio::fs::write(&file_path, b"body").await.unwrap();
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let transfer_id = "00000000-0000-0000-0000-0000000000ee".to_string();
    // Cancelled before it starts, so the loop returns on its first check.
    {
        let mut inner = d.inner.lock().await;
        inner
            .cancelled_transfers
            .insert(TransferKey::new("c1", &transfer_id));
    }

    d.serve_download(
        "c1".into(),
        DownloadRequest {
            transfer_id: transfer_id.clone(),
            file_path: file_path.to_string_lossy().to_string(),
        },
    )
    .await
    .unwrap();

    let inner = d.inner.lock().await;
    assert!(
        inner.live_downloads.is_empty(),
        "the registration is released"
    );
    assert!(
        inner.cancelled_transfers.is_empty(),
        "the cancel is gone with the loop that was reading it"
    );
    drop(inner);
    let _ = rx.recv().await.expect("DownloadResponse");
}
