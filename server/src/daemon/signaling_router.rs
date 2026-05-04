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
use desk_ipc_protocol::message::{
    EnablePrivateScreenPayload, ServiceToWorker, UpdateDeskSettingsPayload,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::private_screen::EnablePrivateScreenData;
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

        // Batch 1 of the typed-IPC migration: types that only flow
        // *outbound* from the host (worker → daemon → browser) or
        // are dead enums no client/worker handles. An inbound copy
        // is a protocol error from the browser; daemon swallows it
        // here rather than bridging — the worker would either fall
        // through to `UNKNOWN_SIGNALING_TYPE` (ChangeDisplaySettings:
        // never wired up) or have no handler at all.
        //
        // - `ChangeDisplaySettings`: the front-end never emits it
        //   and the worker's `DeskSession::handle_message` has no
        //   arm; effectively a dead enum variant.
        // - `PrivateScreenStateChanged`: worker → browser only;
        //   emitted by `WorkerToService::PrivateScreenStateChanged`
        //   typed IPC since this batch.
        // - `AudioPlaybackError`: emitted from the PC's `on_track`
        //   callback; in Arch IV daemon-worker mode the daemon's
        //   pc_manager does not attach an `on_track` handler so the
        //   variant is dead until that work lands. Portable mode
        //   still produces it from `service::signaling`, but that
        //   path bypasses the router entirely.
        SignalingType::ChangeDisplaySettings
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackError => RouteOwnership::Daemon,

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
        // `EnablePrivateScreen` and `UpdateDeskSettings` are still
        // worker-owned (the actual handlers live in
        // `service/signaling::DeskSession::handle_message`'s arms
        // for those types) but as of batch 1 they ride typed
        // [`ServiceToWorker::EnablePrivateScreen`] /
        // [`ServiceToWorker::UpdateDeskSettings`] IPC instead of
        // the legacy `SignalingMessage` opaque envelope. The router
        // returns `HandledByDaemon` for both because the typed
        // IPC send happens inline below.
        SignalingType::EnablePrivateScreen
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
        // Daemon-emitted or dead inbound; the browser should never
        // send these at us but if it does, swallow rather than relay
        // to the worker. See classify() doc-comments for per-variant
        // rationale.
        SignalingType::Answer
        | SignalingType::Init
        | SignalingType::AcceptControl
        | SignalingType::DenyControl
        | SignalingType::ChangeDisplaySettings
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackError
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
        SignalingType::EnablePrivateScreen => {
            handle_enable_private_screen_inbound(ctx, model).await?;
            Ok(RouteOutcome::HandledByDaemon)
        }
        SignalingType::UpdateDeskSettings => {
            handle_update_desk_settings_inbound(ctx, model).await?;
            Ok(RouteOutcome::HandledByDaemon)
        }
        // Worker-owned: subsequent batches will flip these to typed
        // event-transport payloads; for now they keep flowing as raw
        // SignalingMessage IPC over the legacy bridge.
        _ => Ok(RouteOutcome::ForwardToWorker),
    }
}

/// Batch 1: parse the inbound `EnablePrivateScreen` payload and ship
/// it to the worker as typed [`ServiceToWorker::EnablePrivateScreen`].
/// Replaces the legacy `SignalingMessage` opaque envelope.
///
/// Parse / send failures are non-fatal for the WS connection — they
/// only prevent the toggle from reaching the worker, which is logged.
async fn handle_enable_private_screen_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let from_connection_id = match model.from_connection_id.as_deref() {
        Some(id) => id.to_string(),
        None => {
            log::warn!(
                "[router] EnablePrivateScreen missing from_connection_id; ignoring \
                 (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let data = match model.get_data::<EnablePrivateScreenData>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!(
                "[router] EnablePrivateScreen payload parse failed for {from_connection_id}: \
                 {e}; ignoring"
            );
            return Ok(());
        }
    };
    let payload = EnablePrivateScreenPayload {
        connection_id: from_connection_id.clone(),
        enable: data.enable,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::EnablePrivateScreen(payload))
        .await
    {
        log::warn!(
            "[router] failed to send typed EnablePrivateScreen for {from_connection_id}: {e}",
        );
    }
    Ok(())
}

/// Batch 1: parse the inbound `UpdateDeskSettings` payload, fan out
/// the media-relevant knobs as `UpdateMediaSettings` IPC (so the
/// per-connection encoder pipeline retunes live), and ship the full
/// settings to the worker as typed
/// [`ServiceToWorker::UpdateDeskSettings`] so the worker's
/// `handle_update_desk_settings` still applies non-media fields
/// (`wayland_control_mode`, `private_screen` flags, ...).
async fn handle_update_desk_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let settings = match model.get_data::<DeskSettings>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] UpdateDeskSettings payload parse failed: {e}; dropping \
                 (no media retune, no worker forward)"
            );
            return Ok(());
        }
    };

    ctx.pc_registry
        .broadcast_media_settings_update(
            &ctx.worker_mgr,
            Some(settings.video_fps),
            None,
            Some(settings.video_quality),
        )
        .await;

    let from_connection_id = model
        .from_connection_id
        .clone()
        .unwrap_or_else(|| "<unscoped>".to_string());
    let payload = UpdateDeskSettingsPayload {
        connection_id: from_connection_id.clone(),
        settings,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::UpdateDeskSettings(payload))
        .await
    {
        log::warn!(
            "[router] failed to send typed UpdateDeskSettings for {from_connection_id}: {e}",
        );
    }
    Ok(())
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
            SignalingType::ChangeDisplaySettings,
            SignalingType::PrivateScreenStateChanged,
            SignalingType::AudioPlaybackError,
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
            SignalingType::EnablePrivateScreen,
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

    /// Daemon-emitted or dead inbound variants are swallowed — they
    /// MUST NOT reach the worker (it has no PC to act on, and the
    /// worker's `DeskSession::handle_message` would only return
    /// `UNKNOWN_SIGNALING_TYPE` for the ones it can't handle and
    /// bounce a confusing error to the browser).
    ///
    /// Batch 1 of the typed-IPC migration adds
    /// `ChangeDisplaySettings` (dead enum), `PrivateScreenStateChanged`
    /// (worker → browser only), and `AudioPlaybackError` (dead in
    /// daemon-worker mode) to this list.
    #[tokio::test]
    async fn route_swallows_daemon_emitted_variants() {
        let ctx = make_ctx();
        for t in [
            SignalingType::Answer,
            SignalingType::Init,
            SignalingType::AcceptControl,
            SignalingType::DenyControl,
            SignalingType::ChangeDisplaySettings,
            SignalingType::PrivateScreenStateChanged,
            SignalingType::AudioPlaybackError,
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

    /// Worker-owned variants that haven't been typed-migrated yet
    /// (manager plane / terminal — batches 2 and 3) still flow over
    /// the legacy `SignalingMessage` IPC bridge.
    #[tokio::test]
    async fn route_forwards_unmigrated_worker_owned_variants() {
        let ctx = make_ctx();
        for t in [
            SignalingType::ManagerFileList,
            SignalingType::ManagerSystemInfo,
            SignalingType::StartTerminal,
            SignalingType::SendDataToTerminal,
        ] {
            let model = SignalingModel::new("r", t, None, None, None, None);
            assert_eq!(
                route(&model, &ctx).await.unwrap(),
                RouteOutcome::ForwardToWorker,
                "{t:?}",
            );
        }
    }

    /// Batch 1: `EnablePrivateScreen` is now handled inline by the
    /// router (typed [`ServiceToWorker::EnablePrivateScreen`] IPC).
    /// `route` returns `HandledByDaemon` — the legacy
    /// `SignalingMessage` bridge no longer carries this type. With no
    /// active worker the typed send is logged but the route call
    /// itself still succeeds.
    #[tokio::test]
    async fn route_enable_private_screen_handled_inline_not_bridged() {
        let ctx = make_ctx();
        let data = desk_signal_facade::model::private_screen::EnablePrivateScreenData {
            enable: true,
        };
        let model = SignalingModel::new(
            "r-eps",
            SignalingType::EnablePrivateScreen,
            Some("conn-priv".to_string()),
            None,
            Some(serde_json::to_value(&data).unwrap()),
            None,
        );
        let outcome = route(&model, &ctx).await.unwrap();
        assert_eq!(
            outcome,
            RouteOutcome::HandledByDaemon,
            "EnablePrivateScreen must ride typed IPC, not the SignalingMessage bridge",
        );
    }

    /// `EnablePrivateScreen` arriving without a `from_connection_id`
    /// is a malformed message — daemon logs and drops, no panic, no
    /// IPC send.
    #[tokio::test]
    async fn route_enable_private_screen_without_connection_id_is_noop() {
        let ctx = make_ctx();
        let data = desk_signal_facade::model::private_screen::EnablePrivateScreenData {
            enable: false,
        };
        let model = SignalingModel::new(
            "r-eps-noid",
            SignalingType::EnablePrivateScreen,
            None,
            None,
            Some(serde_json::to_value(&data).unwrap()),
            None,
        );
        let outcome = route(&model, &ctx).await.unwrap();
        assert_eq!(outcome, RouteOutcome::HandledByDaemon);
    }

    /// Batch 1: `UpdateDeskSettings` is now fully handled by the
    /// router — it both fans out the typed `UpdateMediaSettings` IPC
    /// for the encoder pipeline AND ships the full settings to the
    /// worker as typed [`ServiceToWorker::UpdateDeskSettings`]. The
    /// route returns `HandledByDaemon`; the legacy SignalingMessage
    /// bridge no longer carries this type.
    #[tokio::test]
    async fn route_update_desk_settings_handled_inline_not_bridged() {
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
            RouteOutcome::HandledByDaemon,
            "UpdateDeskSettings must ride typed IPC, not the SignalingMessage bridge",
        );
    }

    /// Malformed `UpdateDeskSettings` payload (not a DeskSettings
    /// object) must not crash the router — it should log and drop.
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
        assert_eq!(
            outcome,
            RouteOutcome::HandledByDaemon,
            "malformed UpdateDeskSettings is logged + dropped — no bridge fallback now \
             that batch 1 carries it on typed IPC",
        );
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
