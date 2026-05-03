//! # Worker-side media producer (Arch IV)
//!
//! Owns the screen / audio capture loop and the per-`connection_id`
//! encoder pool. Replaces the in-`service::signaling`-mod
//! `capture_screen_task` / `capture_audio_task` that ran one capture
//! pipeline per peer connection in Arch III; in Arch IV one worker
//! serves many simultaneous browsers and capture is shared across
//! connections.
//!
//! ## Responsibilities
//!
//! - Hold one capture pipeline per *desktop* (typically one DXGI
//!   duplication on Windows). When the desktop changes, the desktop
//!   monitor signals — the producer reuses the existing pipeline if
//!   the new desktop already has one, or starts a fresh one.
//! - Distribute the raw captured frame to per-connection encoders via
//!   `tokio::sync::broadcast`. Each connection gets its own encoder
//!   (codec / bitrate / fps configured independently from
//!   `ServiceToWorker::StartMedia`); the encoder produces
//!   [`desk_ipc_protocol::message::MediaFrame`]s that flow to the
//!   daemon via the dedicated media transport.
//! - Handle the per-connection event-transport messages:
//!   - `StartMedia { connection_id }` → spawn encoder, register with
//!     capture broadcast.
//!   - `StopMedia { connection_id }` → drop the per-connection
//!     context (encoder, channels). Capture stops only when the last
//!     encoder for the desktop is gone (enforced via `Weak` references
//!     from the capture loop).
//!   - `ForceKeyframe { connection_id }` → on the *next* encode pass
//!     the matching encoder emits a video I-frame.
//!   - `UpdateMediaSettings { connection_id, ... }` → live
//!     fps / bitrate / quality update without recreating the encoder.
//! - Emit a one-shot
//!   [`desk_ipc_protocol::message::WorkerToService::Capabilities`]
//!   message immediately after `Ready` so the daemon knows which
//!   codecs / devices the worker can drive on this desktop.
//!
//! Backpressure handling is delegated to
//! `desk_ipc_protocol::dual_transport::MediaSender`:
//! P-frames drop on a full media queue; I-frames block up to 500 ms
//! and then surface `TransportError::IFrameTimeout`. The producer
//! turns the timeout into
//! `WorkerToService::Error { code: MediaTransportStuck, connection_id }`
//! so the daemon can issue `StopMedia` + `StartMedia` to reset rather
//! than the worker self-deciding to abort.
//!
//! ## Status
//!
//! Skeleton only — populated in PR 2 cut 4.

// The real `pub struct MediaProducer { ... }` and the
// `pub fn run(...)` task entry point land in PR 2 cut 4.
