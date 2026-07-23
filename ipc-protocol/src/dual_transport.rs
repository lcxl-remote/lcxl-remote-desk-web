//! # Multi-pipe IPC transport
//!
//! Three independent transports run in parallel between daemon and worker:
//!
//! 1. **`MediaTransport`** — single-direction, worker → daemon. Carries
//!    encoded video / audio frames ([`MediaFrame`]). Bounded capacity,
//!    *drop-on-backpressure* for P-frames so a slow daemon never blocks
//!    the encoder. I-frames block with a hard timeout instead of being
//!    dropped (a missing IDR breaks the stream until the next keyframe).
//!
//! 2. **`EventTransport`** — bidirectional, generic over message type.
//!    Used for [`ServiceToWorker`](crate::message::ServiceToWorker) and
//!    [`WorkerToService`](crate::message::WorkerToService). Larger
//!    bounded capacity, *never drops*; the sender awaits on full so the
//!    upstream producer slows down. Splitting events from media is what
//!    eliminates head-of-line blocking under 4K extreme load (POC found
//!    mouse latency dropped from 10 ms max to 0.56 ms P99 once the two
//!    were on independent pipes).
//!
//! 3. **`FileTransport`** — bidirectional, carries
//!    [`FileTransferPayload`](crate::message::FileTransferPayload).
//!    Same wire shape as the event transport but at a smaller capacity
//!    (`FILE_QUEUE_CAP = 32`) and on its own dedicated pipe. Carved
//!    out so SCTP-level backpressure on a slow browser DataChannel can
//!    propagate end-to-end (DC `dc.send().await` → daemon writer task
//!    → per-connection bounded queue → file lane → worker download
//!    loop → `file.read` blocks). Putting file-transfer on the event
//!    lane would mean a slow GB-scale download head-of-line blocks
//!    `ManagerFileListResponse` / `Heartbeat` / desktop switches —
//!    fix-2026-05-05 (`pc_manager.rs` regression test
//!    `event_lane_unaffected_by_file_lane_backlog`) explicitly forbids
//!    that. The trade-off: a stalled connection will hold up *other*
//!    connections' file transfers (single file lane, single drain
//!    task). Acceptable today; if it becomes a problem,
//!    per-connection sharding or fan-out spawn would be the next step.
//!
//! ## Design notes
//!
//! - The trait surface is intentionally minimal. Concrete implementations
//!   ([`inprocess`] and [`framed`]) live alongside; the strict-ACL
//!   named-pipe constructor lives in `server`.
//! - `MediaSender` distinguishes I-frame vs P-frame at send time so the
//!   transport applies different policies. The default
//!   [`MediaSender::send_frame`] dispatches by [`MediaFrameKind`].
//! - I-frame timeout default: 500 ms. On timeout the worker is expected
//!   to surface `Error { code: MediaTransportStuck }` to the daemon so
//!   the daemon can `StopMedia` + `StartMedia` to reset the channel,
//!   instead of the worker self-deciding to abort.
//! - Capacities (`MEDIA_QUEUE_CAP = 8`, `EVENT_QUEUE_CAP = 256`) come
//!   from the POC sizing — `8` covers ~133 ms of 60 fps frames so the
//!   daemon has slack for one IDR-write spike, and `256` is well above
//!   the worst-case event burst (mouse 100 Hz × few connections + a
//!   handful of one-shot commands).
//!
//! ## Multi-connection_id concurrency contract
//!
//! Every IPC message except `Ready` / `Heartbeat` / `Capabilities` /
//! `Init` / `Shutdown` carries a `connection_id`. The transport itself
//! is *one* pipe per direction — connection-level routing is the
//! responsibility of higher-level dispatch on each side:
//!
//! - **daemon** indexes per-connection `PeerConnectionContext` by
//!   `connection_id` and routes ServiceToWorker out and WorkerToService
//!   in based on that field.
//! - **worker** holds a `HashMap<connection_id, PerConnectionContext>`
//!   for per-connection encoder + clipboard + cursor state. The worker's
//!   capture loop is shared (one DXGI duplication per desktop); each
//!   connection owns its own encoder and consumes the same broadcast
//!   feed.
//! - `ForceKeyframe` is **never broadcast** — a PLI from browser A must
//!   not trigger an IDR burst from connection B's encoder.
//! - `StopMedia { connection_id }` is the *only* worker-side cleanup
//!   trigger. Workers do not infer connection-close from
//!   anything else; daemon owns connection lifecycle (PC close →
//!   StopMedia). `PerConnectionContext::Drop` releases the encoder
//!   handle, the per-connection IPC senders, and the cursor channel.
//! - Capture stops only when the last per-desktop encoder for that
//!   capture target stops — bookkeeping enforced via `Weak` references
//!   from the capture loop into the per-connection contexts.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::message::{MediaFrame, MediaFrameKind};
use crate::transport::{IPC_CONFIG, IpcConfigType};

// ---------------------------- Capacities ----------------------------

/// Bounded capacity of the media transport queue. ~8 frames at 60 fps
/// = ~133 ms of slack. Large enough to absorb one IDR write spike on
/// the daemon side without dropping; small enough that a *sustained*
/// slowdown is felt at the worker encoder within a quarter-second.
pub const MEDIA_QUEUE_CAP: usize = 8;

/// Bounded capacity of the event transport queue. Far above any
/// expected steady-state burst (mouse 100 Hz × several connections +
/// occasional one-shots). Sized so legitimate bursts never trigger the
/// `await-on-full` backpressure path.
pub const EVENT_QUEUE_CAP: usize = 256;

/// Bounded capacity of the file-transfer transport queue (per direction).
///
/// File transfer rides its own dedicated lane so SCTP-level
/// backpressure on the browser DataChannel can propagate end-to-end
/// (DC `dc.send().await` → daemon writer task → per-connection bounded
/// queue → file lane → worker download / upload loop) without
/// head-of-line blocking the event lane (heartbeat, signaling, manager
/// responses). 32 chunks × 60 KB ≈ 1.9 MB single-direction buffer
/// cap, matching the previous single-pipe high-watermark.
pub const FILE_QUEUE_CAP: usize = 32;

/// Default I-frame send timeout. If the media queue is full and an
/// I-frame send blocks for this long, the worker treats it as a stuck
/// transport and surfaces an `Error { MediaTransportStuck }` to the
/// daemon instead of deadlocking. The daemon then resets the channel
/// (`StopMedia` + `StartMedia`).
pub const I_FRAME_SEND_TIMEOUT: Duration = Duration::from_millis(500);

// ---------------------------- Errors ----------------------------

/// Errors that can occur on either transport. Variants are designed to
/// be matched on by the daemon/worker (rather than just logged) so the
/// caller can apply the right recovery policy.
#[derive(Debug)]
pub enum TransportError {
    /// The transport is closed (peer disconnected, named-pipe broken,
    /// in-process channel dropped, ...). Caller should tear down the
    /// per-connection state and let the supervisor reconnect.
    Closed,
    /// P-frame could not be enqueued because the queue is full. The
    /// frame is dropped; the encoder should request a fresh keyframe
    /// on the next encode pass so the stream resyncs.
    Backpressured,
    /// I-frame send blocked past [`I_FRAME_SEND_TIMEOUT`]. Indicates a
    /// stuck consumer; worker should surface the failure to the daemon
    /// so the daemon can reset the channel.
    IFrameTimeout,
    /// Wire-level encode failure (wincode). Indicates a bug; not
    /// retryable.
    Encode(wincode::error::WriteError),
    /// Wire-level decode failure (wincode). Indicates wire corruption
    /// or a protocol-version mismatch; not retryable.
    Decode(wincode::error::ReadError),
    /// Underlying IO failure on a framed transport.
    Io(std::io::Error),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Closed => write!(f, "transport closed"),
            TransportError::Backpressured => write!(f, "transport backpressured (frame dropped)"),
            TransportError::IFrameTimeout => write!(
                f,
                "I-frame send timed out after {:?}; transport stuck",
                I_FRAME_SEND_TIMEOUT
            ),
            TransportError::Encode(e) => write!(f, "encode failed: {e}"),
            TransportError::Decode(e) => write!(f, "decode failed: {e}"),
            TransportError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::Encode(e) => Some(e),
            TransportError::Decode(e) => Some(e),
            TransportError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        TransportError::Io(e)
    }
}
impl From<wincode::error::WriteError> for TransportError {
    fn from(e: wincode::error::WriteError) -> Self {
        TransportError::Encode(e)
    }
}
impl From<wincode::error::ReadError> for TransportError {
    fn from(e: wincode::error::ReadError) -> Self {
        TransportError::Decode(e)
    }
}

// ---------------------------- Traits ----------------------------

/// Worker-side handle for the media transport. Caller stamps the frame
/// kind in [`MediaFrame::kind`]; the transport applies the appropriate
/// policy.
#[async_trait]
pub trait MediaSender: Send + Sync {
    /// Try to enqueue a P-frame without waiting. Returns
    /// [`TransportError::Backpressured`] *immediately* on a full queue
    /// (the frame is dropped and the encoder should request a keyframe).
    async fn send_p_frame(&self, frame: MediaFrame) -> Result<(), TransportError>;

    /// Enqueue an I-frame, blocking up to [`I_FRAME_SEND_TIMEOUT`]. On
    /// timeout returns [`TransportError::IFrameTimeout`]; caller (worker)
    /// is expected to report `Error { MediaTransportStuck }` to the
    /// daemon instead of retrying.
    async fn send_i_frame(&self, frame: MediaFrame) -> Result<(), TransportError>;

    /// Convenience: dispatch by [`MediaFrame::kind`]. Audio frames take
    /// the P-frame (drop-on-backpressure) path because audio recovers
    /// cleanly from a few dropped samples without a keyframe equivalent.
    async fn send_frame(&self, frame: MediaFrame) -> Result<(), TransportError> {
        match frame.kind {
            MediaFrameKind::VideoI => self.send_i_frame(frame).await,
            MediaFrameKind::VideoP | MediaFrameKind::Audio => self.send_p_frame(frame).await,
        }
    }
}

/// Daemon-side handle for the media transport.
#[async_trait]
pub trait MediaReceiver: Send {
    /// Receive the next frame. Returns `None` when the transport is
    /// closed (peer disconnected).
    async fn recv_frame(&mut self) -> Option<MediaFrame>;
}

/// Generic-over-message-type bidirectional event transport.
///
/// `M` is `ServiceToWorker` on the worker's receive side and the
/// daemon's send side, and `WorkerToService` on the inverse pair.
#[async_trait]
pub trait EventSender<M: Send + 'static>: Send + Sync {
    /// Enqueue a message. Awaits on full queue (does not drop).
    /// Returns [`TransportError::Closed`] only if the consumer end is
    /// gone.
    async fn send(&self, msg: M) -> Result<(), TransportError>;
}

#[async_trait]
pub trait EventReceiver<M: Send + 'static>: Send {
    /// Receive the next event. Returns `None` when the transport is
    /// closed.
    async fn recv(&mut self) -> Option<M>;
}

// ---------------------------- In-process impl ----------------------------

/// In-process media transport (used by portable mode where daemon and
/// worker live in the same process). Skips serialization entirely.
pub mod inprocess {
    use super::*;

    /// Construct an in-process media transport. Returns
    /// `(sender, receiver)`.
    pub fn make_media() -> (Arc<dyn MediaSender>, Box<dyn MediaReceiver>) {
        let (tx, rx) = mpsc::channel::<MediaFrame>(MEDIA_QUEUE_CAP);
        (
            Arc::new(InProcessMediaSender {
                tx,
                i_frame_timeout: I_FRAME_SEND_TIMEOUT,
            }),
            Box::new(InProcessMediaReceiver { rx }),
        )
    }

    /// Construct an in-process event transport with a custom queue
    /// capacity. The default `make_event` / `make_file_inprocess`
    /// helpers should be preferred; this is exposed primarily so tests
    /// can stress backpressure paths with a tiny capacity (e.g. 2)
    /// without flooding the queue with thousands of payloads.
    pub fn make_event_inprocess_with_cap<M: Send + 'static>(
        cap: usize,
    ) -> (Arc<dyn EventSender<M>>, Box<dyn EventReceiver<M>>) {
        let (tx, rx) = mpsc::channel::<M>(cap);
        (
            Arc::new(InProcessEventSender { tx }),
            Box::new(InProcessEventReceiver { rx }),
        )
    }

    /// Construct an in-process event transport at the default
    /// `EVENT_QUEUE_CAP`. Returns `(sender, receiver)`.
    pub fn make_event<M: Send + 'static>() -> (Arc<dyn EventSender<M>>, Box<dyn EventReceiver<M>>) {
        make_event_inprocess_with_cap(EVENT_QUEUE_CAP)
    }

    /// Construct an in-process file-transfer transport at
    /// `FILE_QUEUE_CAP`. The semantics are identical to `make_event`
    /// (await-backpressure, never-drop) but with the smaller capacity
    /// reserved for the file lane so each direction's buffer is
    /// bounded near the previous single-pipe watermark.
    pub fn make_file_inprocess<M: Send + 'static>()
    -> (Arc<dyn EventSender<M>>, Box<dyn EventReceiver<M>>) {
        make_event_inprocess_with_cap(FILE_QUEUE_CAP)
    }

    pub struct InProcessMediaSender {
        tx: mpsc::Sender<MediaFrame>,
        i_frame_timeout: Duration,
    }

    #[async_trait]
    impl MediaSender for InProcessMediaSender {
        async fn send_p_frame(&self, frame: MediaFrame) -> Result<(), TransportError> {
            match self.tx.try_send(frame) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => Err(TransportError::Backpressured),
                Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::Closed),
            }
        }
        async fn send_i_frame(&self, frame: MediaFrame) -> Result<(), TransportError> {
            match tokio::time::timeout(self.i_frame_timeout, self.tx.send(frame)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(TransportError::Closed),
                Err(_) => Err(TransportError::IFrameTimeout),
            }
        }
    }

    pub struct InProcessMediaReceiver {
        rx: mpsc::Receiver<MediaFrame>,
    }

    #[async_trait]
    impl MediaReceiver for InProcessMediaReceiver {
        async fn recv_frame(&mut self) -> Option<MediaFrame> {
            self.rx.recv().await
        }
    }

    pub struct InProcessEventSender<M> {
        tx: mpsc::Sender<M>,
    }

    #[async_trait]
    impl<M: Send + 'static> EventSender<M> for InProcessEventSender<M> {
        async fn send(&self, msg: M) -> Result<(), TransportError> {
            self.tx.send(msg).await.map_err(|_| TransportError::Closed)
        }
    }

    pub struct InProcessEventReceiver<M> {
        rx: mpsc::Receiver<M>,
    }

    #[async_trait]
    impl<M: Send + 'static> EventReceiver<M> for InProcessEventReceiver<M> {
        async fn recv(&mut self) -> Option<M> {
            self.rx.recv().await
        }
    }
}

// ---------------------------- Framed (byte-stream) impl ----------------------------

/// Framed transport built on top of any `AsyncRead + AsyncWrite` byte
/// stream (e.g. a connected named pipe, a Unix domain socket, a tokio
/// duplex pair in tests). Wire format: `LengthDelimitedCodec` with
/// `max_frame_length = 16 MB`, payload is wincode (FixInt + LittleEndian,
/// preallocation limit disabled — see [`IPC_CONFIG`]).
///
/// Each direction needs its own internal mpsc + writer task: the writer
/// task drains the mpsc and pushes onto the framed sink so the public
/// `send_*` methods don't have to hold a lock on the underlying stream
/// across `await` points (which would defeat the bounded-queue
/// backpressure).
pub mod framed {
    use super::*;
    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::sync::mpsc;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    const MAX_FRAME: usize = 16 * 1024 * 1024;

    fn make_codec() -> LengthDelimitedCodec {
        LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME)
            .new_codec()
    }

    // ---- Media (uni-directional, sender/receiver pair) ----

    /// Spawn a media writer task on top of `writer_io`, returning a
    /// [`MediaSender`] that hands frames to that task via a bounded
    /// mpsc. The writer task drains the queue and serializes onto the
    /// underlying byte stream.
    pub fn spawn_media_sender<W>(writer_io: W) -> Arc<dyn MediaSender>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<MediaFrame>(MEDIA_QUEUE_CAP);
        tokio::spawn(async move {
            let mut sink = Framed::new(writer_io, make_codec());
            while let Some(frame) = rx.recv().await {
                let bytes = match wincode::config::serialize(&frame, IPC_CONFIG) {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("media writer encode failed: {e}");
                        continue;
                    }
                };
                if let Err(e) = sink.send(Bytes::from(bytes)).await {
                    log::warn!("media writer send failed: {e}; exiting");
                    break;
                }
            }
        });
        Arc::new(FramedMediaSender {
            tx,
            i_frame_timeout: I_FRAME_SEND_TIMEOUT,
        })
    }

    /// Build a media receiver around `reader_io`. The receiver decodes
    /// frames inline on `recv_frame` so there's no extra task.
    pub fn make_media_receiver<R>(reader_io: R) -> Box<dyn MediaReceiver>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        Box::new(FramedMediaReceiver {
            stream: Framed::new(reader_io, make_codec()),
        })
    }

    pub struct FramedMediaSender {
        tx: mpsc::Sender<MediaFrame>,
        i_frame_timeout: Duration,
    }

    #[async_trait]
    impl MediaSender for FramedMediaSender {
        async fn send_p_frame(&self, frame: MediaFrame) -> Result<(), TransportError> {
            match self.tx.try_send(frame) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => Err(TransportError::Backpressured),
                Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::Closed),
            }
        }
        async fn send_i_frame(&self, frame: MediaFrame) -> Result<(), TransportError> {
            match tokio::time::timeout(self.i_frame_timeout, self.tx.send(frame)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(TransportError::Closed),
                Err(_) => Err(TransportError::IFrameTimeout),
            }
        }
    }

    pub struct FramedMediaReceiver<R> {
        stream: Framed<R, LengthDelimitedCodec>,
    }

    #[async_trait]
    impl<R: AsyncRead + Unpin + Send> MediaReceiver for FramedMediaReceiver<R> {
        async fn recv_frame(&mut self) -> Option<MediaFrame> {
            let bytes = match self.stream.next().await {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    log::warn!("media receiver read err: {e}");
                    return None;
                }
                None => return None,
            };
            match wincode::config::deserialize::<MediaFrame, _>(&bytes, IPC_CONFIG) {
                Ok(frame) => Some(frame),
                Err(e) => {
                    log::error!("media receiver decode failed: {e}");
                    None
                }
            }
        }
    }

    // ---- Event (bi-directional, generic over message type) ----

    /// Spawn an event writer task with a custom queue capacity.
    /// Mirrors [`spawn_media_sender`] but uses the never-drop
    /// `send().await` policy. The default helpers `spawn_event_sender`
    /// / `spawn_file_sender` should be preferred.
    pub fn spawn_event_sender_with_cap<W, M>(writer_io: W, cap: usize) -> Arc<dyn EventSender<M>>
    where
        W: AsyncWrite + Unpin + Send + 'static,
        M: wincode::SchemaWrite<IpcConfigType, Src = M> + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<M>(cap);
        tokio::spawn(async move {
            let mut sink = Framed::new(writer_io, make_codec());
            while let Some(msg) = rx.recv().await {
                let bytes = match wincode::config::serialize(&msg, IPC_CONFIG) {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("event writer encode failed: {e}");
                        continue;
                    }
                };
                if let Err(e) = sink.send(Bytes::from(bytes)).await {
                    log::warn!("event writer send failed: {e}; exiting");
                    break;
                }
            }
        });
        Arc::new(FramedEventSender { tx })
    }

    /// Spawn an event writer task at the default `EVENT_QUEUE_CAP`.
    pub fn spawn_event_sender<W, M>(writer_io: W) -> Arc<dyn EventSender<M>>
    where
        W: AsyncWrite + Unpin + Send + 'static,
        M: wincode::SchemaWrite<IpcConfigType, Src = M> + Send + 'static,
    {
        spawn_event_sender_with_cap(writer_io, EVENT_QUEUE_CAP)
    }

    /// Spawn a file-transfer writer task at `FILE_QUEUE_CAP`. Same
    /// wire format and semantics as `spawn_event_sender`; carved out
    /// so the file lane can run its own bounded queue independent of
    /// the event lane.
    pub fn spawn_file_sender<W, M>(writer_io: W) -> Arc<dyn EventSender<M>>
    where
        W: AsyncWrite + Unpin + Send + 'static,
        M: wincode::SchemaWrite<IpcConfigType, Src = M> + Send + 'static,
    {
        spawn_event_sender_with_cap(writer_io, FILE_QUEUE_CAP)
    }

    pub fn make_event_receiver<R, M>(reader_io: R) -> Box<dyn EventReceiver<M>>
    where
        R: AsyncRead + Unpin + Send + 'static,
        M: for<'de> wincode::SchemaRead<'de, IpcConfigType, Dst = M> + Send + 'static,
    {
        Box::new(FramedEventReceiver::<R, M> {
            stream: Framed::new(reader_io, make_codec()),
            _marker: std::marker::PhantomData,
        })
    }

    pub struct FramedEventSender<M> {
        tx: mpsc::Sender<M>,
    }

    #[async_trait]
    impl<M: Send + 'static> EventSender<M> for FramedEventSender<M> {
        async fn send(&self, msg: M) -> Result<(), TransportError> {
            self.tx.send(msg).await.map_err(|_| TransportError::Closed)
        }
    }

    pub struct FramedEventReceiver<R, M> {
        stream: Framed<R, LengthDelimitedCodec>,
        _marker: std::marker::PhantomData<M>,
    }

    #[async_trait]
    impl<
        R: AsyncRead + Unpin + Send,
        M: for<'de> wincode::SchemaRead<'de, IpcConfigType, Dst = M> + Send + 'static,
    > EventReceiver<M> for FramedEventReceiver<R, M>
    {
        async fn recv(&mut self) -> Option<M> {
            let bytes = match self.stream.next().await {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    log::warn!("event receiver read err: {e}");
                    return None;
                }
                None => return None,
            };
            match wincode::config::deserialize::<M, _>(&bytes, IPC_CONFIG) {
                Ok(msg) => Some(msg),
                Err(e) => {
                    log::error!("event receiver decode failed: {e}");
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        MediaCodec, MediaFrame, MediaFrameKind, OpaqueConnectionPayload, ServiceToWorker,
    };

    fn make_video_p(seq: u64, payload_bytes: usize) -> MediaFrame {
        MediaFrame {
            connection_id: "c1".to_string(),
            seq,
            ts_ns: 0,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoP,
            codec: MediaCodec::H264,
            payload: vec![0u8; payload_bytes],
        }
    }
    fn make_video_i(seq: u64, payload_bytes: usize) -> MediaFrame {
        MediaFrame {
            kind: MediaFrameKind::VideoI,
            ..make_video_p(seq, payload_bytes)
        }
    }

    // ---------------- In-process media ----------------

    #[tokio::test]
    async fn inproc_media_p_frame_drops_on_backpressure() {
        let (tx, mut rx) = inprocess::make_media();
        // Fill the queue ( capacity = 8 ) without consuming.
        for i in 0..MEDIA_QUEUE_CAP {
            tx.send_p_frame(make_video_p(i as u64, 10)).await.unwrap();
        }
        // 9th P-frame must drop, not block.
        let err = tx.send_p_frame(make_video_p(99, 10)).await.unwrap_err();
        assert!(matches!(err, TransportError::Backpressured));
        // Consumer drains everything queued (doesn't see the dropped 9th).
        for _ in 0..MEDIA_QUEUE_CAP {
            let f = rx.recv_frame().await.expect("frame present");
            assert_eq!(f.kind, MediaFrameKind::VideoP);
        }
    }

    #[tokio::test]
    async fn inproc_media_i_frame_times_out_when_consumer_stalls() {
        let (tx, mut rx) = inprocess::make_media();
        // Fill queue.
        for i in 0..MEDIA_QUEUE_CAP {
            tx.send_p_frame(make_video_p(i as u64, 10)).await.unwrap();
        }
        // I-frame should *block* up to `I_FRAME_SEND_TIMEOUT` then fail.
        let started = tokio::time::Instant::now();
        let err = tx.send_i_frame(make_video_i(100, 10)).await.unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(err, TransportError::IFrameTimeout));
        // Sanity: actually waited at least the timeout amount (allow
        // small scheduler slack on Windows).
        assert!(
            elapsed >= I_FRAME_SEND_TIMEOUT - Duration::from_millis(50),
            "elapsed = {:?}",
            elapsed
        );
        // Drain so receiver isn't left dangling.
        for _ in 0..MEDIA_QUEUE_CAP {
            rx.recv_frame().await;
        }
    }

    #[tokio::test]
    async fn inproc_media_send_frame_dispatches_by_kind() {
        let (tx, mut rx) = inprocess::make_media();
        tx.send_frame(make_video_p(0, 10)).await.unwrap();
        tx.send_frame(make_video_i(1, 10)).await.unwrap();
        assert_eq!(rx.recv_frame().await.unwrap().kind, MediaFrameKind::VideoP);
        assert_eq!(rx.recv_frame().await.unwrap().kind, MediaFrameKind::VideoI);
    }

    // ---------------- In-process event ----------------

    #[tokio::test]
    async fn inproc_event_round_trips() {
        let (tx, mut rx) = inprocess::make_event::<ServiceToWorker>();
        tx.send(ServiceToWorker::Shutdown).await.unwrap();
        assert!(matches!(rx.recv().await, Some(ServiceToWorker::Shutdown)));
    }

    #[tokio::test]
    async fn inproc_event_closes_when_receiver_dropped() {
        let (tx, rx) = inprocess::make_event::<ServiceToWorker>();
        drop(rx);
        let err = tx.send(ServiceToWorker::Shutdown).await.unwrap_err();
        assert!(matches!(err, TransportError::Closed));
    }

    /// Backpressure isolation: filling the media transport must not
    /// affect concurrent event-pipe sends. This is the central
    /// guarantee of the dual-pipe design.
    #[tokio::test]
    async fn dual_pipe_media_backpressure_does_not_stall_event_pipe() {
        let (m_tx, mut _m_rx) = inprocess::make_media();
        let (e_tx, mut e_rx) = inprocess::make_event::<ServiceToWorker>();

        // Saturate media without draining.
        for i in 0..MEDIA_QUEUE_CAP {
            m_tx.send_p_frame(make_video_p(i as u64, 10)).await.unwrap();
        }
        // Event sends must still succeed at low latency. Time them.
        let started = tokio::time::Instant::now();
        for _ in 0..32 {
            e_tx.send(ServiceToWorker::Shutdown).await.unwrap();
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "event sends were stalled by media backpressure: elapsed={:?}",
            elapsed
        );
        // Drain.
        for _ in 0..32 {
            e_rx.recv().await;
        }
    }

    // ---------------- Framed (byte-stream) ----------------

    #[tokio::test]
    async fn framed_media_round_trips_through_duplex() {
        // tokio::io::duplex is a perfect stand-in for a connected pipe.
        let (a, b) = tokio::io::duplex(64 * 1024);
        let sender = framed::spawn_media_sender(a);
        let mut receiver = framed::make_media_receiver(b);

        let f0 = make_video_p(0, 1024);
        let f1 = make_video_i(1, 4096);
        sender.send_frame(f0.clone()).await.unwrap();
        sender.send_frame(f1.clone()).await.unwrap();

        let r0 = receiver.recv_frame().await.unwrap();
        let r1 = receiver.recv_frame().await.unwrap();
        assert_eq!(r0.seq, 0);
        assert_eq!(r0.kind, MediaFrameKind::VideoP);
        assert_eq!(r0.payload.len(), 1024);
        assert_eq!(r1.seq, 1);
        assert_eq!(r1.kind, MediaFrameKind::VideoI);
        assert_eq!(r1.payload.len(), 4096);
    }

    #[tokio::test]
    async fn framed_event_round_trips_through_duplex() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let sender = framed::spawn_event_sender::<_, ServiceToWorker>(a);
        let mut receiver = framed::make_event_receiver::<_, ServiceToWorker>(b);

        sender.send(ServiceToWorker::Shutdown).await.unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(ServiceToWorker::Shutdown)
        ));
    }

    // ---------------- File lane (separate bounded queue) ----------------

    /// `make_event_inprocess_with_cap` honors a custom capacity. We
    /// fill it with `cap` payloads, then assert the next send is
    /// observably blocked (timeout) until the consumer drains one
    /// slot. This is the testing primitive
    /// `download_blocks_when_file_lane_full` in the worker dispatcher
    /// will rely on.
    #[tokio::test]
    async fn file_lane_blocks_when_full_and_unblocks_on_drain() {
        let (tx, mut rx) = inprocess::make_event_inprocess_with_cap::<ServiceToWorker>(2);
        // Fill capacity.
        tx.send(ServiceToWorker::Shutdown).await.unwrap();
        tx.send(ServiceToWorker::Shutdown).await.unwrap();

        // Third send must NOT complete within a short timeout.
        let third = tx.send(ServiceToWorker::Shutdown);
        let res = tokio::time::timeout(Duration::from_millis(50), third).await;
        assert!(
            res.is_err(),
            "third send should be backpressured, got: {res:?}"
        );

        // Drain one; the (currently parked) third send is allowed to
        // complete on its own task. Spawn a fresh send and verify it
        // resolves quickly once a slot frees.
        rx.recv().await.expect("first drain");
        let started = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_millis(100),
            tx.send(ServiceToWorker::Shutdown),
        )
        .await
        .expect("send timed out post-drain")
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "send took too long after drain: {:?}",
            started.elapsed()
        );
    }

    /// `make_file_inprocess` returns a transport at exactly
    /// `FILE_QUEUE_CAP`. Saturate it and confirm the (cap+1)-th send
    /// is observably blocked.
    #[tokio::test]
    async fn make_file_inprocess_uses_file_queue_cap() {
        let (tx, mut _rx) = inprocess::make_file_inprocess::<ServiceToWorker>();
        // Fill exactly FILE_QUEUE_CAP slots without consuming.
        for _ in 0..FILE_QUEUE_CAP {
            tokio::time::timeout(
                Duration::from_millis(50),
                tx.send(ServiceToWorker::Shutdown),
            )
            .await
            .expect("send-within-cap should not block")
            .unwrap();
        }
        // (cap + 1)-th must block.
        let blocked = tokio::time::timeout(
            Duration::from_millis(50),
            tx.send(ServiceToWorker::Shutdown),
        )
        .await;
        assert!(
            blocked.is_err(),
            "send beyond FILE_QUEUE_CAP should block, got: {blocked:?}"
        );
    }

    // === Production payload-size coverage on the framed path ===

    /// A wincode-encoded event payload comfortably above
    /// wincode's 4 MiB default preallocation guard must round-trip
    /// through the framed event lane. If `spawn_event_sender` or
    /// `make_event_receiver` falls back to the default
    /// `wincode::serialize` / `wincode::deserialize` instead of going
    /// through [`crate::transport::IPC_CONFIG`], the preallocation
    /// guard fires on encode and this test fails — that is the gold
    /// standard "did we wire IPC_CONFIG into the framed path"
    /// assertion (twin of `transport::tests::frame_between_4mib_and_16mb_round_trips`).
    #[tokio::test]
    async fn framed_event_above_4mib_round_trips() {
        let (a, b) = tokio::io::duplex(32 * 1024 * 1024);
        let sender = framed::spawn_event_sender::<_, ServiceToWorker>(a);
        let mut receiver = framed::make_event_receiver::<_, ServiceToWorker>(b);

        let payload = vec![0xCDu8; 8 * 1024 * 1024]; // 8 MiB
        let original = ServiceToWorker::WhiteboardCommand(OpaqueConnectionPayload {
            connection_id: "conn-large".to_string(),
            data: payload.clone(),
        });
        sender.send(original).await.expect("send");
        let received = receiver.recv().await.expect("recv");
        match received {
            ServiceToWorker::WhiteboardCommand(p) => {
                assert_eq!(p.connection_id, "conn-large");
                assert_eq!(p.data.len(), payload.len());
                assert_eq!(&p.data[..16], &payload[..16]);
                assert_eq!(&p.data[p.data.len() - 16..], &payload[payload.len() - 16..]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// A 2 MB I-frame (the upper-end of a 4K H.264 IDR seen
    /// in the POC) must round-trip through the framed media lane.
    /// Combined with `framed_event_above_4mib_round_trips`, this
    /// covers both the media and event framed paths against the
    /// 4 MiB preallocation default — the worker's real-world IDR
    /// frame size is the load this lane was built for.
    #[tokio::test]
    async fn framed_media_round_trips_2mb_idr_frame() {
        let (a, b) = tokio::io::duplex(16 * 1024 * 1024);
        let sender = framed::spawn_media_sender(a);
        let mut receiver = framed::make_media_receiver(b);

        let payload = vec![0xEFu8; 2 * 1024 * 1024]; // 2 MB IDR
        let frame = MediaFrame {
            connection_id: "conn-idr".to_string(),
            seq: 1,
            ts_ns: 1_700_000_000_000_000_000,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoI,
            codec: MediaCodec::H264,
            payload: payload.clone(),
        };
        sender.send_frame(frame).await.expect("send");
        let received = receiver.recv_frame().await.expect("recv");
        assert_eq!(received.connection_id, "conn-idr");
        assert_eq!(received.kind, MediaFrameKind::VideoI);
        assert_eq!(received.payload.len(), payload.len());
        assert_eq!(&received.payload[..16], &payload[..16]);
        assert_eq!(
            &received.payload[received.payload.len() - 16..],
            &payload[payload.len() - 16..]
        );
    }
}
