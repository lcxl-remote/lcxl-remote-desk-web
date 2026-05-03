//! # Daemon-side WebRTC PeerConnection manager (Arch IV)
//!
//! Owner of the [`webrtc::peer_connection::RTCPeerConnection`] lifecycle.
//! In Arch III the worker process held the PC, which meant every UAC /
//! lock-screen / OS-session-switch (any event that respawns the worker)
//! tore down the PC and forced the browser through full SDP renegotiation
//! + ICE restart — a path that became unstable under SYSTEM-token +
//! Winlogon desktop combinations and showed up as "video garbled / ICE
//! checking → failed" during UAC.
//!
//! Arch IV moves the PC into the daemon: WebRTC negotiation happens once
//! per browser session and survives every worker swap. Worker replacement
//! becomes invisible to the browser apart from a ~1 s frame freeze waiting
//! for the next IDR from the new encoder.
//!
//! ## Responsibilities
//!
//! - Create the [`RTCPeerConnection`] for a given `connection_id`,
//!   including video / audio `TrackLocalStaticSample`s and the standard
//!   set of named `DataChannel`s (`mouse-event`, `mouse-move-event`,
//!   `keyboard-event`, `clipboard-event`, `cursor-sync-event`,
//!   `file-transfer-event`, `whiteboard-event`).
//! - Drive the SDP offer/answer dance with the browser via the daemon's
//!   own signaling stack (no longer routed through the worker).
//! - Read RTCP from each track's RTP sender; when a PLI / FIR comes in
//!   on a video track, fire
//!   [`desk_ipc_protocol::message::ServiceToWorker::ForceKeyframe`]
//!   for the matching `connection_id` over the event transport — the
//!   target encoder lives in the worker.
//! - Bridge incoming MediaFrames from the worker (received over the
//!   media transport) into `track.write_sample(...)` on the matching
//!   per-connection track.
//! - Hold per-connection `SignalingState` (accept_control,
//!   accept_clipboard_sync, display_info, ...). Arch III's
//!   per-connection accept-state synchronisation across worker
//!   restarts becomes a no-op: the daemon survives the worker, so the
//!   state never has to leave its memory.
//!
//! ## Status
//!
//! Skeleton only. PR 2 cut 2 moves
//! `service::signaling::init_ptc_peer_connection` here and refactors
//! `start_webrtc` so the capture / encode tasks live in the worker
//! instead.

// Skeleton — the real types land in PR 2 cut 2. Kept as a module
// declaration here so the daemon module tree reflects the final shape
// during PR 1 → PR 2 review.
