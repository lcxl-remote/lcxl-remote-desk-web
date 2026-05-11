//! # desk-ipc-protocol
//!
//! IPC protocol definitions for Service ↔ Worker and Service ↔ UI
//! communication. Two architectures coexist in this crate while the
//! Arch IV migration lands incrementally:
//!
//! ## Arch III (legacy, single bidirectional pipe)
//!
//! ```text
//!  Service Core (SYSTEM) ── single named pipe ── Desk Worker (User Session)
//!         (write_message / read_message in [`transport`])
//! ```
//!
//! Wire format: little-endian `u32` length prefix + wincode payload
//! (FixInt + LittleEndian, preallocation limit disabled — see
//! [`transport::IPC_CONFIG`]). Frame size cap: 16 MB enforced at the
//! transport layer. Messages travel as
//! [`message::ServiceToWorker`] / [`message::WorkerToService`].
//!
//! ## Arch IV (target, dual independent pipes)
//!
//! ```text
//!                                                    ┌─ media (worker → daemon, single direction)
//!  Daemon (holds RTCPeerConnection) ── two pipes ────┤
//!                                                    └─ event (bidirectional)
//!  Worker = capture + encode + inject only
//! ```
//!
//! - **Media transport** carries [`message::MediaFrame`]s only.
//!   Bounded(8) queue. P-frames *drop* on backpressure; I-frames block
//!   for at most 500 ms then surface
//!   [`dual_transport::TransportError::IFrameTimeout`] (worker reports
//!   the failure upward, daemon resets the channel — no deadlock).
//! - **Event transport** carries `ServiceToWorker` / `WorkerToService`.
//!   Bounded(256) queue, *never drops* — sender awaits-on-full so the
//!   producer slows down. Splitting media from events eliminates the
//!   head-of-line blocking the POC measured under 4K extreme load
//!   (mouse P99 from 2.6 ms → 0.56 ms; max from 10 ms → 3.6 ms).
//!
//! See the [`dual_transport`] module for traits, the in-process
//! implementation, and the framed (byte-stream) helpers used by both
//! the named-pipe and `tokio::io::duplex` paths. The named-pipe ACL
//! constructor lives in `desk_server::daemon::pipe_security` (Win32
//! plumbing kept out of this crate).
//!
//! ## Multi-`connection_id` concurrency
//!
//! One worker process serves many simultaneous browser-side connections
//! over a *single pair* of pipes. Each per-connection IPC variant
//! carries a `connection_id` field; routing on top of the transport is
//! the responsibility of higher-level dispatch on each side.
//!
//! Daemon side:
//!   * indexed `HashMap<connection_id, PeerConnectionContext>` —
//!     each holds the RTCPeerConnection, video/audio tracks, DC
//!     senders, and `SignalingState`;
//!   * incoming `WorkerToService` is dispatched to the correct PC by
//!     `connection_id`;
//!   * outgoing `ServiceToWorker` is sent on the (single) event pipe
//!     with the `connection_id` field set so the worker can route it
//!     to the right encoder / clipboard / handler.
//!
//! Worker side:
//!   * `HashMap<connection_id, PerConnectionContext>` — encoder,
//!     cursor channel, IPC sender, per-connection state. `Drop` on
//!     this struct is the *only* cleanup trigger; map removal is the
//!     single source of truth.
//!   * **Capture is shared** across connections (one DXGI duplication
//!     per desktop); a `tokio::sync::broadcast` distributes the raw
//!     frame to every per-connection encoder. Capture stops only when
//!     the last encoder for a desktop drops.
//!   * `ForceKeyframe { connection_id }` is **never broadcast** — a
//!     PLI from browser A must not produce an IDR burst on
//!     browser B's encoder.
//!   * `StopMedia { connection_id }` releases the encoder, the IPC
//!     sender, and the cursor channel via the per-connection context's
//!     `Drop`. The worker never infers connection-close from any other
//!     signal — daemon owns connection lifecycle.
//!
//! Rationale captured here so it survives PR-by-PR refactoring; the
//! dispatch tables themselves live in the daemon / worker modules
//! (PR 2 onward).

pub mod dual_transport;
pub mod message;
pub mod transport;
