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

use std::sync::Arc;

use actix_web::web;
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use tokio::sync::broadcast;

use crate::daemon::pc_manager::{self, PcRegistry};
use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use crate::host_control::HostControlHub;
use crate::model::settings::SharedSettings;

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
        SignalingType::FetchConnections | SignalingType::ConnectionList => RouteOwnership::Daemon,

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
    /// A handler failed (PC creation, SDP exchange, ICE add, ...).
    /// Carries the upstream `DeskError` so the caller can decide
    /// between reporting back to the signaling server vs. logging.
    Handler(DeskError),
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouterError::Handler(e) => write!(f, "router handler failed: {e}"),
        }
    }
}

impl std::error::Error for RouterError {}

impl From<DeskError> for RouterError {
    fn from(e: DeskError) -> Self {
        RouterError::Handler(e)
    }
}

/// Context the router needs from the calling daemon. Constructed once
/// by `signaling_proxy` per WS connection lifetime and shared with
/// every [`route`] invocation on that connection.
#[derive(Clone)]
pub struct RouterContext {
    pub pc_registry: PcRegistry,
    pub outbound_tx: broadcast::Sender<String>,
    pub settings: web::Data<SharedSettings>,
    pub host_control_hub: Arc<HostControlHub>,
    /// Cut 4: handle_request_remote reads `worker_capabilities` from
    /// here to populate the Init reply, and handle_offer issues
    /// `ServiceToWorker::StartMedia` through it once the SDP exchange
    /// completes (so the worker knows to spin up the per-connection
    /// encoder).
    pub worker_mgr: WorkerManager,
}

/// Route a signaling message.
///
/// Cut 3b: daemon-owned WebRTC SDP/ICE types
/// (`RequestRemote` / `Offer` / `Canid` / `CloseControl`) are
/// dispatched against `ctx.pc_registry`; daemon emits replies via
/// `ctx.outbound_tx`. `Answer` returns `HandledByDaemon` immediately
/// because the daemon-as-callee never receives an Answer (the daemon
/// SENDS Answers in `handle_offer`); a stray Answer in the inbound
/// stream is a protocol error from the browser and dropping it on
/// the floor is the safest reaction.
///
/// `Init` / `DesktopSwitching` / `DesktopReady` /
/// `FetchConnections` / `ConnectionList` / `Heartbeat` are
/// daemon-owned but daemon-emitted in this codebase (the browser
/// does not send them at us); the router classifies them as
/// `HandledByDaemon` so they don't leak to the worker, and the
/// "handler" is a no-op.
///
/// Worker-owned types still return `ForwardToWorker` (cut 3c flips
/// individual ones to typed event-transport payloads).
pub async fn route(
    model: &SignalingModel,
    ctx: &RouterContext,
) -> Result<RouteOutcome, RouterError> {
    match model.signaling_type {
        SignalingType::RequestRemote => {
            let s = ctx.settings.read().await.clone();
            let user_name = "worker_node".to_string(); // Cut 3b placeholder; cut 3c threads CurrentUser through.
            let has_tauri = ctx.host_control_hub.has_tauri_ui();
            let capabilities = ctx.worker_mgr.worker_capabilities();
            pc_manager::handle_request_remote(
                &ctx.pc_registry,
                &ctx.outbound_tx,
                &s,
                &user_name,
                has_tauri,
                capabilities.as_ref(),
                Some(&ctx.worker_mgr),
                model,
            )
            .await?;
            Ok(RouteOutcome::HandledByDaemon)
        }
        SignalingType::Offer => {
            pc_manager::handle_offer(&ctx.pc_registry, &ctx.outbound_tx, &ctx.worker_mgr, model)
                .await?;
            Ok(RouteOutcome::HandledByDaemon)
        }
        SignalingType::Canid => {
            pc_manager::handle_canid(&ctx.pc_registry, model).await?;
            Ok(RouteOutcome::HandledByDaemon)
        }
        SignalingType::CloseControl => {
            pc_manager::handle_close_control(&ctx.pc_registry, &ctx.worker_mgr, model).await?;
            Ok(RouteOutcome::HandledByDaemon)
        }
        // Daemon-emitted; the browser should never send these at us
        // but if it does, swallow rather than relay to the worker.
        SignalingType::Answer
        | SignalingType::Init
        | SignalingType::DesktopSwitching
        | SignalingType::DesktopReady
        | SignalingType::FetchConnections
        | SignalingType::ConnectionList
        | SignalingType::Heartbeat => {
            log::trace!(
                "[router] daemon-emitted variant arrived inbound, dropping: {:?}",
                model.signaling_type,
            );
            Ok(RouteOutcome::HandledByDaemon)
        }
        // Worker-owned: cut 3c flips these to typed event-transport
        // payloads; for now they keep flowing as raw SignalingMessage
        // IPC over the legacy path.
        _ => Ok(RouteOutcome::ForwardToWorker),
    }
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

    fn make_ctx() -> RouterContext {
        let (outbound_tx, _) = broadcast::channel::<String>(16);
        let shared = crate::model::settings::SharedSettings::from(
            crate::model::settings::Settings::default(),
        );
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        let (worker_mgr, _) = WorkerManager::new(settings.clone(), pc_registry.clone());
        RouterContext {
            pc_registry,
            outbound_tx,
            settings,
            host_control_hub: Arc::new(HostControlHub::new_local()),
            worker_mgr,
        }
    }

    /// Daemon-emitted-only variants (Answer / Init / DesktopSwitching
    /// / DesktopReady / FetchConnections / ConnectionList /
    /// Heartbeat) arriving on the inbound WS stream are swallowed —
    /// they MUST NOT reach the worker (which has no PC to act on).
    #[tokio::test]
    async fn route_swallows_daemon_emitted_variants() {
        let ctx = make_ctx();
        for t in [
            SignalingType::Answer,
            SignalingType::Init,
            SignalingType::DesktopSwitching,
            SignalingType::DesktopReady,
            SignalingType::FetchConnections,
            SignalingType::ConnectionList,
            SignalingType::Heartbeat,
        ] {
            let model = SignalingModel::new("r", t, None, None, None, None);
            assert_eq!(
                route(&model, &ctx).await.unwrap(),
                RouteOutcome::HandledByDaemon,
                "{t:?}",
            );
        }
    }

    /// Worker-owned variants still flow over the legacy IPC path
    /// after cut 3b — cut 3c flips them to typed event-transport
    /// payloads.
    #[tokio::test]
    async fn route_forwards_worker_owned_variants() {
        let ctx = make_ctx();
        for t in [
            SignalingType::RequireControl,
            SignalingType::ManagerFileList,
            SignalingType::ManagerSystemInfo,
            SignalingType::EnablePrivateScreen,
        ] {
            let model = SignalingModel::new("r", t, None, None, None, None);
            assert_eq!(
                route(&model, &ctx).await.unwrap(),
                RouteOutcome::ForwardToWorker,
                "{t:?}",
            );
        }
    }

    /// `CloseControl` against an empty registry doesn't error — the
    /// daemon logs a warning and treats it as a no-op so a stale
    /// CloseControl after a previous PC dispose does not surface as
    /// a handler error to the caller.
    #[tokio::test]
    async fn route_close_control_empty_registry_is_ok() {
        let ctx = make_ctx();
        let model = SignalingModel::new(
            "r",
            SignalingType::CloseControl,
            Some("conn-x".to_string()),
            None,
            None,
            None,
        );
        let outcome = route(&model, &ctx).await.unwrap();
        assert_eq!(outcome, RouteOutcome::HandledByDaemon);
    }
}
