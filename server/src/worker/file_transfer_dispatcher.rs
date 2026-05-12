//! # Worker-side file-transfer dispatcher (Arch IV PR 4 cut 2)
//!
//! Mirrors the Arch III bidirectional protocol from
//! `service::file_transfer::handle_file_transfer_event` but split
//! across the daemon/worker IPC boundary on a *dedicated* file lane —
//! see [`desk_ipc_protocol::dual_transport`] for the three-pipe IPC
//! topology (event / media / file).
//!
//! - **Browser → host (upload + download requests + cancels)**: the
//!   daemon's `on_data_channel` router forwards the
//!   `file_transfer_event` DC payload onto the **file lane** via
//!   `WorkerManager::send_file_to_worker`. Each frame carries `is_text`
//!   so the dispatcher knows whether to JSON-decode a
//!   `FileTransferMessage` (control frame) or treat the bytes as a
//!   binary upload chunk. The worker's session task runs a drain
//!   loop on the file-lane receiver that calls into
//!   [`FileTransferDispatcher::handle_command`].
//!
//! - **Host → browser (download chunks + control replies)**: the
//!   dispatcher's `emit_*` helpers send a [`FileTransferPayload`] over
//!   the same file lane in the worker→daemon direction. The daemon's
//!   file-lane drain task hands each payload to
//!   `pc_manager::write_file_transfer_data`, which writes it to the
//!   matching browser's `file_transfer_event` DC using `dc.send_text`
//!   or `dc.send` according to `is_text`.
//!
//! ## Filesystem privilege
//!
//! File IO runs in the worker process which is launched on the user's
//! token. Reading and writing files therefore respects the desktop
//! user's permissions, the same as Arch III. The daemon (SYSTEM) is
//! deliberately a byte pump and never opens a file directly — that
//! would bypass per-user ACLs.
//!
//! ## Permission gating
//!
//! The daemon's DC router gates browser→worker forwarding on
//! `accept_control` (see `pc_manager::route_is_permitted`). On top of
//! that the worker re-checks the finer-grained
//! `check_security_permission(allow_file_transfer, FileTransfer)` gate
//! once per connection — see `permission_for` and the
//! `DispatcherInner::permission_cache` field. The cache mirrors Arch
//! III's per-DC behaviour: each connection prompts at most once
//! (further commands hit the cache), the entry is dropped on
//! `stop_connection`, and the whole map clears on `shutdown`. The
//! settings-level "remember = allow" / "remember = deny" choice
//! still short-circuits the prompt entirely without user
//! interaction.
//!
//! ## Backpressure
//!
//! The Arch III handler watched `dc.buffered_amount()` to throttle
//! download chunk emission when SCTP buffers grew above 2 MB. Arch IV
//! re-establishes the same end-to-end backpressure by routing all
//! file traffic over the dedicated file lane (`FILE_QUEUE_CAP = 32`
//! per direction). When the browser DataChannel slows, SCTP fills →
//! `dc.send().await` blocks → the daemon's per-connection bounded
//! writer queue fills → its `write_file_transfer_data` drain blocks
//! → the file pipe fills → the worker's `file_sender.send().await`
//! in `emit_*` blocks → `serve_download` parks before the next
//! `file.read()`. The chain holds the worker process at ~4 MB
//! steady-state regardless of file size. `emit_*` returns
//! [`TransportError::Closed`] when the file lane has gone away (peer
//! restart / shutdown); callers fail-fast on that path so dangling
//! file handles are released promptly.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use desk_ipc_protocol::dual_transport::{EventSender, TransportError};
use desk_ipc_protocol::message::{
    FileTransferPayload, FileTransferSendErrorKind, FileTransferSendFailedPayload,
    StartMediaPayload, StopMediaPayload,
};
use log::{debug, error, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as TokioMutex;

use crate::host_control::HostControlHub;
use crate::model::file_transfer::*;
use crate::model::security_approval::{SecurityPermissionType, check_security_permission};
use crate::model::settings::SharedSettings;

/// Per-chunk DC payload size for downloads (host → browser).
///
/// Raised from 60 KB to 240 KiB after the 2026-05-11 ft-metrics
/// investigation pinned the bottleneck on `webrtc-rs` `dc.send` itself:
/// every 60 KB frame burned ~20 ms of single-core CPU inside the SCTP
/// stack while the browser-side SCTP receive buffer sat at <300 KB
/// (i.e. the receiver was perpetually starved, not the sender's
/// link). The per-message overhead (TSN allocation, EOR fragmentation
/// bookkeeping, congestion-control work, interceptor pipeline)
/// dominated the per-byte cost, so amortizing that fixed cost across
/// a ~4× larger payload lifts throughput proportionally on CPU-bound
/// LAN transfers — empirically confirmed at ~230 MB/s on the worker
/// side in the same investigation.
///
/// ## Why exactly 240 KiB and not 256 KiB
///
/// The first attempt used 256 KiB (262144) which corresponds *exactly*
/// to Chrome's SDP-advertised `a=max-message-size:262144`. webrtc-sctp
/// enforces that limit on the **wire-level message size**, which is
/// `chunk_size + BINARY_HEADER_SIZE (40)`. A 256 KiB payload yields a
/// 262184-byte SCTP message — **40 bytes over Chrome's advertise**.
/// Every binary chunk was rejected with `ErrOutboundPacketTooLarge`;
/// the daemon writer only logged a warn and continued draining, so the
/// worker saw a clean "completed" while the browser received an empty
/// blob (TransferComplete being a tiny text frame that fits within the
/// limit got through, triggering the false-positive UI completion).
/// Lesson: the limit is on the wire-level message size, not the
/// payload.
///
/// 240 KiB leaves ~16 KiB of headroom for the 40-byte header plus any
/// future on-wire protocol field expansion and tolerates browsers that
/// advertise slightly under 256 KiB (some older versions / forks).
/// Throughput impact relative to the 256 KiB attempt is negligible
/// (~6% smaller payload) but eliminates the rejection failure mode.
///
/// ## Browser compatibility
///
/// - Chrome ≥ 76 (Aug 2019): advertises `max-message-size:262144`.
/// - Firefox: advertises `max-message-size:1073741823` (~1 GB).
/// - Safari: ≥ 256 KiB on recent versions.
///
/// The daemon negotiates `SctpMaxMessageSize::Unbounded` (see
/// `build_peer_connection` in `daemon::pc_manager`) so it does not
/// further constrain the send size beyond what the remote advertises.
///
/// ## Downstream sizing impact
///
/// - daemon `FILE_TRANSFER_WRITER_QUEUE_CAP = 16` → 16 × 240 KiB ≈
///   3.75 MB per-PC steady-state buffer (was 960 KB at 60 KB).
/// - file-lane `FILE_QUEUE_CAP = 32` per direction → 32 × 240 KiB ≈
///   7.5 MB per direction (was 1.9 MB).
///
/// Both still well below memory pressure thresholds for a single
/// active transfer.
pub(crate) const FILE_TRANSFER_CHUNK_SIZE_TX: usize = 240 * 1024;
const YIELD_EVERY_N_CHUNKS: u32 = 100;

/// Window size (in chunks) for file-transfer throughput / latency
/// breakdown logging. Each window flushes one `[ft-metrics]` INFO line
/// with per-stage timings + instant throughput. Sized so a 60 KB chunk
/// pipeline emits one line per ~15 MB transferred, which keeps log
/// volume sane on multi-GB transfers while still surfacing transient
/// stalls (a 256-chunk window at 2 MB/s is ~7.5 s, well within the
/// "user complains it's slow" window). The daemon writer task mirrors
/// this constant for its own `[ft-metrics-daemon]` lines so the two
/// halves can be cross-referenced by `transfer_id` / `connection_id`.
pub(crate) const FT_METRICS_WINDOW_CHUNKS: u32 = 256;

/// Rolling per-window accumulator for the download (host → browser)
/// path. Pure data + arithmetic — all timing samples are pushed in
/// from `serve_download`. Exists as a separate struct so the
/// flush / throughput math is unit-testable without spinning up a
/// dispatcher / tokio runtime.
///
/// Throughput is computed against `wall_ns` (loop iteration wall time)
/// rather than the sum of stage timings, because the dominant stall
/// in this pipeline is `emit_binary().await` parking on a full file
/// lane — that's wall time, not CPU time, and showing it as such is
/// the entire point of the metric.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownloadWindow {
    pub chunks: u32,
    pub bytes: u64,
    pub disk_read_ns: u64,
    pub build_chunk_ns: u64,
    pub emit_await_ns: u64,
    pub wall_ns: u64,
}

impl DownloadWindow {
    pub(crate) fn record(
        &mut self,
        bytes: u64,
        disk_read: Duration,
        build: Duration,
        emit_await: Duration,
        wall: Duration,
    ) {
        self.chunks = self.chunks.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.disk_read_ns = self.disk_read_ns.saturating_add(duration_ns(disk_read));
        self.build_chunk_ns = self.build_chunk_ns.saturating_add(duration_ns(build));
        self.emit_await_ns = self.emit_await_ns.saturating_add(duration_ns(emit_await));
        self.wall_ns = self.wall_ns.saturating_add(duration_ns(wall));
    }

    pub(crate) fn is_full(&self) -> bool {
        self.chunks >= FT_METRICS_WINDOW_CHUNKS
    }

    /// Render one INFO line summarising the window. Returns `None`
    /// when there is nothing to report (called on an empty accumulator
    /// at shutdown). The caller resets the window after logging.
    pub(crate) fn flush_line(&self, transfer_id: &str, tag: &'static str) -> Option<String> {
        if self.chunks == 0 {
            return None;
        }
        let mbps = throughput_mbps(self.bytes, self.wall_ns);
        Some(format!(
            "[{tag}] tid={tid} chunks={c} bytes={b} wall={wm:.2}ms \
             disk_read={dm:.2}ms build={bm:.2}ms emit_await={em:.2}ms \
             throughput={mbps:.2}MB/s",
            tid = transfer_id,
            c = self.chunks,
            b = self.bytes,
            wm = ns_to_ms(self.wall_ns),
            dm = ns_to_ms(self.disk_read_ns),
            bm = ns_to_ms(self.build_chunk_ns),
            em = ns_to_ms(self.emit_await_ns),
        ))
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Rolling per-window accumulator for the upload (browser → host)
/// path. Mirrors [`DownloadWindow`] but tracks the upload-specific
/// stages: time spent waiting on the dispatcher inner mutex (`lock_ns`)
/// and time spent in `state.file.write_all().await` (`disk_write_ns`).
/// The lock wait surfaces lock contention between the upload chunk
/// path and concurrent control messages / cancels; the disk write
/// surfaces filesystem-level stalls.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadWindow {
    pub chunks: u32,
    pub bytes: u64,
    pub lock_ns: u64,
    pub disk_write_ns: u64,
    pub wall_ns: u64,
}

impl UploadWindow {
    pub(crate) fn record(
        &mut self,
        bytes: u64,
        lock_wait: Duration,
        disk_write: Duration,
        wall: Duration,
    ) {
        self.chunks = self.chunks.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.lock_ns = self.lock_ns.saturating_add(duration_ns(lock_wait));
        self.disk_write_ns = self.disk_write_ns.saturating_add(duration_ns(disk_write));
        self.wall_ns = self.wall_ns.saturating_add(duration_ns(wall));
    }

    pub(crate) fn is_full(&self) -> bool {
        self.chunks >= FT_METRICS_WINDOW_CHUNKS
    }

    pub(crate) fn flush_line(&self, transfer_id: &str, tag: &'static str) -> Option<String> {
        if self.chunks == 0 {
            return None;
        }
        let mbps = throughput_mbps(self.bytes, self.wall_ns);
        Some(format!(
            "[{tag}] tid={tid} chunks={c} bytes={b} wall={wm:.2}ms \
             lock_wait={lm:.2}ms disk_write={dm:.2}ms throughput={mbps:.2}MB/s",
            tid = transfer_id,
            c = self.chunks,
            b = self.bytes,
            wm = ns_to_ms(self.wall_ns),
            lm = ns_to_ms(self.lock_ns),
            dm = ns_to_ms(self.disk_write_ns),
        ))
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Convert a [`Duration`] to ns saturating at `u64::MAX`. `Duration::as_nanos`
/// returns u128 because the type covers ~584 years; the metric windows
/// here cover a few seconds at most, so the truncation is a no-op in
/// practice and lets the accumulators stay on plain `u64`. Exposed at
/// `pub(crate)` so the daemon-side `DaemonFtWindow` reuses the same
/// saturating-cast policy without duplicating the helper.
pub(crate) fn duration_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn ns_to_ms(ns: u64) -> f64 {
    (ns as f64) / 1_000_000.0
}

/// Compute MB/s (decimal megabytes, matching the user-visible UI in
/// `use-file-transfer.ts`) from a byte count and a wall-time duration
/// in nanoseconds. Returns `0.0` when `wall_ns == 0` to avoid the
/// `0/0` case at startup.
pub(crate) fn throughput_mbps(bytes: u64, wall_ns: u64) -> f64 {
    if wall_ns == 0 {
        return 0.0;
    }
    // bytes / wall_secs / 1e6 = bytes * 1e9 / wall_ns / 1e6 = bytes * 1000 / wall_ns
    (bytes as f64) * 1_000.0 / (wall_ns as f64)
}

/// Per-transfer in-flight upload state (browser uploading to host).
struct UploadState {
    file: tokio::fs::File,
    file_path: PathBuf,
    total_chunks: u64,
    received_chunks: u64,
    /// Per-transfer metrics window. Flushed every
    /// [`FT_METRICS_WINDOW_CHUNKS`] chunks and once more on completion
    /// so the final partial window does not get lost.
    metrics: UploadWindow,
}

struct DispatcherInner {
    upload_states: HashMap<String, UploadState>,
    cancelled_transfers: HashSet<String>,
    active_connections: HashSet<String>,
    /// Per-connection cached `allow_file_transfer` decision. Mirrors the
    /// per-DC permission cache from Arch III's
    /// `service::file_transfer::handle_file_transfer_event` so each
    /// connection only triggers the Tauri approval prompt at most once,
    /// regardless of how many DownloadRequest / UploadRequest /
    /// chunk frames flow over its `file_transfer_event` DC.
    permission_cache: HashMap<String, bool>,
}

impl DispatcherInner {
    fn new() -> Self {
        Self {
            upload_states: HashMap::new(),
            cancelled_transfers: HashSet::new(),
            active_connections: HashSet::new(),
            permission_cache: HashMap::new(),
        }
    }
}

/// Worker-side file-transfer dispatcher. Cheap to clone (`Arc` inside)
/// so the IPC loop can take a clone for each call site.
#[derive(Clone)]
pub struct FileTransferDispatcher {
    inner: Arc<TokioMutex<DispatcherInner>>,
    /// Dedicated file-lane sender (worker → daemon) for download
    /// chunks and control replies. Bounded (`FILE_QUEUE_CAP = 32`); a
    /// full lane parks `send().await` so the chain
    /// `dc.send().await` ↔ daemon writer queue ↔ file pipe ↔ this
    /// sender ↔ `serve_download` loop applies end-to-end backpressure
    /// without spilling file bytes onto the event lane.
    file_sender: Arc<dyn EventSender<FileTransferPayload>>,
    /// Shared settings used by the permission gate to read
    /// `security.allow_file_transfer` and (when the user picks
    /// "remember") to persist the choice via `Settings::save()`.
    settings: Arc<SharedSettings>,
    /// Host-control hub used by `check_security_permission` to surface
    /// the approval prompt in the Tauri shell. In portable mode this is
    /// the daemon's hub directly (shared in-process); in named-pipe
    /// mode it's the worker's Forwarder hub that bridges back to the
    /// daemon's aggregator over ws.
    hub: Arc<HostControlHub>,
}

impl FileTransferDispatcher {
    pub fn new(
        file_sender: Arc<dyn EventSender<FileTransferPayload>>,
        settings: Arc<SharedSettings>,
        hub: Arc<HostControlHub>,
    ) -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(DispatcherInner::new())),
            file_sender,
            settings,
            hub,
        }
    }

    /// Resolve the `allow_file_transfer` decision for a connection.
    ///
    /// Returns the cached value if a previous command on the same
    /// connection already established one. Otherwise calls
    /// [`check_security_permission`] (which prompts the user via Tauri
    /// when the saved preference is `None`) and stores the result.
    ///
    /// Race tolerance: two concurrent commands on a fresh connection
    /// can both miss the cache and call into `check_security_permission`.
    /// Tauri dedups identical pending requests by req-id, so the user
    /// sees one prompt; the two callers receive the same answer and
    /// race to write the same value into the cache. Either ordering
    /// is correct.
    async fn permission_for(&self, connection_id: &str) -> bool {
        {
            let inner = self.inner.lock().await;
            if let Some(&v) = inner.permission_cache.get(connection_id) {
                return v;
            }
        }
        let allow_transfer = self.settings.read().await.security.allow_file_transfer;
        let approved = check_security_permission(
            &self.settings,
            &self.hub,
            allow_transfer,
            SecurityPermissionType::FileTransfer,
            Some(connection_id.to_string()),
        )
        .await;
        let mut inner = self.inner.lock().await;
        inner
            .permission_cache
            .insert(connection_id.to_string(), approved);
        approved
    }

    /// Add a connection to the active set. Subsequent file-lane
    /// commands for this `connection_id` will be processed; commands
    /// for inactive connections are dropped.
    pub async fn start_connection(&self, payload: &StartMediaPayload) {
        let mut inner = self.inner.lock().await;
        let inserted = inner
            .active_connections
            .insert(payload.connection_id.clone());
        if !inserted {
            debug!(
                "[FileTransferDispatcher] {}: duplicate StartMedia",
                payload.connection_id
            );
        }
        info!(
            "[FileTransferDispatcher] {}: subscribed (active_count={})",
            payload.connection_id,
            inner.active_connections.len()
        );
    }

    /// Remove a connection from the active set. Any in-flight uploads
    /// for that connection are NOT immediately torn down — the upload
    /// state is keyed by `transfer_id`, not `connection_id`, and we
    /// have no map between them. Practically: when a connection
    /// closes, the partial uploads finish on disk on their own
    /// `TransferComplete` ack from the browser; orphaned states are
    /// dropped on `shutdown`.
    pub async fn stop_connection(&self, payload: &StopMediaPayload) {
        let mut inner = self.inner.lock().await;
        if inner.active_connections.remove(&payload.connection_id) {
            info!(
                "[FileTransferDispatcher] {}: unsubscribed (active_count={})",
                payload.connection_id,
                inner.active_connections.len()
            );
        }
        // Drop the cached permission so a subsequent connection (which
        // may be from a different browser session) re-prompts. The
        // settings-level "allow_file_transfer = Some(true)" remembered
        // choice still short-circuits the prompt without user
        // interaction, so this is cheap correctness, not a UX cost.
        inner.permission_cache.remove(&payload.connection_id);
    }

    /// Drop every connection + every in-flight transfer state. Called
    /// on worker shutdown so dangling file handles do not outlive the
    /// process.
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        inner.active_connections.clear();
        inner.upload_states.clear();
        inner.cancelled_transfers.clear();
        inner.permission_cache.clear();
    }

    /// Apply an incoming file-transfer command. The bytes are either
    /// a JSON `FileTransferMessage` (control frame, `is_text=true`)
    /// or a binary chunk header + payload (`is_text=false`).
    pub async fn handle_command(&self, payload: FileTransferPayload) {
        // Liveness gate: ignore commands for connections we have not
        // been told about. The daemon DC router can race a stop, and
        // we do not want a stray packet to allocate state on a
        // closed-out connection.
        {
            let inner = self.inner.lock().await;
            if !inner.active_connections.contains(&payload.connection_id) {
                debug!(
                    "[FileTransferDispatcher] {}: command for inactive connection — dropping",
                    payload.connection_id
                );
                return;
            }
        }
        // Permission gate: file transfer is on its own access category
        // (`allow_file_transfer`), independent of `accept_control` which
        // governs mouse/keyboard. The daemon's DC router used to gate
        // `file_transfer_event` on `accept_control`; that was wrong —
        // the file management UI in the browser opens a fresh WebRTC
        // connection that has never requested control, so every
        // download/upload would be silently dropped at the daemon.
        // The daemon now passes file_transfer through unconditionally
        // and we do the actual permission check here, mirroring Arch
        // III's behavior in `service::file_transfer::handle_file_transfer_event`.
        if !self.permission_for(&payload.connection_id).await {
            warn!(
                "[FileTransferDispatcher] {}: permission denied — dropping command",
                payload.connection_id
            );
            return;
        }
        if payload.is_text {
            self.handle_text(payload).await;
        } else {
            self.handle_binary(payload).await;
        }
    }

    /// React to a daemon-side `dc.send` failure
    /// ([`ServiceToWorker::FileTransferSendFailed`]). The daemon has
    /// already logged the wire error at an appropriate severity; here
    /// we tear down the matching transfer state and tell the browser
    /// what happened. The browser listens for `TransferError` and
    /// surfaces it as a toast / cancels its progress bar.
    ///
    /// Abort scope:
    ///
    /// - `Some(transfer_id)` — abort just that transfer. Downloads
    ///   stop emitting chunks on the next loop iteration via
    ///   `cancelled_transfers`; uploads release any in-flight file
    ///   handle and remove the partial file from disk so it doesn't
    ///   orphan.
    /// - `None` — fall back to aborting every in-flight upload + every
    ///   download for `connection_id`. Used when the daemon could not
    ///   attribute the failure to a specific transfer (legacy payload
    ///   without `transfer_id`).
    ///
    /// The browser-facing `TransferError` message intentionally
    /// includes the daemon's `kind` + `error` string so a user-visible
    /// toast can distinguish "PacketTooLarge — please update the
    /// server" from "TransportClosed — connection dropped". The
    /// `chunk_index` (when present) goes into the message body for
    /// easier log correlation against the worker's `ft-metrics` line.
    pub async fn handle_send_failed(&self, payload: FileTransferSendFailedPayload) {
        let FileTransferSendFailedPayload {
            connection_id,
            transfer_id,
            chunk_index,
            kind,
            error,
        } = payload;
        let kind_label = match kind {
            FileTransferSendErrorKind::PacketTooLarge => "PacketTooLarge",
            FileTransferSendErrorKind::TransportClosed => "TransportClosed",
            FileTransferSendErrorKind::Other => "Other",
        };
        let message = match chunk_index {
            Some(idx) => format!("daemon dc.send failed [{kind_label}] at chunk {idx}: {error}"),
            None => format!("daemon dc.send failed [{kind_label}]: {error}"),
        };
        // Collect every transfer_id we need to abort. With a specific
        // transfer_id this is just `[id]`; with None we snapshot every
        // active upload + every active download for this connection.
        let aborted_ids: Vec<String> = {
            let mut inner = self.inner.lock().await;
            let ids: Vec<String> = match transfer_id.as_deref() {
                Some(tid) => vec![tid.to_string()],
                None => inner.upload_states.keys().cloned().collect(),
            };
            for tid in &ids {
                inner.cancelled_transfers.insert(tid.clone());
            }
            ids
        };
        warn!(
            "[FileTransferDispatcher] {}: send failure [{kind_label}] — aborting \
             {} transfer(s); {}",
            connection_id,
            aborted_ids.len(),
            error
        );
        // For uploads: release the file handle and remove the partial
        // file so it doesn't orphan on disk. Mirrors the cleanup the
        // TransferCancel arm already does.
        let mut upload_paths_to_remove: Vec<std::path::PathBuf> = Vec::new();
        {
            let mut inner = self.inner.lock().await;
            for tid in &aborted_ids {
                if let Some(state) = inner.upload_states.remove(tid) {
                    upload_paths_to_remove.push(state.file_path.clone());
                }
            }
        }
        for path in upload_paths_to_remove {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                debug!(
                    "[FileTransferDispatcher] failed to remove partial upload {} after \
                     send failure: {e}",
                    path.display()
                );
            }
        }
        // Surface the failure back to the browser for each transfer.
        // If the file lane itself is what's broken these emits may
        // fail too — that's expected; the browser's SCTP timeout will
        // eventually surface the disconnect on its own.
        for tid in &aborted_ids {
            if let Err(e) = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: tid.clone(),
                        message: message.clone(),
                    }),
                )
                .await
            {
                debug!(
                    "[FileTransferDispatcher] {}: emit TransferError for {} after send \
                     failure also failed: {e}",
                    connection_id, tid
                );
            }
        }
    }

    async fn handle_text(&self, payload: FileTransferPayload) {
        let s = match std::str::from_utf8(&payload.data) {
            Ok(s) => s,
            Err(e) => {
                error!(
                    "[FileTransferDispatcher] {}: control payload not UTF-8: {e}",
                    payload.connection_id
                );
                return;
            }
        };
        let msg: FileTransferMessage = match serde_json::from_str(s) {
            Ok(m) => m,
            Err(e) => {
                error!(
                    "[FileTransferDispatcher] {}: control JSON decode failed: {e}",
                    payload.connection_id
                );
                return;
            }
        };
        match msg {
            FileTransferMessage::DownloadRequest(req) => {
                let dispatcher = self.clone();
                let connection_id = payload.connection_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = dispatcher.serve_download(connection_id, req).await {
                        error!("[FileTransferDispatcher] download error: {e}");
                    }
                });
            }
            FileTransferMessage::UploadRequest(req) => {
                if let Err(e) = self.accept_upload(payload.connection_id.clone(), req).await {
                    error!("[FileTransferDispatcher] upload accept error: {e}");
                }
            }
            FileTransferMessage::TransferComplete(complete) => {
                let mut inner = self.inner.lock().await;
                if let Some(state) = inner.upload_states.remove(&complete.transfer_id) {
                    info!(
                        "[FileTransferDispatcher] upload {} completed, received {} chunks",
                        complete.transfer_id, state.received_chunks
                    );
                }
            }
            FileTransferMessage::TransferCancel(cancel) => {
                info!(
                    "[FileTransferDispatcher] {}: cancel transfer_id={}",
                    payload.connection_id, cancel.transfer_id
                );
                let removed_upload = {
                    let mut inner = self.inner.lock().await;
                    inner.cancelled_transfers.insert(cancel.transfer_id.clone());
                    inner.upload_states.remove(&cancel.transfer_id)
                };
                if let Some(state) = removed_upload {
                    let path = state.file_path.clone();
                    drop(state);
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        warn!(
                            "[FileTransferDispatcher] failed to remove cancelled upload file {}: {e}",
                            path.display()
                        );
                    }
                }
            }
            other => {
                debug!(
                    "[FileTransferDispatcher] {}: ignoring browser-side message variant {:?}",
                    payload.connection_id, other
                );
            }
        }
    }

    async fn handle_binary(&self, payload: FileTransferPayload) {
        let iter_start = Instant::now();
        let (transfer_id, chunk_index, chunk_data) = match parse_binary_chunk(&payload.data) {
            Some(t) => t,
            None => {
                warn!(
                    "[FileTransferDispatcher] {}: binary chunk too short ({} bytes)",
                    payload.connection_id,
                    payload.data.len()
                );
                return;
            }
        };
        let transfer_id = transfer_id.to_string();
        let chunk_data = chunk_data.to_vec();
        let chunk_len = chunk_data.len() as u64;
        let connection_id = payload.connection_id.clone();
        let lock_start = Instant::now();
        let mut inner = self.inner.lock().await;
        let lock_elapsed = lock_start.elapsed();
        let complete = match inner.upload_states.get_mut(&transfer_id) {
            Some(state) => {
                let write_start = Instant::now();
                if let Err(e) = state.file.write_all(&chunk_data).await {
                    error!(
                        "[FileTransferDispatcher] {}: write chunk {} for {} failed: {e}",
                        connection_id, chunk_index, transfer_id
                    );
                    return;
                }
                let write_elapsed = write_start.elapsed();
                state.received_chunks += 1;
                state
                    .metrics
                    .record(chunk_len, lock_elapsed, write_elapsed, iter_start.elapsed());
                if state.metrics.is_full() {
                    if let Some(line) = state.metrics.flush_line(&transfer_id, "ft-metrics") {
                        info!("{line}");
                    }
                    state.metrics.reset();
                }
                state.received_chunks >= state.total_chunks
            }
            None => {
                warn!(
                    "[FileTransferDispatcher] {}: chunk for unknown transfer {}",
                    connection_id, transfer_id
                );
                return;
            }
        };
        if complete && let Some(mut state) = inner.upload_states.remove(&transfer_id) {
            if let Err(e) = state.file.flush().await {
                error!(
                    "[FileTransferDispatcher] {}: flush of completed upload {} failed: {e}",
                    connection_id, transfer_id
                );
            }
            // Trailing partial-window flush mirrors the download path so
            // the last <256 chunks of an upload do not vanish from the log.
            if let Some(line) = state.metrics.flush_line(&transfer_id, "ft-metrics") {
                info!("{line}");
            }
            drop(state);
            // Drop the lock before emitting IPC: emit_text awaits on the
            // bounded file lane and must not hold inner across that wait.
            drop(inner);
            if let Err(e) = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferComplete(TransferComplete {
                        transfer_id: transfer_id.clone(),
                    }),
                )
                .await
            {
                warn!("[FileTransferDispatcher] upload {transfer_id} complete ack drop: {e}");
                return;
            }
            info!(
                "[FileTransferDispatcher] upload {} completed successfully",
                transfer_id
            );
        }
    }

    async fn accept_upload(
        &self,
        connection_id: String,
        req: UploadRequest,
    ) -> std::io::Result<()> {
        let target_dir = PathBuf::from(&req.target_dir);
        if !target_dir.exists() || !target_dir.is_dir() {
            // Best-effort error notification to the browser. If the
            // file lane is closed there is nothing to clean up (no file
            // opened yet) — just log and return.
            if let Err(e) = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: req.transfer_id.clone(),
                        message: format!("Target directory not found: {}", req.target_dir),
                    }),
                )
                .await
            {
                warn!(
                    "[FileTransferDispatcher] {}: failed to emit TransferError for {}: {e}",
                    connection_id, req.transfer_id
                );
            }
            return Ok(());
        }
        let file_path = target_dir.join(&req.file_name);
        let file = tokio::fs::File::create(&file_path).await?;
        info!(
            "[FileTransferDispatcher] upload {} started: {} -> {} ({} bytes, {} chunks)",
            req.transfer_id,
            req.file_name,
            file_path.display(),
            req.file_size,
            req.total_chunks
        );
        let state = UploadState {
            file,
            file_path: file_path.clone(),
            total_chunks: req.total_chunks,
            received_chunks: 0,
            metrics: UploadWindow::default(),
        };
        {
            let mut inner = self.inner.lock().await;
            inner.upload_states.insert(req.transfer_id.clone(), state);
        }
        if let Err(e) = self
            .emit_text(
                &connection_id,
                FileTransferMessage::UploadResponse(UploadResponse {
                    transfer_id: req.transfer_id.clone(),
                    accepted: true,
                    message: None,
                }),
            )
            .await
        {
            // File lane closed before the browser could receive the
            // accept ack — no chunks will arrive. Drop the in-flight
            // state and remove the empty placeholder file we just
            // created, otherwise it would orphan on disk.
            warn!(
                "[FileTransferDispatcher] {}: UploadResponse emit failed for {}: {e} \
                 — releasing in-flight state",
                connection_id, req.transfer_id
            );
            let removed = {
                let mut inner = self.inner.lock().await;
                inner.upload_states.remove(&req.transfer_id)
            };
            if let Some(state) = removed {
                let path = state.file_path.clone();
                drop(state);
                if let Err(rm_err) = tokio::fs::remove_file(&path).await {
                    warn!(
                        "[FileTransferDispatcher] failed to clean up {} after upload \
                         abort: {rm_err}",
                        path.display()
                    );
                }
            }
        }
        Ok(())
    }

    async fn serve_download(
        &self,
        connection_id: String,
        req: DownloadRequest,
    ) -> std::io::Result<()> {
        let path = PathBuf::from(&req.file_path);
        if !path.exists() || !path.is_file() {
            if let Err(e) = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: req.transfer_id.clone(),
                        message: format!("File not found: {}", req.file_path),
                    }),
                )
                .await
            {
                warn!(
                    "[FileTransferDispatcher] {}: failed to emit TransferError for {}: {e}",
                    connection_id, req.transfer_id
                );
            }
            return Ok(());
        }
        let metadata = tokio::fs::metadata(&path).await?;
        let file_size = metadata.len();
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let chunk_size = FILE_TRANSFER_CHUNK_SIZE_TX;
        let total_chunks = file_size.div_ceil(chunk_size as u64);

        if let Err(e) = self
            .emit_text(
                &connection_id,
                FileTransferMessage::DownloadResponse(DownloadResponse {
                    transfer_id: req.transfer_id.clone(),
                    file_name: file_name.clone(),
                    file_size,
                    chunk_size,
                    total_chunks,
                }),
            )
            .await
        {
            warn!(
                "[FileTransferDispatcher] {}: download {} aborted before open: {e}",
                connection_id, req.transfer_id
            );
            return Ok(());
        }

        let mut file = tokio::fs::File::open(&path).await?;
        let mut buf = vec![0u8; chunk_size];
        let mut chunk_index: u32 = 0;
        let mut window = DownloadWindow::default();
        info!(
            "[FileTransferDispatcher] download {} starting: {} chunks",
            req.transfer_id, total_chunks
        );
        loop {
            // Check cancel flag before doing more IO.
            {
                let mut inner = self.inner.lock().await;
                if inner.cancelled_transfers.remove(&req.transfer_id) {
                    info!(
                        "[FileTransferDispatcher] download {} cancelled",
                        req.transfer_id
                    );
                    return Ok(());
                }
            }
            let iter_start = Instant::now();
            let read_start = Instant::now();
            let n = file.read(&mut buf).await?;
            let read_elapsed = read_start.elapsed();
            if n == 0 {
                break;
            }
            let build_start = Instant::now();
            let chunk_bytes = build_binary_chunk(&req.transfer_id, chunk_index, &buf[..n]);
            let build_elapsed = build_start.elapsed();
            // Fail-fast on file-lane closure: dropping `file` here
            // releases the OS handle promptly. Continuing to read
            // would just fill memory while no one drains.
            let emit_start = Instant::now();
            if let Err(e) = self
                .emit_binary(&connection_id, &req.transfer_id, chunk_bytes)
                .await
            {
                warn!(
                    "[FileTransferDispatcher] {}: download {} aborted at chunk {}: {e}",
                    connection_id, req.transfer_id, chunk_index
                );
                return Ok(());
            }
            let emit_elapsed = emit_start.elapsed();
            window.record(
                n as u64,
                read_elapsed,
                build_elapsed,
                emit_elapsed,
                iter_start.elapsed(),
            );
            if window.is_full() {
                if let Some(line) = window.flush_line(&req.transfer_id, "ft-metrics") {
                    info!("{line}");
                }
                window.reset();
            }
            chunk_index += 1;
            if chunk_index.is_multiple_of(YIELD_EVERY_N_CHUNKS) {
                tokio::task::yield_now().await;
            }
        }
        // Flush any trailing partial window so a small file or the
        // last few chunks of a large file still surface in the log.
        if let Some(line) = window.flush_line(&req.transfer_id, "ft-metrics") {
            info!("{line}");
        }
        if let Err(e) = self
            .emit_text(
                &connection_id,
                FileTransferMessage::TransferComplete(TransferComplete {
                    transfer_id: req.transfer_id.clone(),
                }),
            )
            .await
        {
            warn!(
                "[FileTransferDispatcher] {}: TransferComplete emit failed for {}: {e}",
                connection_id, req.transfer_id
            );
            return Ok(());
        }
        info!(
            "[FileTransferDispatcher] download {} completed: {} bytes, {} chunks",
            req.transfer_id, file_size, chunk_index
        );
        Ok(())
    }

    /// Emit a JSON-encoded control message over the file lane. Returns
    /// [`TransportError::Closed`] if the lane has gone away; serialization
    /// failures are programmer bugs (`FileTransferMessage` is plain
    /// serde) and are logged + dropped without surfacing as a transport
    /// error.
    async fn emit_text(
        &self,
        connection_id: &str,
        msg: FileTransferMessage,
    ) -> Result<(), TransportError> {
        let transfer_id = transfer_id_of(&msg).map(str::to_string);
        let json = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                error!("[FileTransferDispatcher] serialize control message failed: {e}");
                return Ok(());
            }
        };
        self.emit_payload(connection_id, json, true, transfer_id)
            .await
    }

    async fn emit_binary(
        &self,
        connection_id: &str,
        transfer_id: &str,
        data: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.emit_payload(connection_id, data, false, Some(transfer_id.to_string()))
            .await
    }

    async fn emit_payload(
        &self,
        connection_id: &str,
        data: Vec<u8>,
        is_text: bool,
        transfer_id: Option<String>,
    ) -> Result<(), TransportError> {
        let payload = FileTransferPayload {
            connection_id: connection_id.to_string(),
            data,
            is_text,
            transfer_id,
        };
        // `send().await` parks on a full file lane (FILE_QUEUE_CAP = 32),
        // which is exactly the backpressure we want — see module docs.
        // It only returns `Err(Closed)` when the daemon-side receiver
        // has dropped (peer crash / shutdown), in which case callers
        // should fail-fast and release any open file handle.
        self.file_sender.send(payload).await
    }
}

/// Returns the `transfer_id` carried by every outbound
/// [`FileTransferMessage`] variant. The daemon-side writer needs this to
/// scope a `dc.send` failure to the right transfer when sending a
/// [`ServiceToWorker::FileTransferSendFailed`] back; without it the
/// failure can only be associated with the connection, forcing a coarse
/// abort of every transfer on that PC.
fn transfer_id_of(msg: &FileTransferMessage) -> Option<&str> {
    match msg {
        FileTransferMessage::DownloadRequest(m) => Some(&m.transfer_id),
        FileTransferMessage::DownloadResponse(m) => Some(&m.transfer_id),
        FileTransferMessage::UploadRequest(m) => Some(&m.transfer_id),
        FileTransferMessage::UploadResponse(m) => Some(&m.transfer_id),
        FileTransferMessage::TransferComplete(m) => Some(&m.transfer_id),
        FileTransferMessage::TransferError(m) => Some(&m.transfer_id),
        FileTransferMessage::TransferCancel(m) => Some(&m.transfer_id),
    }
}

#[cfg(test)]
mod tests {
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
        (FileTransferDispatcher::new(file_tx, shared, hub), file_rx)
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

    /// Permission gate: when `allow_file_transfer` is `Some(false)`, the
    /// dispatcher silently drops commands for active connections (the
    /// liveness gate would otherwise let them through). Reproduces the
    /// portable-mode bug where the daemon's accept_control gate dropped
    /// every download — by moving the check here, an explicit deny still
    /// blocks but the daemon no longer affects file_transfer routing.
    #[tokio::test]
    async fn handle_command_drops_when_permission_denied() {
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
        assert_no_message(&mut rx).await;
        // Cache is populated so subsequent commands short-circuit.
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
            let chunk_bytes =
                build_binary_chunk(&transfer_id, i, &vec![b'A' + i as u8; chunk_size]);
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
            inner.cancelled_transfers.insert(transfer_id.clone());
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

    // ============== F1: handle_send_failed ==============

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
            assert!(!inner.upload_states.contains_key(&id_a));
            assert!(inner.upload_states.contains_key(&id_b));
            assert!(inner.cancelled_transfers.contains(&id_a));
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
            assert!(inner.cancelled_transfers.contains(&id_a));
            assert!(inner.cancelled_transfers.contains(&id_b));
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
            assert!(inner.cancelled_transfers.contains(&tid));
        }
        let p = rx.recv().await.expect("TransferError emit");
        let parsed: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
        assert!(matches!(parsed, FileTransferMessage::TransferError(_)));
    }
}
