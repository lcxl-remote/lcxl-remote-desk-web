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
use desk_signal_facade::model::desk_settings::DeskSettings;
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

        // Cut 6: daemon owns SignalingState now, so the per-connection
        // accept-control flow runs daemon-side (browser → daemon →
        // host_control_hub → user → daemon updates SignalingState +
        // emits AcceptControl/DenyControl back). Worker no longer
        // sees RequireControl in daemon-worker mode.
        SignalingType::RequireControl => RouteOwnership::Daemon,

        // Daemon-emitted reply variants for the RequireControl flow.
        // The daemon emits AcceptControl / DenyControl outbound to the
        // browser from `pc_manager::handle_require_control`; browsers
        // never echo them back. If a stray inbound copy arrives the
        // daemon swallows it (worker's `DeskSession::handle_message`
        // has no arm for these and would only return
        // `UNKNOWN_SIGNALING_TYPE` — bridging would just bounce
        // confusing errors back to the browser).
        SignalingType::AcceptControl | SignalingType::DenyControl => RouteOwnership::Daemon,

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
        SignalingType::ChangeDisplaySettings
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
        // Treat error envelopes as worker-bound; the worker's existing
        // handler logs and discards them. A future cleanup can move
        // this to daemon-side reporting if the worker ever stops
        // surfacing useful context.
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
/// `ForwardToWorker` is the transitional outcome that says "ship the
/// raw JSON to the worker via `ServiceToWorker::SignalingMessage`".
/// Daemon-owned types (PC / SDP / ICE / SignalingState) are migrated;
/// 22 worker-owned types (terminal control, manager file/system info,
/// `EnablePrivateScreen`, `UpdateDeskSettings`, ...) still flow over
/// the bridge because their handlers run inside the user-session
/// worker. A future cleanup can replace the bridge with typed
/// event-transport payloads per signaling type.
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
        SignalingType::RequireControl => {
            let settings: &SharedSettings = &ctx.settings;
            pc_manager::handle_require_control(
                &ctx.pc_registry,
                &ctx.outbound_tx,
                settings,
                &ctx.host_control_hub,
                model,
            )
            .await?;
            Ok(RouteOutcome::HandledByDaemon)
        }
        // Daemon-emitted; the browser should never send these at us
        // but if it does, swallow rather than relay to the worker.
        // AcceptControl / DenyControl are reply variants emitted by
        // `pc_manager::handle_require_control`; an inbound copy from
        // the browser is a protocol error and gets dropped here.
        SignalingType::Answer
        | SignalingType::Init
        | SignalingType::AcceptControl
        | SignalingType::DenyControl
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
        SignalingType::UpdateDeskSettings => {
            // The worker's `DeskSession::handle_update_desk_settings`
            // still owns non-media fields (wayland_control_mode,
            // private_screen toggles, etc.), so we keep the
            // SignalingMessage forward via `ForwardToWorker`. In
            // addition, the daemon sniffs the media-relevant knobs
            // here and fans them out as typed `UpdateMediaSettings`
            // IPC so the per-connection encoder pipeline retunes
            // live. Without this hop the worker's media_producer
            // would never see the new fps / quality — its capture
            // loop reads `merged_settings` locally and the legacy
            // watch channel that DeskSession writes to does not
            // drive the Arch IV pipeline.
            match model.get_data::<DeskSettings>() {
                Ok(settings) => {
                    ctx.pc_registry
                        .broadcast_media_settings_update(
                            &ctx.worker_mgr,
                            Some(settings.video_fps),
                            None,
                            Some(settings.video_quality),
                        )
                        .await;
                }
                Err(e) => {
                    log::warn!(
                        "[router] UpdateDeskSettings payload parse failed: {e}; forwarding the \
                         raw message to the worker but no media settings will be retuned"
                    );
                }
            }
            Ok(RouteOutcome::ForwardToWorker)
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
            SignalingType::RequireControl,
            SignalingType::AcceptControl,
            SignalingType::DenyControl,
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

    /// Daemon-emitted-only variants (Answer / Init / AcceptControl /
    /// DenyControl / DesktopSwitching / DesktopReady /
    /// FetchConnections / ConnectionList / Heartbeat) arriving on
    /// the inbound WS stream are swallowed — they MUST NOT reach
    /// the worker (which has no PC to act on, and whose
    /// `DeskSession::handle_message` would only return
    /// `UNKNOWN_SIGNALING_TYPE` and bounce a confusing error to the
    /// browser).
    #[tokio::test]
    async fn route_swallows_daemon_emitted_variants() {
        let ctx = make_ctx();
        for t in [
            SignalingType::Answer,
            SignalingType::Init,
            SignalingType::AcceptControl,
            SignalingType::DenyControl,
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

    /// Pin behaviour: a stray inbound `AcceptControl` (which would
    /// be a protocol error from the browser, since the daemon emits
    /// AcceptControl outbound) is swallowed — `route` returns
    /// `HandledByDaemon` so the message never crosses the
    /// `SignalingMessage` bridge to the worker. This guards the
    /// reclassification done in batch 0 of the typed-IPC migration:
    /// before it the same input reached the worker and got bounced
    /// back as `UNKNOWN_SIGNALING_TYPE`.
    #[tokio::test]
    async fn route_inbound_accept_control_is_swallowed_not_bridged() {
        let ctx = make_ctx();
        let model = SignalingModel::new(
            "stray-accept",
            SignalingType::AcceptControl,
            Some("conn-z".to_string()),
            None,
            None,
            None,
        );
        let outcome = route(&model, &ctx).await.unwrap();
        assert_eq!(
            outcome,
            RouteOutcome::HandledByDaemon,
            "AcceptControl inbound must be swallowed, not bridged",
        );
    }

    /// Worker-owned variants still flow over the legacy IPC path
    /// after cut 3b — cut 3c flips them to typed event-transport
    /// payloads.
    #[tokio::test]
    async fn route_forwards_worker_owned_variants() {
        let ctx = make_ctx();
        for t in [
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

    /// `UpdateDeskSettings` keeps flowing as `ForwardToWorker` (so
    /// the worker's DeskSession sees `wayland_control_mode` etc.) and,
    /// with a parseable DeskSettings payload, the daemon also fans
    /// out a typed `UpdateMediaSettings` per active connection. The
    /// fan-out is exercised over an empty pc_registry here — no panic
    /// even when the inner loop iterates zero cached PCs.
    #[tokio::test]
    async fn route_update_desk_settings_forwards_and_broadcasts() {
        let ctx = make_ctx();
        let settings = desk_signal_facade::model::desk_settings::DeskSettings {
            video_fps: 45,
            video_quality: 33,
            ..desk_signal_facade::model::desk_settings::DeskSettings::default()
        };
        let model = SignalingModel::new(
            "r-update",
            SignalingType::UpdateDeskSettings,
            Some("conn-y".to_string()),
            None,
            Some(serde_json::to_value(&settings).unwrap()),
            None,
        );
        let outcome = route(&model, &ctx).await.unwrap();
        assert_eq!(
            outcome,
            RouteOutcome::ForwardToWorker,
            "UpdateDeskSettings must still bridge to worker for non-media fields"
        );
    }

    /// Malformed `UpdateDeskSettings` payload (not a DeskSettings
    /// object) must not crash the router — it should log and still
    /// return `ForwardToWorker` so the worker's existing handler
    /// gets a chance to log its own validation error.
    #[tokio::test]
    async fn route_update_desk_settings_with_invalid_payload_still_forwards() {
        let ctx = make_ctx();
        let model = SignalingModel::new(
            "r-bad",
            SignalingType::UpdateDeskSettings,
            None,
            None,
            Some(serde_json::json!("not an object")),
            None,
        );
        let outcome = route(&model, &ctx).await.unwrap();
        assert_eq!(outcome, RouteOutcome::ForwardToWorker);
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
