//! # Worker-side clipboard dispatcher
//!
//! Per-worker, multi-`connection_id` clipboard handler. Mirrors the
//! legacy bidirectional logic in
//! `service::clipboard_event::handle_clipboard_event` but split across
//! the IPC boundary:
//!
//! - **Browser → host (write)**: daemon's `on_data_channel` router
//!   forwards `clipboard_event` DC bytes as
//!   `ServiceToWorker::ClipboardWrite`. The dispatcher decodes the
//!   `ClipboardEventData` JSON and reassembles chunked images, then
//!   pushes onto the local clipboard via
//!   `desk_input_injection::HostControlHelper::set_*_to_clipboard`.
//!
//! - **Host → browser (read)**: a single worker-wide polling task
//!   reads the local clipboard at 500 ms cadence and, on hash change,
//!   emits `WorkerToService::ClipboardRead` for *every active*
//!   connection. The daemon (`pc_manager::write_clipboard_data`) gates
//!   on per-connection `accept_clipboard_sync` and writes the JSON
//!   bytes unchanged to that connection's clipboard DC.
//!
//! The polling task starts when `start_connection` is called for the
//! first connection and stops (via `JoinHandle::abort`) when
//! `stop_connection` removes the last one. This keeps the clipboard
//! API quiet while no browser is attached.
//!
//! ## Echo-loop prevention
//!
//! When the dispatcher writes incoming clipboard data to the local
//! clipboard, it also stamps `last_written_hash` with the hash of what
//! it just wrote. The polling task skips emissions whose hash matches
//! that stamp, so a browser-supplied paste does not bounce back as a
//! "host clipboard changed" event. Same logic the legacy handler used.
//!
//! ## Permission gating
//!
//! The dispatcher does *not* know per-connection `accept_clipboard_sync`
//! state — that lives on the daemon. Browser→host writes are gated by
//! the daemon's DC router (`route_is_permitted`); host→browser writes
//! are gated by `pc_manager::write_clipboard_data`. The worker emits
//! for every active connection unconditionally; the daemon drops what
//! the user has not authorised.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::prelude::*;
use desk_input_injection::host_control::host_control_factory::create_host_control_helper;
use desk_input_injection::model::host_control::{ClipboardImage, HostControlHelper};
use desk_ipc_protocol::message::{
    ClipboardPayload, ConnectionRefPayload, StartMediaPayload, StopMediaPayload, WorkerToService,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use log::{debug, error, info, trace, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as TokioMutex, mpsc};
use tokio::task::JoinHandle;

/// Polling cadence for the host-clipboard read loop. Matches the
/// legacy `service::clipboard_event` value so user-perceptible
/// latency is identical.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 1 MB cap for clipboard text.
const MAX_TEXT_SIZE: usize = 1024 * 1024;

/// 25 MB cap for clipboard image (PNG-encoded).
const MAX_IMAGE_SIZE: usize = 25 * 1024 * 1024;

/// 32 KB chunk size for image transfer. Sized for safe SCTP DataChannel
/// transmit.
const IMAGE_CHUNK_SIZE: usize = 32 * 1024;

/// Wire-format JSON ferried over the `clipboard_event` DC and the
/// `ClipboardWrite`/`ClipboardRead` IPC variants. Identical shape to
/// `crate::model::data_channel::ClipboardEventData` so the browser is
/// unaware of the daemon/worker split.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardEventData {
    r#type: String,
    content: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    total_bytes: Option<u64>,
    chunk_count: Option<u32>,
    index: Option<u32>,
}

/// Reassembly state for an in-flight image upload from a browser.
#[derive(Default)]
struct ImageTransferState {
    total_chunks: u32,
    chunks_received: u32,
    data: Vec<u8>,
}

struct ClipboardInner {
    helper: Box<dyn HostControlHelper + Send + Sync>,
    /// Hash of the last clipboard contents we *wrote* to the local
    /// system from an incoming browser message. The polling task uses
    /// this to suppress the bounce-back: when it next sees the local
    /// clipboard, the hash will match and it will skip the emission.
    last_written_hash: Option<u64>,
    /// Hash of the last clipboard contents the polling task pushed
    /// upstream. Suppresses re-emission when nothing has changed.
    last_pushed_hash: Option<u64>,
    /// Per-connection image-reassembly state for incoming
    /// `image_start`/`image_chunk`/`image_end` sequences.
    image_states: HashMap<String, ImageTransferState>,
    /// Connections currently subscribed to clipboard sync (those that
    /// have received a `StartMedia` and not yet a `StopMedia`).
    active_connections: HashSet<String>,
    /// JoinHandle of the polling task, when running.
    poll_handle: Option<JoinHandle<()>>,
}

impl ClipboardInner {
    fn new(helper: Box<dyn HostControlHelper + Send + Sync>) -> Self {
        Self {
            helper,
            last_written_hash: None,
            last_pushed_hash: None,
            image_states: HashMap::new(),
            active_connections: HashSet::new(),
            poll_handle: None,
        }
    }
}

/// Worker-side clipboard dispatcher. Cheap to clone (`Arc` inside) so
/// the IPC loop can take a clone for each call site.
#[derive(Clone)]
pub struct ClipboardDispatcher {
    inner: Arc<TokioMutex<ClipboardInner>>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
}

impl ClipboardDispatcher {
    /// Construct a dispatcher backed by a per-platform host-control
    /// helper. Returns `Err` only when the helper cannot be built
    /// (e.g. Linux without X11/Wayland clipboard backend); on that
    /// failure the worker falls back to running without a clipboard
    /// dispatcher and clipboard IPC variants log-and-drop.
    pub fn new(
        desk_settings: &DeskSettings,
        error_tx: mpsc::UnboundedSender<WorkerToService>,
    ) -> Result<Self, String> {
        let helper = create_host_control_helper(desk_settings, None).map_err(|e| {
            format!(
                "ClipboardDispatcher: create_host_control_helper failed: {e}; \
                 clipboard sync will be unavailable for this worker"
            )
        })?;
        Ok(Self {
            inner: Arc::new(TokioMutex::new(ClipboardInner::new(helper))),
            error_tx,
        })
    }

    /// Add a connection to the active set. Spawns the polling task on
    /// the first connection so the local clipboard is not polled when
    /// no browser is attached.
    pub async fn start_connection(&self, payload: &StartMediaPayload) {
        let mut inner = self.inner.lock().await;
        let was_empty = inner.active_connections.is_empty();
        let inserted = inner
            .active_connections
            .insert(payload.connection_id.clone());
        if !inserted {
            debug!(
                "[ClipboardDispatcher] {}: duplicate StartMedia (already active)",
                payload.connection_id
            );
        }
        if was_empty && inner.poll_handle.is_none() {
            let inner_clone = Arc::clone(&self.inner);
            let tx_clone = self.error_tx.clone();
            inner.poll_handle = Some(tokio::spawn(async move {
                run_poll_loop(inner_clone, tx_clone).await;
            }));
            info!("[ClipboardDispatcher] poll loop started");
        }
        info!(
            "[ClipboardDispatcher] {}: subscribed (active_count={})",
            payload.connection_id,
            inner.active_connections.len()
        );
    }

    /// Remove a connection from the active set. Aborts the polling
    /// task when the last connection leaves.
    pub async fn stop_connection(&self, payload: &StopMediaPayload) {
        let mut inner = self.inner.lock().await;
        let removed = inner.active_connections.remove(&payload.connection_id);
        // Drop any in-flight image reassembly state for this connection
        // so a half-uploaded image cannot leak into a subsequent
        // connection that happens to reuse the same id.
        inner.image_states.remove(&payload.connection_id);
        if removed {
            info!(
                "[ClipboardDispatcher] {}: unsubscribed (active_count={})",
                payload.connection_id,
                inner.active_connections.len()
            );
        }
        if inner.active_connections.is_empty()
            && let Some(handle) = inner.poll_handle.take()
        {
            handle.abort();
            info!("[ClipboardDispatcher] poll loop stopped (no active connections)");
        }
    }

    /// Drop every connection and stop the polling task. Called from
    /// worker shutdown so the polling task does not outlive the worker.
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        inner.active_connections.clear();
        inner.image_states.clear();
        if let Some(handle) = inner.poll_handle.take() {
            handle.abort();
        }
    }

    /// Apply a browser→host clipboard write. The bytes carry a
    /// `ClipboardEventData` JSON; image transfers arrive as a
    /// `image_start` / N×`image_chunk` / `image_end` sequence keyed
    /// by `connection_id` so concurrent browsers do not mix chunks.
    pub async fn handle_clipboard_write(&self, payload: ClipboardPayload) {
        let event = match decode_event(&payload.data) {
            Some(e) => e,
            None => return,
        };
        let mut inner = self.inner.lock().await;
        match event.r#type.as_str() {
            "text" => apply_text(&mut inner, &payload.connection_id, event.content),
            "image_start" => apply_image_start(&mut inner, &payload.connection_id, &event),
            "image_chunk" => apply_image_chunk(&mut inner, &payload.connection_id, event.content),
            "image_end" => apply_image_end(&mut inner, &payload.connection_id),
            other => {
                debug!(
                    "[ClipboardDispatcher] {}: ignoring clipboard event type '{}'",
                    payload.connection_id, other
                );
            }
        }
    }

    /// Force a one-shot re-emission of the current host-clipboard
    /// contents to the requesting connection. Used when a browser sends
    /// `ClipboardRequest` (typically after the DC opens, before the
    /// next polling-tick natural change). Bypasses the
    /// `last_pushed_hash` dedup so the requester sees the current
    /// state even if no change has occurred.
    pub async fn handle_clipboard_request(&self, payload: ConnectionRefPayload) {
        let mut inner = self.inner.lock().await;
        if !inner.active_connections.contains(&payload.connection_id) {
            debug!(
                "[ClipboardDispatcher] {}: ClipboardRequest for inactive connection — dropping",
                payload.connection_id
            );
            return;
        }
        if let Some(messages) = read_local_clipboard(&mut inner) {
            for msg in messages {
                emit_to(&self.error_tx, &payload.connection_id, msg);
            }
        }
    }
}

/// Background loop owned by the dispatcher. Polls the host clipboard
/// every `POLL_INTERVAL`; on a hash change broadcasts the new contents
/// to every active connection. Exits cleanly when `JoinHandle::abort`
/// is called on it (i.e. the last connection unsubscribed or the
/// dispatcher is shutting down).
async fn run_poll_loop(
    inner: Arc<TokioMutex<ClipboardInner>>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let mut guard = inner.lock().await;
        if guard.active_connections.is_empty() {
            // Defensive: the stop_connection path aborts the handle,
            // but a stray loop iteration during shutdown should still
            // exit gracefully rather than spam empty broadcasts.
            continue;
        }
        let messages = match read_local_clipboard(&mut guard) {
            Some(msgs) => msgs,
            None => continue,
        };
        // Snapshot active connections so we drop the lock before
        // sending. The error_tx is unbounded so `send` is non-blocking
        // anyway, but holding the lock across a `try_send` for every
        // connection magnifies contention with handle_clipboard_write.
        let recipients: Vec<String> = guard.active_connections.iter().cloned().collect();
        drop(guard);
        for connection_id in &recipients {
            for msg in &messages {
                emit_to(&error_tx, connection_id, msg.clone());
            }
        }
    }
}

/// Read the local clipboard once; if it differs from the last pushed
/// hash *and* from the last written hash (echo guard), build the JSON
/// message(s) we should fan out. Returns `None` when nothing changed.
fn read_local_clipboard(inner: &mut ClipboardInner) -> Option<Vec<Vec<u8>>> {
    // Text first. If text is present and
    // unchanged, image is not consulted (avoids racing the same
    // shell put-text-then-put-image flow).
    match inner.helper.get_text_from_clipboard() {
        Ok(Some(text)) => {
            let hash = hash_text(&text);
            if Some(hash) == inner.last_written_hash {
                return None;
            }
            if Some(hash) == inner.last_pushed_hash {
                return None;
            }
            if text.len() > MAX_TEXT_SIZE {
                warn!("[ClipboardDispatcher] local clipboard text too large to sync (>1MB)");
                inner.last_pushed_hash = Some(hash);
                return Some(vec![encode_error("Text too large to sync (>1MB)")]);
            }
            inner.last_pushed_hash = Some(hash);
            Some(vec![encode_text(&text)])
        }
        Ok(None) => {
            // No text → check image
            match inner.helper.get_image_from_clipboard() {
                Ok(Some(image)) => {
                    let hash = hash_image(&image);
                    if Some(hash) == inner.last_written_hash {
                        return None;
                    }
                    if Some(hash) == inner.last_pushed_hash {
                        return None;
                    }
                    let png_data = match encode_png(&image) {
                        Some(b) => b,
                        None => {
                            warn!("[ClipboardDispatcher] PNG encode failed for clipboard image");
                            return None;
                        }
                    };
                    if png_data.len() > MAX_IMAGE_SIZE {
                        warn!(
                            "[ClipboardDispatcher] local clipboard image too large to sync (>25MB)"
                        );
                        inner.last_pushed_hash = Some(hash);
                        return Some(vec![encode_error("Image too large to sync (>25MB)")]);
                    }
                    inner.last_pushed_hash = Some(hash);
                    Some(encode_image_messages(&png_data, image.width, image.height))
                }
                Ok(None) => None,
                Err(e) => {
                    trace!("[ClipboardDispatcher] image clipboard read failed: {e}");
                    None
                }
            }
        }
        Err(e) => {
            trace!("[ClipboardDispatcher] text clipboard read failed: {e}");
            None
        }
    }
}

/// Push a single message body onto the error_tx channel as a
/// `WorkerToService::ClipboardRead`. Failures (channel closed) are
/// logged at warn level — the dispatcher cannot recover from a dead
/// IPC writer task and the worker will exit shortly anyway.
fn emit_to(tx: &mpsc::UnboundedSender<WorkerToService>, connection_id: &str, data: Vec<u8>) {
    let payload = ClipboardPayload {
        connection_id: connection_id.to_string(),
        data,
    };
    if tx.send(WorkerToService::ClipboardRead(payload)).is_err() {
        warn!(
            "[ClipboardDispatcher] failed to forward clipboard read for {} (IPC writer gone)",
            connection_id
        );
    }
}

fn decode_event(data: &[u8]) -> Option<ClipboardEventData> {
    let s = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(e) => {
            error!("[ClipboardDispatcher] clipboard payload not UTF-8: {e}");
            return None;
        }
    };
    match serde_json::from_str::<ClipboardEventData>(s) {
        Ok(e) => Some(e),
        Err(e) => {
            error!("[ClipboardDispatcher] clipboard JSON decode failed: {e}");
            None
        }
    }
}

fn apply_text(inner: &mut ClipboardInner, connection_id: &str, content: Option<String>) {
    let text = match content {
        Some(t) => t,
        None => {
            debug!("[ClipboardDispatcher] {connection_id}: text event with no content; dropping");
            return;
        }
    };
    if text.len() > MAX_TEXT_SIZE {
        warn!("[ClipboardDispatcher] {connection_id}: incoming text exceeds 1MB; dropping");
        return;
    }
    if let Err(e) = inner.helper.set_text_to_clipboard(&text) {
        error!("[ClipboardDispatcher] {connection_id}: set_text_to_clipboard failed: {e}");
        return;
    }
    inner.last_written_hash = Some(hash_text(&text));
    debug!(
        "[ClipboardDispatcher] {connection_id}: applied {} bytes of text to local clipboard",
        text.len()
    );
}

fn apply_image_start(inner: &mut ClipboardInner, connection_id: &str, event: &ClipboardEventData) {
    let total = event.total_bytes.unwrap_or(0);
    if total > MAX_IMAGE_SIZE as u64 {
        warn!(
            "[ClipboardDispatcher] {connection_id}: incoming image exceeds 25MB ({} bytes); rejecting",
            total
        );
        inner.image_states.remove(connection_id);
        return;
    }
    let total_chunks = event.chunk_count.unwrap_or(0);
    inner.image_states.insert(
        connection_id.to_string(),
        ImageTransferState {
            total_chunks,
            chunks_received: 0,
            data: Vec::with_capacity(total as usize),
        },
    );
    debug!(
        "[ClipboardDispatcher] {connection_id}: image_start total_chunks={total_chunks} total_bytes={total}"
    );
}

fn apply_image_chunk(inner: &mut ClipboardInner, connection_id: &str, content: Option<String>) {
    let chunk = match content {
        Some(c) => c,
        None => return,
    };
    if let Some(state) = inner.image_states.get_mut(connection_id) {
        state.data.extend_from_slice(chunk.as_bytes());
        state.chunks_received += 1;
    } else {
        debug!("[ClipboardDispatcher] {connection_id}: image_chunk before image_start; dropping");
    }
}

fn apply_image_end(inner: &mut ClipboardInner, connection_id: &str) {
    let state = match inner.image_states.remove(connection_id) {
        Some(s) => s,
        None => {
            debug!("[ClipboardDispatcher] {connection_id}: image_end without start; dropping");
            return;
        }
    };
    if state.chunks_received != state.total_chunks {
        warn!(
            "[ClipboardDispatcher] {connection_id}: image transfer incomplete \
             ({}/{} chunks); dropping",
            state.chunks_received, state.total_chunks
        );
        return;
    }
    let png_bytes = match BASE64_STANDARD.decode(&state.data) {
        Ok(b) => b,
        Err(e) => {
            error!("[ClipboardDispatcher] {connection_id}: base64 decode failed: {e}");
            return;
        }
    };
    let cursor = std::io::Cursor::new(png_bytes);
    let decoder = png::Decoder::new(cursor);
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(e) => {
            error!("[ClipboardDispatcher] {connection_id}: PNG header decode failed: {e}");
            return;
        }
    };
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = match reader.next_frame(&mut buf) {
        Ok(i) => i,
        Err(e) => {
            error!("[ClipboardDispatcher] {connection_id}: PNG frame decode failed: {e}");
            return;
        }
    };
    let img = ClipboardImage {
        width: info.width as usize,
        height: info.height as usize,
        bytes: std::borrow::Cow::Owned(buf[..info.buffer_size()].to_vec()),
    };
    if let Err(e) = inner.helper.set_image_to_clipboard(&img) {
        error!("[ClipboardDispatcher] {connection_id}: set_image_to_clipboard failed: {e}");
        return;
    }
    inner.last_written_hash = Some(hash_image(&img));
    debug!(
        "[ClipboardDispatcher] {connection_id}: applied {}×{} image to local clipboard",
        img.width, img.height
    );
}

fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn hash_image(image: &ClipboardImage) -> u64 {
    let mut h = DefaultHasher::new();
    image.bytes.hash(&mut h);
    h.write_usize(image.width);
    h.write_usize(image.height);
    h.finish()
}

fn encode_text(text: &str) -> Vec<u8> {
    let event = ClipboardEventData {
        r#type: "text".to_string(),
        content: Some(text.to_string()),
        width: None,
        height: None,
        total_bytes: None,
        chunk_count: None,
        index: None,
    };
    serde_json::to_vec(&event).expect("ClipboardEventData (text) JSON serialise must succeed")
}

fn encode_error(msg: &str) -> Vec<u8> {
    let event = ClipboardEventData {
        r#type: "error".to_string(),
        content: Some(msg.to_string()),
        width: None,
        height: None,
        total_bytes: None,
        chunk_count: None,
        index: None,
    };
    serde_json::to_vec(&event).expect("ClipboardEventData (error) JSON serialise must succeed")
}

fn encode_png(image: &ClipboardImage) -> Option<Vec<u8>> {
    let mut png_data = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_data, image.width as u32, image.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&image.bytes).ok()?;
    }
    Some(png_data)
}

fn encode_image_messages(png_data: &[u8], width: usize, height: usize) -> Vec<Vec<u8>> {
    let base64_str = BASE64_STANDARD.encode(png_data);
    let mut chunks: Vec<&str> = Vec::new();
    let bytes = base64_str.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let end = std::cmp::min(offset + IMAGE_CHUNK_SIZE, bytes.len());
        // Safe because base64 is pure ASCII and chunk boundaries fall
        // on byte boundaries (no multi-byte UTF-8 split).
        let s =
            std::str::from_utf8(&bytes[offset..end]).expect("base64 output is always valid UTF-8");
        chunks.push(s);
        offset = end;
    }
    let chunk_count = chunks.len() as u32;
    let mut out = Vec::with_capacity(chunks.len() + 2);
    let start = ClipboardEventData {
        r#type: "image_start".to_string(),
        content: None,
        width: Some(width as u32),
        height: Some(height as u32),
        total_bytes: Some(png_data.len() as u64),
        chunk_count: Some(chunk_count),
        index: None,
    };
    out.push(serde_json::to_vec(&start).expect("ClipboardEventData (image_start) serialise"));
    for (i, chunk) in chunks.into_iter().enumerate() {
        let msg = ClipboardEventData {
            r#type: "image_chunk".to_string(),
            content: Some(chunk.to_string()),
            width: None,
            height: None,
            total_bytes: None,
            chunk_count: None,
            index: Some(i as u32),
        };
        out.push(serde_json::to_vec(&msg).expect("ClipboardEventData (image_chunk) serialise"));
    }
    let end = ClipboardEventData {
        r#type: "image_end".to_string(),
        content: None,
        width: None,
        height: None,
        total_bytes: None,
        chunk_count: None,
        index: None,
    };
    out.push(serde_json::to_vec(&end).expect("ClipboardEventData (image_end) serialise"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_input_injection::error::InputError;
    use desk_input_injection::model::host_control::DisplaySettings;
    use std::sync::Mutex as StdMutex;

    /// In-memory mock helper: lets tests inject clipboard reads and
    /// observe writes without touching the real OS clipboard. Only
    /// the methods clipboard_dispatcher actually exercises are
    /// implemented; the rest panic so a misuse is loud rather than
    /// silently passing.
    struct MockHelper {
        text: Arc<StdMutex<Option<String>>>,
        image: Arc<StdMutex<Option<ClipboardImage>>>,
        writes: Arc<StdMutex<Vec<MockWrite>>>,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum MockWrite {
        Text(String),
        Image { width: usize, height: usize },
    }

    impl MockHelper {
        fn new() -> Self {
            Self {
                text: Arc::new(StdMutex::new(None)),
                image: Arc::new(StdMutex::new(None)),
                writes: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn handles(&self) -> MockHandles {
            MockHandles {
                text: Arc::clone(&self.text),
                image: Arc::clone(&self.image),
            }
        }
    }

    struct MockHandles {
        text: Arc<StdMutex<Option<String>>>,
        image: Arc<StdMutex<Option<ClipboardImage>>>,
    }

    impl HostControlHelper for MockHelper {
        fn change_display_settings(&self, _: &DisplaySettings) -> Result<(), InputError> {
            unimplemented!()
        }
        fn block_input(&self, _: bool) -> Result<(), InputError> {
            unimplemented!()
        }
        fn enable_private_screen(&self, _: &str, _: bool) -> Result<(), InputError> {
            unimplemented!()
        }
        fn control_monitor_power(&self, _: bool) -> Result<(), InputError> {
            unimplemented!()
        }
        fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), InputError> {
            *self.text.lock().unwrap() = Some(text.to_string());
            self.writes
                .lock()
                .unwrap()
                .push(MockWrite::Text(text.to_string()));
            Ok(())
        }
        fn get_text_from_clipboard(&mut self) -> Result<Option<String>, InputError> {
            Ok(self.text.lock().unwrap().clone())
        }
        fn get_image_from_clipboard(&mut self) -> Result<Option<ClipboardImage>, InputError> {
            Ok(self.image.lock().unwrap().clone())
        }
        fn set_image_to_clipboard(&mut self, image: &ClipboardImage) -> Result<(), InputError> {
            *self.image.lock().unwrap() = Some(image.clone());
            self.writes.lock().unwrap().push(MockWrite::Image {
                width: image.width,
                height: image.height,
            });
            Ok(())
        }
    }

    fn dispatcher_with_mock() -> (
        ClipboardDispatcher,
        MockHandles,
        mpsc::UnboundedReceiver<WorkerToService>,
    ) {
        let mock = MockHelper::new();
        let handles = mock.handles();
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(TokioMutex::new(ClipboardInner::new(Box::new(mock))));
        let d = ClipboardDispatcher {
            inner,
            error_tx: tx,
        };
        (d, handles, rx)
    }

    fn start_payload(connection_id: &str) -> StartMediaPayload {
        StartMediaPayload {
            connection_id: connection_id.to_string(),
            video_codec: desk_ipc_protocol::message::MediaCodec::H264,
            audio_codec: desk_ipc_protocol::message::MediaCodec::Opus,
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

    /// Decoder rejects malformed JSON without panicking.
    #[test]
    fn decode_event_rejects_invalid_json() {
        assert!(decode_event(b"not json").is_none());
    }

    /// Decoder rejects non-UTF8 bytes without panicking.
    #[test]
    fn decode_event_rejects_invalid_utf8() {
        assert!(decode_event(&[0xFFu8, 0xFE]).is_none());
    }

    /// Incoming text payload writes through to the helper and stamps
    /// `last_written_hash` so the polling loop will not re-emit the
    /// same value.
    #[tokio::test]
    async fn handle_clipboard_write_text_sets_local_clipboard() {
        let (d, handles, _rx) = dispatcher_with_mock();
        let event = ClipboardEventData {
            r#type: "text".to_string(),
            content: Some("hello".to_string()),
            width: None,
            height: None,
            total_bytes: None,
            chunk_count: None,
            index: None,
        };
        let bytes = serde_json::to_vec(&event).unwrap();
        d.handle_clipboard_write(ClipboardPayload {
            connection_id: "c1".to_string(),
            data: bytes,
        })
        .await;
        let written = handles.text.lock().unwrap().clone();
        assert_eq!(written.as_deref(), Some("hello"));
        // Echo guard: last_written_hash should match the text hash.
        let guard = d.inner.lock().await;
        assert_eq!(guard.last_written_hash, Some(hash_text("hello")));
    }

    /// Image upload sequence (start + 2 chunks + end) writes a real
    /// PNG-decoded image into the helper.
    #[tokio::test]
    async fn handle_clipboard_write_image_sequence_assembles_and_writes() {
        let (d, handles, _rx) = dispatcher_with_mock();
        // Build a 2x2 RGBA image, encode to PNG, base64, then split.
        let raw_image = ClipboardImage {
            width: 2,
            height: 2,
            bytes: std::borrow::Cow::Owned(vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ]),
        };
        let png = encode_png(&raw_image).expect("png encode");
        let b64 = BASE64_STANDARD.encode(&png);
        let half = b64.len() / 2;
        let (chunk_a, chunk_b) = b64.split_at(half);
        let start = ClipboardEventData {
            r#type: "image_start".into(),
            content: None,
            width: Some(2),
            height: Some(2),
            total_bytes: Some(png.len() as u64),
            chunk_count: Some(2),
            index: None,
        };
        let chunks = [chunk_a.to_string(), chunk_b.to_string()];
        let end = ClipboardEventData {
            r#type: "image_end".into(),
            content: None,
            width: None,
            height: None,
            total_bytes: None,
            chunk_count: None,
            index: None,
        };
        let cid = "c1";
        d.handle_clipboard_write(ClipboardPayload {
            connection_id: cid.into(),
            data: serde_json::to_vec(&start).unwrap(),
        })
        .await;
        for (i, chunk) in chunks.iter().enumerate() {
            let msg = ClipboardEventData {
                r#type: "image_chunk".into(),
                content: Some(chunk.clone()),
                width: None,
                height: None,
                total_bytes: None,
                chunk_count: None,
                index: Some(i as u32),
            };
            d.handle_clipboard_write(ClipboardPayload {
                connection_id: cid.into(),
                data: serde_json::to_vec(&msg).unwrap(),
            })
            .await;
        }
        d.handle_clipboard_write(ClipboardPayload {
            connection_id: cid.into(),
            data: serde_json::to_vec(&end).unwrap(),
        })
        .await;
        let written_image = handles
            .image
            .lock()
            .unwrap()
            .clone()
            .expect("image written");
        assert_eq!(written_image.width, 2);
        assert_eq!(written_image.height, 2);
        // last_written_hash stamped with the decoded image's hash.
        let guard = d.inner.lock().await;
        assert_eq!(guard.last_written_hash, Some(hash_image(&written_image)));
    }

    /// `start_connection` then `stop_connection` adds and removes the
    /// id from the active set and stops the polling task.
    #[tokio::test]
    async fn start_then_stop_releases_state() {
        let (d, _handles, _rx) = dispatcher_with_mock();
        d.start_connection(&start_payload("c1")).await;
        {
            let g = d.inner.lock().await;
            assert!(g.active_connections.contains("c1"));
            assert!(g.poll_handle.is_some(), "poll task must be spawned");
        }
        d.stop_connection(&StopMediaPayload {
            connection_id: "c1".into(),
        })
        .await;
        let g = d.inner.lock().await;
        assert!(!g.active_connections.contains("c1"));
        assert!(
            g.poll_handle.is_none(),
            "poll task must be aborted when no active connections"
        );
    }

    /// `shutdown` clears state and aborts the polling task.
    #[tokio::test]
    async fn shutdown_clears_state() {
        let (d, _handles, _rx) = dispatcher_with_mock();
        d.start_connection(&start_payload("c1")).await;
        d.start_connection(&start_payload("c2")).await;
        d.shutdown().await;
        let g = d.inner.lock().await;
        assert!(g.active_connections.is_empty());
        assert!(g.poll_handle.is_none());
    }

    /// Polling task emits a ClipboardRead per active connection when
    /// the host clipboard contains text and no echo dedup applies.
    #[tokio::test]
    async fn poll_loop_emits_per_active_connection_on_change() {
        let (d, handles, mut rx) = dispatcher_with_mock();
        // Seed the mock clipboard with text BEFORE the poll task runs.
        *handles.text.lock().unwrap() = Some("greeting".to_string());
        d.start_connection(&start_payload("c1")).await;
        d.start_connection(&start_payload("c2")).await;
        // Wait up to 2 polling intervals for both emissions.
        let mut received_for_c1 = false;
        let mut received_for_c2 = false;
        let deadline = tokio::time::Instant::now() + POLL_INTERVAL * 4;
        while tokio::time::Instant::now() < deadline && !(received_for_c1 && received_for_c2) {
            match tokio::time::timeout(POLL_INTERVAL, rx.recv()).await {
                Ok(Some(WorkerToService::ClipboardRead(p))) => {
                    if p.connection_id == "c1" {
                        received_for_c1 = true;
                    }
                    if p.connection_id == "c2" {
                        received_for_c2 = true;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {}
            }
        }
        d.shutdown().await;
        assert!(received_for_c1, "c1 must receive ClipboardRead");
        assert!(received_for_c2, "c2 must receive ClipboardRead");
    }

    /// Poll task respects the echo guard: clipboard contents that
    /// match `last_written_hash` (because the dispatcher itself just
    /// wrote them) are not re-emitted.
    #[tokio::test]
    async fn poll_loop_skips_echoed_writes() {
        let (d, handles, mut rx) = dispatcher_with_mock();
        // Apply an incoming text — this sets last_written_hash and
        // also stores the text in the mock clipboard.
        let event = ClipboardEventData {
            r#type: "text".into(),
            content: Some("from-browser".into()),
            width: None,
            height: None,
            total_bytes: None,
            chunk_count: None,
            index: None,
        };
        d.handle_clipboard_write(ClipboardPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&event).unwrap(),
        })
        .await;
        assert_eq!(
            handles.text.lock().unwrap().as_deref(),
            Some("from-browser")
        );
        // Now subscribe and let the poller tick a few times. It must
        // NOT bounce the same text back as ClipboardRead.
        d.start_connection(&start_payload("c1")).await;
        let result = tokio::time::timeout(POLL_INTERVAL * 3, rx.recv()).await;
        d.shutdown().await;
        assert!(
            result.is_err(),
            "poll loop must suppress echoed write, got {result:?}"
        );
    }

    /// `handle_clipboard_request` for an unknown connection_id is a
    /// silent no-op (no panic, no IPC emission).
    #[tokio::test]
    async fn handle_clipboard_request_unknown_connection_is_silent() {
        let (d, _handles, mut rx) = dispatcher_with_mock();
        d.handle_clipboard_request(ConnectionRefPayload {
            connection_id: "ghost".into(),
        })
        .await;
        // Nothing should have been emitted.
        assert!(rx.try_recv().is_err());
    }

    /// `handle_clipboard_request` for an active connection emits the
    /// current clipboard contents bypassing the `last_pushed_hash`
    /// dedup, even if the polling loop has already pushed the same
    /// hash before. Verifies the request → IPC path independently of
    /// the polling cadence.
    #[tokio::test]
    async fn handle_clipboard_request_emits_current_text() {
        let (d, handles, mut rx) = dispatcher_with_mock();
        *handles.text.lock().unwrap() = Some("snapshot".to_string());
        d.start_connection(&start_payload("c1")).await;
        d.handle_clipboard_request(ConnectionRefPayload {
            connection_id: "c1".into(),
        })
        .await;
        // First message could be from the request handler OR from a
        // poll tick that fired between start and request — accept
        // either ordering, just require at least one emission for c1.
        let msg = tokio::time::timeout(POLL_INTERVAL * 4, rx.recv())
            .await
            .expect("must receive at least one ClipboardRead")
            .expect("channel closed unexpectedly");
        d.shutdown().await;
        match msg {
            WorkerToService::ClipboardRead(p) => {
                assert_eq!(p.connection_id, "c1");
                let event: ClipboardEventData = serde_json::from_slice(&p.data).unwrap();
                assert_eq!(event.r#type, "text");
                assert_eq!(event.content.as_deref(), Some("snapshot"));
            }
            other => panic!("expected ClipboardRead, got {other:?}"),
        }
    }

    /// Image chunk arriving without a preceding image_start is a
    /// silent drop (no panic).
    #[tokio::test]
    async fn image_chunk_without_start_drops_silently() {
        let (d, _handles, _rx) = dispatcher_with_mock();
        let event = ClipboardEventData {
            r#type: "image_chunk".into(),
            content: Some("AAAA".into()),
            width: None,
            height: None,
            total_bytes: None,
            chunk_count: None,
            index: Some(0),
        };
        d.handle_clipboard_write(ClipboardPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&event).unwrap(),
        })
        .await;
        let g = d.inner.lock().await;
        assert!(g.image_states.is_empty());
    }

    /// `image_end` arriving without `image_start` is a silent drop.
    #[tokio::test]
    async fn image_end_without_start_drops_silently() {
        let (d, _handles, _rx) = dispatcher_with_mock();
        let event = ClipboardEventData {
            r#type: "image_end".into(),
            content: None,
            width: None,
            height: None,
            total_bytes: None,
            chunk_count: None,
            index: None,
        };
        d.handle_clipboard_write(ClipboardPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&event).unwrap(),
        })
        .await;
        // Should not panic; image state still empty.
        let g = d.inner.lock().await;
        assert!(g.image_states.is_empty());
    }

    /// Unknown event types log + drop without panic.
    #[tokio::test]
    async fn unknown_event_type_drops_silently() {
        let (d, _handles, _rx) = dispatcher_with_mock();
        let event = ClipboardEventData {
            r#type: "garbage".into(),
            content: Some("x".into()),
            width: None,
            height: None,
            total_bytes: None,
            chunk_count: None,
            index: None,
        };
        d.handle_clipboard_write(ClipboardPayload {
            connection_id: "c1".into(),
            data: serde_json::to_vec(&event).unwrap(),
        })
        .await;
    }
}
