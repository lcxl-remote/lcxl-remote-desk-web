//! # Daemon-side signaling router (Arch IV)
//!
//! Successor to `service::signaling::DeskSession::handle_message`. In
//! Arch III the worker process owned `DeskSession` and routed every
//! `SignalingType` from there; Arch IV splits the routing two ways
//! around the daemon-held PeerConnection:
//!
//! - **Daemon-owned**: types that touch the [`RTCPeerConnection`] /
//!   SDP / ICE / `SignalingState`. Handled inline by the router, on
//!   the daemon side, against [`super::pc_manager`]'s registry.
//! - **Worker-owned**: types that need the user-session WinSta0
//!   (file system, terminal, Tauri shell, screen / audio capture
//!   parameters, ...). Forwarded to the worker — initially as raw
//!   `SignalingMessage` IPC for Arch III compatibility, later as
//!   typed `OpaqueConnectionPayload` events.
//!
//! ## Rollout schedule
//!
//! - **Cut 3a (this commit)**: skeleton + [`classify`] table +
//!   [`route`] entry point that always returns
//!   [`RouteOutcome::ForwardToWorker`]. The dispatch point is wired
//!   into `signaling_proxy` so subsequent cuts can flip individual
//!   types over without touching the proxy again.
//! - **Cut 3b**: daemon takes over `RequestRemote` / `Offer` /
//!   `Answer` / `Canid` / `CloseControl` against a
//!   `pc_manager::PcRegistry`.
//! - **Cut 3c**: worker-owned types switch from raw `SignalingMessage`
//!   forwarding to typed event-transport payloads (`MouseInput` /
//!   `ClipboardWrite` / `FileTransferCommand` / etc. — the variants
//!   added in PR 1 commit 3).

use desk_signal_facade::model::signal::{SignalingModel, SignalingType};

/// Whether a given `SignalingType` is owned by the daemon (handled
/// inline against the PC registry) or by the worker (forwarded over
/// IPC). Pure function — easy to unit-test exhaustively.
///
/// The full audit is in `agent_works/web/2026-05-03_pr2-pre-flight-audit`
/// (committed alongside PR 2 cut 1).
pub fn classify(signaling_type: SignalingType) -> RouteOwnership {
    match signaling_type {
        // ---- Daemon-owned: PC / SDP / ICE / SignalingState ----
        SignalingType::RequestRemote
        | SignalingType::Init
        | SignalingType::Offer
        | SignalingType::Answer
        | SignalingType::Canid
        | SignalingType::CloseControl => RouteOwnership::Daemon,

        // Daemon-emitted notifications. Browsers don't send these
        // back at us, but if they did the daemon should swallow them
        // rather than relay to the worker (which has no PC to act on).
        SignalingType::DesktopSwitching | SignalingType::DesktopReady => RouteOwnership::Daemon,

        // Connection-list bookkeeping is daemon state too — the
        // daemon knows about every active PC, the worker only knows
        // its own per-connection encoder set.
        SignalingType::FetchConnections | SignalingType::ConnectionList => {
            RouteOwnership::Daemon
        }

        // Heartbeat is a WS keepalive — not for the worker.
        SignalingType::Heartbeat => RouteOwnership::Daemon,

        // ---- Worker-owned: user-session resources ----
        SignalingType::RequireControl
        | SignalingType::AcceptControl
        | SignalingType::DenyControl
        | SignalingType::ChangeDisplaySettings
        | SignalingType::EnablePrivateScreen
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackError
        | SignalingType::UpdateDeskSettings
        | SignalingType::ManagerSystemInfo
        | SignalingType::ManagerSystemStatue
        | SignalingType::ManagerFileList
        | SignalingType::ManagerFileDelete
        | SignalingType::StartTerminal
        | SignalingType::SendDataToTerminal
        | SignalingType::ResizeTerminal
        | SignalingType::CloseTerminal
        | SignalingType::ReplyFromTerminal
        | SignalingType::ListTerminal
        | SignalingType::TerminalStarted
        | SignalingType::TerminalClosed
        | SignalingType::ManagerQuerySettings
        | SignalingType::ManagerUpdateSettings => RouteOwnership::Worker,

        // ---- Error / Unknown ----
        // Treat error envelopes as worker-bound for the time being;
        // worker's existing handler logs and discards them. PR 7
        // will revisit once daemon-side error reporting lands.
        SignalingType::Error | SignalingType::Unknown => RouteOwnership::Worker,
    }
}

/// Whether a `SignalingType` is owned by the daemon or the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOwnership {
    Daemon,
    Worker,
}

/// What [`route`] told the caller to do with the message.
///
/// `ForwardToWorker` is the *legacy* outcome that says "ship the raw
/// JSON to the worker via `ServiceToWorker::SignalingMessage`". Cuts
/// 3b / 3c shrink its remit one variant at a time; PR 7 retires the
/// variant entirely once every signaling type has either been
/// handled in the daemon or replaced by a typed event-transport
/// payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Daemon handled the message inline; caller does nothing more.
    HandledByDaemon,
    /// Daemon did not handle the message; caller falls back to the
    /// raw-JSON `ServiceToWorker::SignalingMessage` IPC path.
    ForwardToWorker,
}

/// Errors the router can surface while routing.
#[derive(Debug)]
pub enum RouterError {
    // Cut 3b populates with concrete failure modes (PC creation
    // failure, ICE filtering rejection, worker IPC down, ...).
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unreachable!("no variants yet")
    }
}

impl std::error::Error for RouterError {}

/// Context the router needs from the calling daemon. Currently empty
/// — cut 3b populates it with the per-connection PC registry, the
/// outbound broadcast channel for sending replies back to the
/// signaling server, the worker_mgr handle, etc.
#[derive(Default)]
pub struct RouterContext {}

/// Route a signaling message. Cut 3a always answers
/// [`RouteOutcome::ForwardToWorker`] regardless of classification —
/// the router is wired in as a pass-through so subsequent cuts can
/// flip individual `SignalingType` variants over without touching
/// the call site again.
pub async fn route(
    _model: &SignalingModel,
    _ctx: &RouterContext,
) -> Result<RouteOutcome, RouterError> {
    Ok(RouteOutcome::ForwardToWorker)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Daemon-owned: WebRTC SDP/ICE/PC lifecycle + daemon-emitted
    /// notifications + connection bookkeeping + WS heartbeat.
    /// Pinning these prevents accidental classification flips: the
    /// only way to move a daemon type back to the worker should be a
    /// deliberate code review.
    #[test]
    fn classify_daemon_owned_types() {
        for t in [
            SignalingType::RequestRemote,
            SignalingType::Init,
            SignalingType::Offer,
            SignalingType::Answer,
            SignalingType::Canid,
            SignalingType::CloseControl,
            SignalingType::DesktopSwitching,
            SignalingType::DesktopReady,
            SignalingType::FetchConnections,
            SignalingType::ConnectionList,
            SignalingType::Heartbeat,
        ] {
            assert_eq!(
                classify(t),
                RouteOwnership::Daemon,
                "{t:?} should be daemon-owned",
            );
        }
    }

    /// Worker-owned: user-session resources (files, terminal,
    /// settings, overlays, approval, manager queries).
    #[test]
    fn classify_worker_owned_types() {
        for t in [
            SignalingType::RequireControl,
            SignalingType::AcceptControl,
            SignalingType::DenyControl,
            SignalingType::ChangeDisplaySettings,
            SignalingType::EnablePrivateScreen,
            SignalingType::PrivateScreenStateChanged,
            SignalingType::AudioPlaybackError,
            SignalingType::UpdateDeskSettings,
            SignalingType::ManagerSystemInfo,
            SignalingType::ManagerSystemStatue,
            SignalingType::ManagerFileList,
            SignalingType::ManagerFileDelete,
            SignalingType::StartTerminal,
            SignalingType::SendDataToTerminal,
            SignalingType::ResizeTerminal,
            SignalingType::CloseTerminal,
            SignalingType::ReplyFromTerminal,
            SignalingType::ListTerminal,
            SignalingType::TerminalStarted,
            SignalingType::TerminalClosed,
            SignalingType::ManagerQuerySettings,
            SignalingType::ManagerUpdateSettings,
            SignalingType::Error,
            SignalingType::Unknown,
        ] {
            assert_eq!(
                classify(t),
                RouteOwnership::Worker,
                "{t:?} should be worker-owned",
            );
        }
    }

    /// Cut 3a contract: `route` always defers to the worker. Pinning
    /// this prevents an accidental "I implemented the daemon path
    /// for X" change from sneaking through without flipping the
    /// matching `classify` arm in the same commit.
    #[tokio::test]
    async fn route_always_forwards_in_cut_3a() {
        let ctx = RouterContext::default();
        for t in [
            SignalingType::RequestRemote, // would-be daemon-owned, but cut 3a still forwards
            SignalingType::ManagerFileList, // worker-owned
            SignalingType::Heartbeat,     // daemon-owned (cut 3b will handle)
        ] {
            let model = SignalingModel::new(
                "test",
                t,
                None,
                None,
                None,
                None,
            );
            assert_eq!(
                route(&model, &ctx).await.unwrap(),
                RouteOutcome::ForwardToWorker,
            );
        }
    }
}
