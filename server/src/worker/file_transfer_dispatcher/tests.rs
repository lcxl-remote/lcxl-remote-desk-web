use super::*;
use crate::model::settings::Settings;
use desk_ipc_protocol::dual_transport::{EventReceiver, inprocess};
use desk_ipc_protocol::message::MediaCodec;
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
    let mut settings = Settings::default();
    settings.security.allow_file_transfer = allow_file_transfer;
    let shared = Arc::new(SharedSettings::from(settings));
    let hub = Arc::new(HostControlHub::new_local());
    let (file_tx, file_rx) =
        inprocess::make_event_inprocess_with_cap::<FileTransferPayload>(file_cap);
    (
        FileTransferDispatcher::new(
            file_tx,
            shared,
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
    let mut settings = Settings::default();
    settings.security.allow_file_transfer = Some(true);
    let shared = Arc::new(SharedSettings::from(settings));
    let hub = Arc::new(HostControlHub::new_local());
    let (file_tx, _file_rx) = inprocess::make_event_inprocess_with_cap::<FileTransferPayload>(16);
    let (activity_tx, activity_rx) = mpsc::unbounded_channel();
    (
        FileTransferDispatcher::new(
            file_tx,
            shared,
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
        assert_eq!(g.permission_cache.get("c1").copied(), Some(false));
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
    assert_eq!(g.permission_cache.get("c1").copied(), Some(false));
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
    assert_eq!(g.permission_cache.get("c1").copied(), Some(true));
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
    assert!(tmp.path().join("first.bin").exists());
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

/// Binary chunk shorter than the 40-byte header is silently
/// dropped (no panic, no IPC). Defends against a malformed
/// browser payload.
#[tokio::test]
async fn binary_chunk_too_short_drops_silently() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    d.handle_command(FileTransferPayload {
        connection_id: "c1".into(),
        data: vec![0u8; 10],
        is_text: false,
        transfer_id: None,
    })
    .await;
    assert_no_message(&mut rx).await;
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
            inner
                .cancelled_transfers
                .contains(&TransferKey::new("c1", &id_a))
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
            inner
                .cancelled_transfers
                .contains(&TransferKey::new("c1", &id_a))
        );
        assert!(
            inner
                .cancelled_transfers
                .contains(&TransferKey::new("c1", &id_b))
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

/// Cancel flag is set even when the targeted transfer is a
/// download (no upload_states entry). serve_download polls the
/// flag on each loop iteration, so this is how a daemon-side
/// send failure aborts a download already in flight.
#[tokio::test]
async fn handle_send_failed_for_download_sets_cancel_flag() {
    let (d, mut rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    let tid = "00000000-0000-0000-0000-0000000000dd".to_string();
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
