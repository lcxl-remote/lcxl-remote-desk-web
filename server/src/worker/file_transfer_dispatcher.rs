//! # Worker-side file-transfer dispatcher
//!
//! Mirrors the legacy bidirectional protocol from
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
//! user's permissions. The daemon (SYSTEM) is
//! deliberately a byte pump and never opens a file directly — that
//! would bypass per-user ACLs.
//!
//! ## Permission gating
//!
//! The daemon's DC router gates browser→worker forwarding on
//! `accept_control` (see `pc_manager::route_is_permitted`). On top of
//! that the worker re-checks the finer-grained
//! `resolve_permission(allow_file_transfer, FileTransfer)` gate
//! once per connection — see `permission_for` and the
//! `DispatcherInner::permission_cache` field. The cache mirrors the
//! legacy per-DC behaviour: each connection prompts at most once
//! (further commands hit the cache), the entry is dropped on
//! `stop_connection`, and the whole map clears on `shutdown`. The
//! settings-level "remember = allow" / "remember = deny" choice
//! still short-circuits the prompt entirely without user
//! interaction.
//!
//! ## Backpressure
//!
//! The legacy single-process handler watched `dc.buffered_amount()` to
//! throttle download chunk emission when SCTP buffers grew above 2 MB.
//! The split worker/daemon path re-establishes the same end-to-end
//! backpressure by routing all
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use desk_ipc_protocol::dual_transport::{EventSender, TransportError};
use desk_ipc_protocol::message::{
    FileTransferDirection, FileTransferFinishedPayload, FileTransferOutcome, FileTransferPayload,
    FileTransferSendErrorKind, FileTransferSendFailedPayload, FileTransferStartedPayload,
    StartMediaPayload, StopMediaPayload, WorkerToService,
};
use desk_utils::error::DeskErrorCode;
use log::{debug, error, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex as TokioMutex, mpsc};

use crate::host_control::HostControlHub;
use crate::model::file_transfer::*;
use crate::model::policy_access::{CachedDecision, PolicyAccess};
use crate::model::security_approval::{SecurityPermissionType, resolve_permission};
use crate::worker::connection_ceiling::ConnectionCeilingStore;

mod download;
mod lifecycle;
mod metrics;
mod upload;

use metrics::*;
pub(crate) use metrics::{
    FILE_TRANSFER_CHUNK_SIZE_TX, FT_METRICS_WINDOW_CHUNKS, duration_ns, throughput_mbps,
};

/// Where an upload's bytes go.
///
/// A trait rather than [`tokio::fs::File`] for two reasons. A test can put a
/// writer that fails, or blocks, in its place — a write or flush that fails
/// partway is what turns a filesystem problem into an answer the browser can act
/// on, and there is no portable way to make a real file do it on demand. And
/// reaching stable storage is part of the contract, not an extra: the file only
/// takes the destination's name once it is complete, and a rename that outlives
/// the bytes it renames would replace a good file with an empty one.
#[async_trait::async_trait]
trait UploadSink: tokio::io::AsyncWrite + Unpin + Send {
    async fn sync(&mut self) -> std::io::Result<()>;
}

#[async_trait::async_trait]
impl UploadSink for tokio::fs::File {
    async fn sync(&mut self) -> std::io::Result<()> {
        self.sync_all().await
    }
}

/// Discards everything. Installed in place of a real file once an upload is
/// finished with, so the handle is released at a point the code chooses rather
/// than whenever the last reference happens to go.
#[async_trait::async_trait]
impl UploadSink for tokio::io::Sink {
    async fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Per-transfer in-flight upload state (browser uploading to host).
struct UploadState {
    file: Box<dyn UploadSink>,
    total_chunks: u64,
    received_chunks: u64,
    expected_bytes: u64,
    received_bytes: u64,
    /// Per-transfer metrics window. Flushed every
    /// [`FT_METRICS_WINDOW_CHUNKS`] chunks and once more on completion
    /// so the final partial window does not get lost.
    metrics: UploadWindow,
}

/// One live upload, reachable from [`DispatcherInner::upload_states`] through a
/// handle small enough to take out from under the shared lock.
///
/// Writing a chunk means waiting on a disk, and how long that takes is not the
/// worker's to decide — a network share or a stalled device can hold a single
/// write for as long as it likes. The same shared lock is what a connection
/// ending, a revoked session, a cancel and a shutdown all have to take before
/// they can do anything, and those are the paths that withdraw a peer's access.
/// So the file and its counters live behind a lock of their own, and a write
/// holds nothing but the transfer it belongs to.
///
/// **Lock order**: the shared lock is never held while waiting for a state lock
/// — it is taken to look a transfer up and released before the state lock is
/// asked for. The reverse is how a writer takes itself out of the map on its way
/// out, and is safe precisely because of that rule.
struct Upload {
    /// Whether the transfer has been called off.
    ///
    /// Teardown records this and moves on; it does not wait for a write that is
    /// already in flight, which is the whole point. The writer reads it once it
    /// has the state lock and again after its write returns, and refuses to go
    /// any further either way.
    cancelled: AtomicBool,
    /// The file the browser asked for. Nothing is written here: it is created,
    /// in one step, out of [`Upload::staging`] once the whole upload has arrived
    /// and reached stable storage.
    ///
    /// Uploading over an existing file is allowed and is what the browser's file
    /// manager offers. Writing straight to this path is not, because then every
    /// way an upload can end badly — a cancel, a full disk, a peer that
    /// disconnects, a size that does not match — ends with the file the user
    /// already had truncated or deleted, in exchange for one that never
    /// finished arriving.
    destination: PathBuf,
    /// Where the bytes actually go while they arrive. Same directory as
    /// [`Upload::destination`], so the final step is a rename within one
    /// filesystem, and uniquely named, so two uploads never adopt each other's
    /// partial file.
    ///
    /// Kept out here rather than behind the state lock so teardown can name the
    /// file to remove without taking that lock — it is exactly what a slow write
    /// is holding.
    staging: PathBuf,
    state: TokioMutex<UploadState>,
}

impl Upload {
    /// Call the transfer off, so a write that is in flight stops at the end of
    /// the chunk it is on.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TransferKey {
    connection_id: String,
    transfer_id: String,
}

impl TransferKey {
    fn new(connection_id: &str, transfer_id: &str) -> Self {
        Self {
            connection_id: connection_id.to_string(),
            transfer_id: transfer_id.to_string(),
        }
    }
}

struct DispatcherInner {
    upload_states: HashMap<TransferKey, Arc<Upload>>,
    /// Destinations an upload in flight has claimed.
    ///
    /// Two transfers writing the same file would each stage their own copy and
    /// then rename over one another, so the file left behind is whichever
    /// finished last while both are told they succeeded. The second one to ask
    /// is refused instead. Keyed on the resolved destination, so the same file
    /// reached through two spellings of its directory is still one claim.
    upload_destinations: HashSet<PathBuf>,
    /// Downloads whose streaming loop is running and can therefore still act on
    /// a cancel. Registered by `serve_download` and released by it on the way
    /// out, whichever way it leaves.
    live_downloads: HashSet<TransferKey>,
    /// Cancels a running download loop has not picked up yet.
    ///
    /// Bounded by [`DispatcherInner::live_downloads`]: only a download that is
    /// streaming right now can be marked, and the loop clears whatever is left
    /// when it stops. Marking anything else would leave an entry with no reader
    /// and nothing to collect it — and a peer chooses the ids it cancels, so
    /// "anything else" is as much as it cares to send.
    cancelled_transfers: HashSet<TransferKey>,
    active_connections: HashSet<String>,
    /// Per-connection cached `allow_file_transfer` decision. Mirrors the
    /// per-DC permission cache from the legacy
    /// `service::file_transfer::handle_file_transfer_event` so each
    /// connection only triggers the Tauri approval prompt at most once,
    /// regardless of how many DownloadRequest / UploadRequest /
    /// chunk frames flow over its `file_transfer_event` DC.
    permission_cache: HashMap<String, CachedDecision>,
    activities: HashSet<TransferKey>,
}

impl DispatcherInner {
    fn new() -> Self {
        Self {
            upload_states: HashMap::new(),
            upload_destinations: HashSet::new(),
            live_downloads: HashSet::new(),
            cancelled_transfers: HashSet::new(),
            active_connections: HashSet::new(),
            permission_cache: HashMap::new(),
            activities: HashSet::new(),
        }
    }

    /// Ask a running download to stop.
    ///
    /// Only a download that is streaming right now reads this, so anything
    /// else — an upload, a transfer that already ended, an id that named
    /// nothing — is recorded nowhere rather than kept forever.
    fn cancel_download(&mut self, key: &TransferKey) {
        if self.live_downloads.contains(key) {
            self.cancelled_transfers.insert(key.clone());
        }
    }

    /// Take an upload out of reach and call it off, returning the staging file
    /// the caller should remove.
    ///
    /// What comes back is never the destination: an upload that ends this way
    /// never arrived, and the file the user already had is not the host's to
    /// delete.
    ///
    /// Marking is what lets the caller leave without waiting: a chunk write
    /// already under way holds only its own transfer's lock, and when it returns
    /// it reads the mark, closes the file and removes what it wrote. So the
    /// caller's own removal is allowed to fail — a platform that refuses to
    /// remove an open file still ends up with the staging file gone, and a
    /// connection ending never has to wait on a disk.
    fn take_upload(&mut self, key: &TransferKey) -> Option<PathBuf> {
        let upload = self.upload_states.remove(key)?;
        upload.cancel();
        self.upload_destinations.remove(&upload.destination);
        Some(upload.staging.clone())
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
    /// The host security policy as this worker reaches it: the daemon's
    /// published copy, and where a "remember" answer is sent to be stored.
    policy: Arc<PolicyAccess>,
    /// Host-control hub used by the permission gate to surface
    /// the approval prompt in the Tauri shell. In portable mode this is
    /// the daemon's hub directly (shared in-process); in named-pipe
    /// mode it's the worker's Forwarder hub that bridges back to the
    /// daemon's aggregator over ws.
    hub: Arc<HostControlHub>,
    /// Per-connection capability ceilings for redeemed-grant sessions. The
    /// permission gate meets the connection's ceiling with the global so a grant
    /// can only tighten file-transfer; owner connections carry no ceiling.
    connection_ceilings: ConnectionCeilingStore,
    activity_sender: mpsc::UnboundedSender<WorkerToService>,
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

/// The `transfer_id` an inbound command refers to, without acting on it.
///
/// The permission gate needs this to refuse a command with a `TransferError`
/// the browser can match against its pending transfer. It parses and nothing
/// else — no handler runs, so a refused command never opens a file, allocates
/// transfer state, or writes a byte.
///
/// `None` means the frame is unattributable: not valid UTF-8, not a known
/// message, or a binary chunk whose header is truncated. Such a frame cannot be
/// answered on its own (see `reject_unattributable_frame`).
fn inbound_transfer_id(payload: &FileTransferPayload) -> Option<String> {
    if payload.is_text {
        let text = std::str::from_utf8(&payload.data).ok()?;
        let message: FileTransferMessage = serde_json::from_str(text).ok()?;
        transfer_id_of(&message).map(str::to_owned)
    } else {
        parse_binary_chunk(&payload.data).map(|(transfer_id, _, _)| transfer_id.to_string())
    }
}

fn sanitized_file_name(value: &str) -> String {
    let basename = value.rsplit(['/', '\\']).next().unwrap_or(value);
    let cleaned: String = basename
        .chars()
        .filter(|c| !c.is_control())
        .take(255)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests;
