//! # Dual-pipe IPC transport (Arch IV)
//!
//! Two independent transports run in parallel between daemon and worker:
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
//! ## Design notes
//!
//! - The trait surface is intentionally minimal. Concrete implementations
//!   ([`InProcess`] and [`framed`]) live alongside; PR 1 commit 5 adds
//!   the strict-ACL named-pipe constructor in `server`.
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
//!   trigger in Arch IV. Workers do not infer connection-close from
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
    /// Wire-level encode failure (bincode). Indicates a bug; not
    /// retryable.
    Encode(bincode::error::EncodeError),
    /// Wire-level decode failure (bincode). Indicates wire corruption
    /// or a protocol-version mismatch; not retryable.
    Decode(bincode::error::DecodeError),
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
impl From<bincode::error::EncodeError> for TransportError {
    fn from(e: bincode::error::EncodeError) -> Self {
        TransportError::Encode(e)
    }
}
impl From<bincode::error::DecodeError> for TransportError {
    fn from(e: bincode::error::DecodeError) -> Self {
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

    /// Construct an in-process event transport. Returns
    /// `(sender, receiver)`.
    pub fn make_event<M: Send + 'static>() -> (Arc<dyn EventSender<M>>, Box<dyn EventReceiver<M>>) {
        let (tx, rx) = mpsc::channel::<M>(EVENT_QUEUE_CAP);
        (
            Arc::new(InProcessEventSender { tx }),
            Box::new(InProcessEventReceiver { rx }),
        )
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
/// `max_frame_length = 16 MB`, payload is bincode v2.
///
/// Each direction needs its own internal mpsc + writer task: the writer
/// task drains the mpsc and pushes onto the framed sink so the public
/// `send_*` methods don't have to hold a lock on the underlying stream
/// across `await` points (which would defeat the bounded-queue
/// backpressure).
pub mod framed {
    use super::*;
    use bincode::{Decode, Encode};
    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::sync::mpsc;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    const MAX_FRAME: usize = 16 * 1024 * 1024;

    fn bincode_config() -> bincode::config::Configuration {
        bincode::config::standard()
    }

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
                let bytes = match bincode::encode_to_vec(&frame, bincode_config()) {
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
            match bincode::decode_from_slice::<MediaFrame, _>(&bytes, bincode_config()) {
                Ok((frame, _)) => Some(frame),
                Err(e) => {
                    log::error!("media receiver decode failed: {e}");
                    None
                }
            }
        }
    }

    // ---- Event (bi-directional, generic over message type) ----

    /// Spawn an event writer task. Mirrors [`spawn_media_sender`] but
    /// uses `EVENT_QUEUE_CAP` and the never-drop `send().await` policy.
    pub fn spawn_event_sender<W, M>(writer_io: W) -> Arc<dyn EventSender<M>>
    where
        W: AsyncWrite + Unpin + Send + 'static,
        M: Encode + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<M>(EVENT_QUEUE_CAP);
        tokio::spawn(async move {
            let mut sink = Framed::new(writer_io, make_codec());
            while let Some(msg) = rx.recv().await {
                let bytes = match bincode::encode_to_vec(&msg, bincode_config()) {
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

    pub fn make_event_receiver<R, M>(reader_io: R) -> Box<dyn EventReceiver<M>>
    where
        R: AsyncRead + Unpin + Send + 'static,
        M: Decode<()> + Send + 'static,
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
    impl<R: AsyncRead + Unpin + Send, M: Decode<()> + Send + 'static> EventReceiver<M>
        for FramedEventReceiver<R, M>
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
            match bincode::decode_from_slice::<M, _>(&bytes, bincode_config()) {
                Ok((msg, _)) => Some(msg),
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
    use crate::message::{MediaCodec, MediaFrame, MediaFrameKind, ServiceToWorker};

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
        assert_eq!(
            rx.recv_frame().await.unwrap().kind,
            MediaFrameKind::VideoP
        );
        assert_eq!(
            rx.recv_frame().await.unwrap().kind,
            MediaFrameKind::VideoI
        );
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
        assert!(matches!(receiver.recv().await, Some(ServiceToWorker::Shutdown)));
    }
}
