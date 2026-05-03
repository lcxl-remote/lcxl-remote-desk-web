//! # Daemon-side signaling router (Arch IV)
//!
//! Successor to `service::signaling::DeskSession::handle_message`. In
//! Arch III the worker process owned `DeskSession` and routed every
//! `SignalingType` from there; Arch IV splits the routing two ways:
//!
//! - **Daemon-handled** types: `RequestRemote`, `Offer`, `Answer`,
//!   `Canid` (sic — ICE candidate), `CloseControl`. Anything that
//!   touches the [`RTCPeerConnection`] / SDP / ICE / `SignalingState`
//!   stays in the daemon ([`super::pc_manager`]) because that is now
//!   where the PC lives.
//! - **Worker-routed** types: `RequireControl`, `EnablePrivateScreen`,
//!   `ManagerSystemInfo`, `ManagerFileList`, `StartTerminal` and the
//!   rest of the manager-shell / user-session ops. These all run in
//!   the user's WinSta0 (file system, terminal, Tauri shell), so the
//!   daemon forwards the `SignalingModel` payload over the event
//!   transport using the `OpaqueConnectionPayload` family
//!   (e.g. `WhiteboardCommand`, `FileTransferCommand`, etc.).
//!
//! The full audit table lives in `agent_works/web/2026-05-03_pr2-pre-flight-audit`
//! (see PR 2 commit message); this module's `route(...)` entry point
//! must remain *exhaustive* on `SignalingType` so the compiler catches
//! a missing case the moment a new variant is added to
//! `desk-signal-facade`.
//!
//! ## Status
//!
//! Skeleton only — populated in PR 2 cut 3.

// The real `pub fn route(model: SignalingModel, ctx: &RouterContext) ->
// Result<...>` entry point lands in PR 2 cut 3.
