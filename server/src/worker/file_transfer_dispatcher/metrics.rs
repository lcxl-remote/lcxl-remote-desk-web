use super::*;

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
pub(super) const YIELD_EVERY_N_CHUNKS: u32 = 100;

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
