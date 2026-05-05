//! # Worker-side file-transfer dispatcher (Arch IV PR 4 cut 2)
//!
//! Mirrors the Arch III bidirectional protocol from
//! `service::file_transfer::handle_file_transfer_event` but split
//! across the daemon/worker IPC boundary:
//!
//! - **Browser → host (upload + download requests + cancels)**: the
//!   daemon's `on_data_channel` router forwards the
//!   `file_transfer_event` DC payload as
//!   `ServiceToWorker::FileTransferCommand`. The IPC carries
//!   `is_text` so the dispatcher knows whether to JSON-decode a
//!   `FileTransferMessage` (control frame) or treat the bytes as a
//!   binary upload chunk.
//!
//! - **Host → browser (download chunks + control replies)**: the
//!   dispatcher emits `WorkerToService::FileTransferData` with
//!   `is_text=true` for JSON control replies (`DownloadResponse` /
//!   `TransferComplete` / `TransferError`) and `is_text=false` for
//!   binary chunk bodies. The daemon's
//!   `pc_manager::write_file_transfer_data` writes them to the
//!   matching browser's `file_transfer_event` DC using
//!   `dc.send_text` or `dc.send` accordingly.
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
//! `accept_control` (see `pc_manager::route_is_permitted`). The
//! Arch III handler additionally cached
//! `check_security_permission(allow_file_transfer, FileTransfer)` on
//! a per-DC basis; restoring that finer-grained gate is left for a
//! follow-up. For PR 4 cut 2 the worker trusts the daemon's gate and
//! does not re-check.
//!
//! ## Backpressure
//!
//! The Arch III handler watched `dc.buffered_amount()` to throttle
//! download chunk emission when SCTP buffers grew above 2 MB. In
//! Arch IV the worker can't observe that signal directly. PR 4 cut 2
//! emits chunks unconditionally and relies on (a) typical files
//! being under tens of MB, (b) `tokio::task::yield_now()` after
//! every 100 chunks to let the IPC writer task drain. Restoring
//! daemon-side buffered-amount throttling (via a back-pressure IPC)
//! is a follow-up if large-file throughput regressions appear.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use desk_ipc_protocol::message::{
    FileTransferPayload, StartMediaPayload, StopMediaPayload, WorkerToService,
};
use log::{debug, error, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex as TokioMutex, mpsc};

use crate::host_control::HostControlHub;
use crate::model::file_transfer::*;
use crate::model::security_approval::{SecurityPermissionType, check_security_permission};
use crate::model::settings::SharedSettings;

const FILE_TRANSFER_CHUNK_SIZE_TX: usize = 60 * 1024;
const YIELD_EVERY_N_CHUNKS: u32 = 100;

/// Per-transfer in-flight upload state (browser uploading to host).
struct UploadState {
    file: tokio::fs::File,
    file_path: PathBuf,
    total_chunks: u64,
    received_chunks: u64,
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
    error_tx: mpsc::UnboundedSender<WorkerToService>,
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
        error_tx: mpsc::UnboundedSender<WorkerToService>,
        settings: Arc<SharedSettings>,
        hub: Arc<HostControlHub>,
    ) -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(DispatcherInner::new())),
            error_tx,
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

    /// Add a connection to the active set. Subsequent
    /// `FileTransferCommand` IPC for this `connection_id` will be
    /// processed; commands for inactive connections are dropped.
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
        let connection_id = payload.connection_id.clone();
        let mut inner = self.inner.lock().await;
        let complete = match inner.upload_states.get_mut(&transfer_id) {
            Some(state) => {
                if let Err(e) = state.file.write_all(&chunk_data).await {
                    error!(
                        "[FileTransferDispatcher] {}: write chunk {} for {} failed: {e}",
                        connection_id, chunk_index, transfer_id
                    );
                    return;
                }
                state.received_chunks += 1;
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
            drop(state);
            // Drop the lock before emitting IPC.
            drop(inner);
            self.emit_text(
                &connection_id,
                FileTransferMessage::TransferComplete(TransferComplete {
                    transfer_id: transfer_id.clone(),
                }),
            );
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
            self.emit_text(
                &connection_id,
                FileTransferMessage::TransferError(TransferError {
                    transfer_id: req.transfer_id.clone(),
                    message: format!("Target directory not found: {}", req.target_dir),
                }),
            );
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
        };
        {
            let mut inner = self.inner.lock().await;
            inner.upload_states.insert(req.transfer_id.clone(), state);
        }
        self.emit_text(
            &connection_id,
            FileTransferMessage::UploadResponse(UploadResponse {
                transfer_id: req.transfer_id,
                accepted: true,
                message: None,
            }),
        );
        Ok(())
    }

    async fn serve_download(
        &self,
        connection_id: String,
        req: DownloadRequest,
    ) -> std::io::Result<()> {
        let path = PathBuf::from(&req.file_path);
        if !path.exists() || !path.is_file() {
            self.emit_text(
                &connection_id,
                FileTransferMessage::TransferError(TransferError {
                    transfer_id: req.transfer_id.clone(),
                    message: format!("File not found: {}", req.file_path),
                }),
            );
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

        self.emit_text(
            &connection_id,
            FileTransferMessage::DownloadResponse(DownloadResponse {
                transfer_id: req.transfer_id.clone(),
                file_name: file_name.clone(),
                file_size,
                chunk_size,
                total_chunks,
            }),
        );

        let mut file = tokio::fs::File::open(&path).await?;
        let mut buf = vec![0u8; chunk_size];
        let mut chunk_index: u32 = 0;
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
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            let chunk_bytes = build_binary_chunk(&req.transfer_id, chunk_index, &buf[..n]);
            self.emit_binary(&connection_id, chunk_bytes);
            chunk_index += 1;
            if chunk_index.is_multiple_of(YIELD_EVERY_N_CHUNKS) {
                tokio::task::yield_now().await;
            }
        }
        self.emit_text(
            &connection_id,
            FileTransferMessage::TransferComplete(TransferComplete {
                transfer_id: req.transfer_id.clone(),
            }),
        );
        info!(
            "[FileTransferDispatcher] download {} completed: {} bytes, {} chunks",
            req.transfer_id, file_size, chunk_index
        );
        Ok(())
    }

    fn emit_text(&self, connection_id: &str, msg: FileTransferMessage) {
        let json = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                error!("[FileTransferDispatcher] serialize control message failed: {e}");
                return;
            }
        };
        self.emit_payload(connection_id, json, true);
    }

    fn emit_binary(&self, connection_id: &str, data: Vec<u8>) {
        self.emit_payload(connection_id, data, false);
    }

    fn emit_payload(&self, connection_id: &str, data: Vec<u8>, is_text: bool) {
        let payload = FileTransferPayload {
            connection_id: connection_id.to_string(),
            data,
            is_text,
        };
        if self
            .error_tx
            .send(WorkerToService::FileTransferData(payload))
            .is_err()
        {
            warn!(
                "[FileTransferDispatcher] failed to forward file transfer data for {} \
                 (IPC writer gone)",
                connection_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use desk_ipc_protocol::message::MediaCodec;
    use tempfile::TempDir;

    /// Build a dispatcher whose permission gate auto-passes
    /// (`allow_file_transfer = Some(true)`) so tests focus on the
    /// dispatch / IO logic rather than the Tauri approval prompt.
    /// Tests that need to assert the permission deny path can build a
    /// dispatcher with `allow_file_transfer = Some(false)` via
    /// `dispatcher_with_setting` instead.
    fn dispatcher() -> (
        FileTransferDispatcher,
        mpsc::UnboundedReceiver<WorkerToService>,
    ) {
        dispatcher_with_setting(Some(true))
    }

    fn dispatcher_with_setting(
        allow_file_transfer: Option<bool>,
    ) -> (
        FileTransferDispatcher,
        mpsc::UnboundedReceiver<WorkerToService>,
    ) {
        let mut settings = Settings::default();
        settings.security.allow_file_transfer = allow_file_transfer;
        let shared = Arc::new(SharedSettings::from(settings));
        let hub = Arc::new(HostControlHub::new_local());
        let (tx, rx) = mpsc::unbounded_channel();
        (FileTransferDispatcher::new(tx, shared, hub), rx)
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
        };
        d.handle_command(payload).await;
        // Yield + try_recv to ensure no spawned download task emitted.
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
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
        };
        d.handle_command(payload).await;
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "denied command must not produce IPC output"
        );
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
        let m = rx.recv().await.expect("download response");
        match m {
            WorkerToService::FileTransferData(p) => {
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
            }
            other => panic!("expected FileTransferData, got {other:?}"),
        }
        // Followed by ≥2 binary chunks then a TransferComplete text.
        let mut binary_count = 0;
        let mut saw_complete = false;
        while let Some(msg) = rx.recv().await {
            match msg {
                WorkerToService::FileTransferData(p) if !p.is_text => {
                    binary_count += 1;
                    let (tid, _idx, body) = parse_binary_chunk(&p.data).expect("chunk parse");
                    assert_eq!(tid, "00000000-0000-0000-0000-000000000001");
                    assert!(!body.is_empty());
                }
                WorkerToService::FileTransferData(p) if p.is_text => {
                    let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
                    if matches!(m, FileTransferMessage::TransferComplete(_)) {
                        saw_complete = true;
                        break;
                    }
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert!(binary_count >= 2, "expected ≥2 chunks, got {binary_count}");
        assert!(saw_complete, "expected TransferComplete");
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
        })
        .await;
        // Expect UploadResponse text first
        let resp = rx.recv().await.unwrap();
        match resp {
            WorkerToService::FileTransferData(p) => {
                assert!(p.is_text);
                let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
                assert!(matches!(m, FileTransferMessage::UploadResponse(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Send 2 chunks
        for i in 0..total_chunks as u32 {
            let chunk_bytes =
                build_binary_chunk(&transfer_id, i, &vec![b'A' + i as u8; chunk_size]);
            d.handle_command(FileTransferPayload {
                connection_id: "c1".into(),
                data: chunk_bytes,
                is_text: false,
            })
            .await;
        }
        // Expect TransferComplete text
        let complete = rx.recv().await.unwrap();
        match complete {
            WorkerToService::FileTransferData(p) => {
                assert!(p.is_text);
                let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
                assert!(
                    matches!(m, FileTransferMessage::TransferComplete(_)),
                    "expected TransferComplete, got {m:?}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
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
        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }
        assert_eq!(messages.len(), 1, "expected only DownloadResponse");
        match &messages[0] {
            WorkerToService::FileTransferData(p) => {
                assert!(p.is_text);
                let m: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
                assert!(matches!(m, FileTransferMessage::DownloadResponse(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }
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
        let m = rx.recv().await.unwrap();
        match m {
            WorkerToService::FileTransferData(p) => {
                assert!(p.is_text);
                let parsed: FileTransferMessage = serde_json::from_slice(&p.data).unwrap();
                assert!(matches!(parsed, FileTransferMessage::TransferError(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }
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
        })
        .await;
        assert!(rx.try_recv().is_err());
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
        })
        .await;
        assert!(rx.try_recv().is_err());
    }
}
