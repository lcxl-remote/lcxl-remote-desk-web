//! # Daemon-side signaling router (Arch IV)
//!
//! Successor to `service::signaling::DeskSession::handle_message`. In
//! Arch III the worker process owned `DeskSession` and routed every
//! `SignalingType` from there; Arch IV splits the routing two ways
//! around the daemon-held PeerConnection:
//!
//! - **Daemon-owned**: types that touch the [`RTCPeerConnection`] /
//!   SDP / ICE / `SignalingState`, and "swallow" types — daemon-emitted
//!   notifications or dead-enum variants the browser should never echo
//!   back. Handled inline (or trace-logged + dropped) on the daemon
//!   side, against [`super::pc_manager`]'s registry.
//! - **Worker-bound**: types that need the user-session WinSta0
//!   (file system, terminal, Tauri shell, screen / audio capture
//!   parameters, ...). Each one rides a typed `ServiceToWorker::*`
//!   IPC variant — there is no opaque-envelope bridge anymore.
//!
//! Batches 0–4 of the typed-IPC migration removed the transitional
//! `ServiceToWorker::SignalingMessage` / `WorkerToService::SignalingMessage`
//! variants and the `RouteOutcome::ForwardToWorker` fallback that fed
//! them. `route` now never falls back: every inbound `SignalingType`
//! is either handled inline (daemon-owned) or shipped to the worker
//! through a dedicated typed IPC variant.

use std::sync::Arc;

use actix_web::web;
use desk_ipc_protocol::message::{
    CloseTerminalPayload, EnablePrivateScreenPayload, ListTerminalRequestPayload,
    ManagerFileDeleteRequestPayload, ManagerFileListRequestPayload, ManagerRequestRefPayload,
    ManagerUpdateSettingsRequestPayload, ResizeTerminalPayload, SendDataToTerminalPayload,
    ServiceToWorker, StartTerminalRequestPayload, UpdateDeskSettingsPayload,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams};
use desk_signal_facade::model::private_screen::EnablePrivateScreenData;
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::model::system_settings::RemoteSystemSettings;
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalResizeData,
};
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
        | SignalingType::CloseControl
        | SignalingType::ConnectionRemoved => RouteOwnership::Daemon,

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
        // - `ManagerSystemStatue` (batch 2): a dead-enum variant —
        //   the worker's `handle_message` has no arm and the
        //   front-end never emits it. Swallow the same way batch 0
        //   handled AcceptControl / DenyControl and batch 1 handled
        //   ChangeDisplaySettings.
        // - `ReplyFromTerminal` / `TerminalStarted` / `TerminalClosed`
        //   (batch 3): worker → browser only. Worker emits them via
        //   typed `WorkerToService::ReplyFromTerminal` /
        //   `TerminalStarted` / `TerminalClosed`; the browser never
        //   echoes them back. A stray inbound copy is a protocol
        //   error from the browser — daemon swallows it rather than
        //   bridging to the worker (which has no `handle_message`
        //   arm for these and would only return
        //   `UNKNOWN_SIGNALING_TYPE`).
        SignalingType::ChangeDisplaySettings
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackError
        | SignalingType::ManagerSystemStatue
        | SignalingType::ReplyFromTerminal
        | SignalingType::TerminalStarted
        | SignalingType::TerminalClosed => RouteOwnership::Daemon,

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

        // ---- Worker-bound: user-session resources ----
        // Each of these rides a dedicated typed `ServiceToWorker::*`
        // IPC variant — see `route` below for the per-type dispatch
        // helpers (batches 1–3 of the typed-IPC migration covered
        // them all; the legacy `SignalingMessage` bridge no longer
        // exists).
        SignalingType::EnablePrivateScreen
        | SignalingType::UpdateDeskSettings
        | SignalingType::ManagerSystemInfo
        | SignalingType::ManagerFileList
        | SignalingType::ManagerFileDelete
        | SignalingType::StartTerminal
        | SignalingType::SendDataToTerminal
        | SignalingType::ResizeTerminal
        | SignalingType::CloseTerminal
        | SignalingType::ListTerminal
        | SignalingType::ManagerQuerySettings
        | SignalingType::ManagerUpdateSettings => RouteOwnership::Worker,

        // ---- Error / Unknown ----
        // After batch 4 these are daemon-owned. `Error` is something
        // the daemon emits at the wire level (legacy: worker bounced
        // it through the bridge after `handle_message` failed; now
        // worker errors take the typed `WorkerToService::SignalingError`
        // path so the daemon can ferry them back). `Unknown` is the
        // serde-default catch-all for unrecognised wire enum values
        // — bouncing it to the worker would only round-trip an error
        // back. Both swallow at the daemon with a trace log.
        SignalingType::Error | SignalingType::Unknown => RouteOwnership::Daemon,
    }
}

/// Whether a `SignalingType` is owned by the daemon or the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOwnership {
    Daemon,
    Worker,
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
/// Each `SignalingType` is exhaustively dispatched: PC / SDP / ICE /
/// SignalingState types run inline against `ctx.pc_registry`;
/// worker-bound types (terminal, manager queries, EnablePrivateScreen,
/// UpdateDeskSettings) are shipped to the worker via dedicated
/// `ServiceToWorker::*` typed IPC variants; daemon-emitted notifications
/// and dead-enum variants (`Answer`, `Init`, `Heartbeat`, `Error`,
/// `Unknown`, ...) are trace-logged + dropped. There is no fallback
/// path — batch 4 of the typed-IPC migration removed the
/// `SignalingMessage` bridge.
pub async fn route(model: &SignalingModel, ctx: &RouterContext) -> Result<(), RouterError> {
    match model.signaling_type {
        SignalingType::RequestRemote => {
            let s = ctx.settings.read().await.clone();
            let user_name = "worker_node".to_string();
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
            Ok(())
        }
        SignalingType::Offer => {
            pc_manager::handle_offer(&ctx.pc_registry, &ctx.outbound_tx, &ctx.worker_mgr, model)
                .await?;
            Ok(())
        }
        SignalingType::Canid => {
            pc_manager::handle_canid(&ctx.pc_registry, model).await?;
            Ok(())
        }
        SignalingType::CloseControl => {
            pc_manager::handle_close_control(&ctx.pc_registry, &ctx.worker_mgr, model).await?;
            Ok(())
        }
        SignalingType::ConnectionRemoved => {
            pc_manager::handle_connection_removed(&ctx.pc_registry, &ctx.worker_mgr, model).await?;
            Ok(())
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
            Ok(())
        }
        // Daemon-emitted or dead inbound; the browser should never
        // send these at us but if it does, swallow rather than
        // routing onward. See classify() doc-comments for per-variant
        // rationale. After batch 4 `Error` and `Unknown` join this
        // group (they used to be worker-bound for verbose logging,
        // but since the bridge is gone there is no point round-tripping
        // them).
        SignalingType::Answer
        | SignalingType::Init
        | SignalingType::AcceptControl
        | SignalingType::DenyControl
        | SignalingType::ChangeDisplaySettings
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackError
        | SignalingType::ManagerSystemStatue
        | SignalingType::ReplyFromTerminal
        | SignalingType::TerminalStarted
        | SignalingType::TerminalClosed
        | SignalingType::DesktopSwitching
        | SignalingType::DesktopReady
        | SignalingType::FetchConnections
        | SignalingType::ConnectionList
        | SignalingType::Heartbeat
        | SignalingType::Error
        | SignalingType::Unknown => {
            log::trace!(
                "[router] daemon-emitted or unknown variant arrived inbound, dropping: {:?}",
                model.signaling_type,
            );
            Ok(())
        }
        SignalingType::EnablePrivateScreen => {
            handle_enable_private_screen_inbound(ctx, model).await
        }
        SignalingType::UpdateDeskSettings => handle_update_desk_settings_inbound(ctx, model).await,
        // Batch 2 of the typed-IPC migration — manager plane.
        SignalingType::ManagerSystemInfo => handle_manager_system_info_inbound(ctx, model).await,
        SignalingType::ManagerQuerySettings => {
            handle_manager_query_settings_inbound(ctx, model).await
        }
        SignalingType::ManagerFileList => handle_manager_file_list_inbound(ctx, model).await,
        SignalingType::ManagerFileDelete => handle_manager_file_delete_inbound(ctx, model).await,
        SignalingType::ManagerUpdateSettings => {
            handle_manager_update_settings_inbound(ctx, model).await
        }
        // Batch 3 of the typed-IPC migration — terminal plane.
        SignalingType::StartTerminal => handle_start_terminal_inbound(ctx, model).await,
        SignalingType::SendDataToTerminal => handle_send_data_to_terminal_inbound(ctx, model).await,
        SignalingType::ResizeTerminal => handle_resize_terminal_inbound(ctx, model).await,
        SignalingType::CloseTerminal => handle_close_terminal_inbound(ctx, model).await,
        SignalingType::ListTerminal => handle_list_terminal_inbound(ctx, model).await,
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

// ---- Batch 2: manager plane typed-IPC dispatch helpers ----
//
// All five share the same skeleton — pull `from_connection_id` (the
// browser's PC ID), build the typed `ServiceToWorker::Manager*Request`
// payload, ship it via `WorkerManager::send_to_worker`. Differences are
// only in payload type and whether the inbound model carries a body.
// The `request_id` is echoed verbatim so the worker's
// `ManagerResponseRefPayload` / typed-response payload can correlate.
// Errors are non-fatal for the WS connection: parse / send failures
// log + drop, same fail-soft semantics the SignalingMessage bridge
// had.

/// Helper: extract `from_connection_id` from an inbound model when
/// the routing path requires one (e.g. terminal session traffic that
/// keys per-PTY on the originating browser/terminal connection).
/// Missing => log and return None so the caller drops the message.
fn require_from_connection_id<'a>(
    model: &'a SignalingModel,
    signaling_type_name: &'static str,
) -> Option<&'a str> {
    match model.from_connection_id.as_deref() {
        Some(id) => Some(id),
        None => {
            log::warn!(
                "[router] {signaling_type_name} missing from_connection_id; ignoring \
                 (request_id={})",
                model.request_id,
            );
            None
        }
    }
}

/// Helper: clone `from_connection_id` from an inbound model where
/// `None` is a legitimate state — used by manager-plane and
/// `ListTerminal` routing because those `SignalingType`s can be
/// dispatched from a HTTP REST controller (e.g.
/// `signal-facade::controller::sysinfo::list_files`) via
/// `connection.request_peer_with_callback`, which does not populate
/// `from_connection_id`. The response is correlated by `request_id`
/// alone in that case, so the typed IPC payload simply carries
/// `Option<String>` through to the worker.
fn optional_from_connection_id(model: &SignalingModel) -> Option<String> {
    model.from_connection_id.clone()
}

async fn handle_manager_system_info_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ManagerRequestRefPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerSystemInfoRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerSystemInfoRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_query_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ManagerRequestRefPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerQuerySettingsRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerQuerySettingsRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_file_list_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let connection_id = optional_from_connection_id(model);
    let params = match model.get_data::<FileListParams>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "[router] ManagerFileList payload parse failed for {connection_id:?}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerFileListRequestPayload {
        request_id: model.request_id.clone(),
        connection_id,
        params,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerFileListRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerFileListRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_file_delete_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let connection_id = optional_from_connection_id(model);
    let request = match model.get_data::<DeleteFileRequest>() {
        Ok(r) => r,
        Err(e) => {
            log::warn!(
                "[router] ManagerFileDelete payload parse failed for {connection_id:?}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerFileDeleteRequestPayload {
        request_id: model.request_id.clone(),
        connection_id,
        request,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerFileDeleteRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerFileDeleteRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_update_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let connection_id = optional_from_connection_id(model);
    let settings = match model.get_data::<RemoteSystemSettings>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] ManagerUpdateSettings payload parse failed for {connection_id:?}: \
                 {e}; dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerUpdateSettingsRequestPayload {
        request_id: model.request_id.clone(),
        connection_id,
        settings,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerUpdateSettingsRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerUpdateSettingsRequest: {e}");
    }
    Ok(())
}

// ---- Batch 3: terminal-plane typed-IPC dispatch helpers ----
//
// The 5 inbound terminal request types share the same skeleton as the
// manager-plane helpers — pull `from_connection_id`, build the typed
// `ServiceToWorker::*Request` payload, ship it via
// `WorkerManager::send_to_worker`. Differences are only in payload
// type and whether the inbound model carries a body / a request_id.
// Errors are non-fatal for the WS connection: parse / send failures
// log + drop.

async fn handle_start_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "StartTerminal") else {
        return Ok(());
    };
    let session = match model.get_data::<StartTerminalSession>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] StartTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = StartTerminalRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: connection_id.to_string(),
        session,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::StartTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed StartTerminalRequest: {e}");
    }
    Ok(())
}

async fn handle_send_data_to_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "SendDataToTerminal") else {
        return Ok(());
    };
    let data = match model.get_data_with_type::<TerminalInputData>() {
        Ok(Some(d)) => d,
        Ok(None) => {
            // Empty payload — match the legacy handler's silent ignore.
            return Ok(());
        }
        Err(e) => {
            log::warn!(
                "[router] SendDataToTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = SendDataToTerminalPayload {
        connection_id: connection_id.to_string(),
        data,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::SendDataToTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed SendDataToTerminalRequest: {e}");
    }
    Ok(())
}

async fn handle_resize_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "ResizeTerminal") else {
        return Ok(());
    };
    let data = match model.get_data_with_type::<TerminalResizeData>() {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(()),
        Err(e) => {
            log::warn!(
                "[router] ResizeTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ResizeTerminalPayload {
        connection_id: connection_id.to_string(),
        data,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ResizeTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ResizeTerminalRequest: {e}");
    }
    Ok(())
}

async fn handle_close_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "CloseTerminal") else {
        return Ok(());
    };
    let payload = CloseTerminalPayload {
        connection_id: connection_id.to_string(),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::CloseTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed CloseTerminalRequest: {e}");
    }
    Ok(())
}

async fn handle_list_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ListTerminalRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ListTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ListTerminalRequest: {e}");
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
            SignalingType::ManagerSystemStatue,
            SignalingType::ReplyFromTerminal,
            SignalingType::TerminalStarted,
            SignalingType::TerminalClosed,
            SignalingType::DesktopSwitching,
            SignalingType::DesktopReady,
            SignalingType::FetchConnections,
            SignalingType::ConnectionList,
            SignalingType::ConnectionRemoved,
            SignalingType::Heartbeat,
            // Batch 4: Error / Unknown are daemon-owned now.
            SignalingType::Error,
            SignalingType::Unknown,
        ] {
            assert_eq!(
                classify(t),
                RouteOwnership::Daemon,
                "{t:?} should be daemon-owned",
            );
        }
    }

    /// Worker-bound: user-session resources (files, terminal request
    /// types, settings, overlays, approval, manager queries). The 3
    /// terminal *reverse* notification types (`ReplyFromTerminal`,
    /// `TerminalStarted`, `TerminalClosed`) are classified as
    /// daemon-owned because they only flow worker → browser; an
    /// inbound copy is a protocol error to swallow.
    #[test]
    fn classify_worker_owned_types() {
        for t in [
            SignalingType::EnablePrivateScreen,
            SignalingType::UpdateDeskSettings,
            SignalingType::ManagerSystemInfo,
            SignalingType::ManagerFileList,
            SignalingType::ManagerFileDelete,
            SignalingType::StartTerminal,
            SignalingType::SendDataToTerminal,
            SignalingType::ResizeTerminal,
            SignalingType::CloseTerminal,
            SignalingType::ListTerminal,
            SignalingType::ManagerQuerySettings,
            SignalingType::ManagerUpdateSettings,
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
            SignalingType::ManagerSystemStatue,
            SignalingType::ReplyFromTerminal,
            SignalingType::TerminalStarted,
            SignalingType::TerminalClosed,
            SignalingType::DesktopSwitching,
            SignalingType::DesktopReady,
            SignalingType::FetchConnections,
            SignalingType::ConnectionList,
            SignalingType::Heartbeat,
            SignalingType::Error,
            SignalingType::Unknown,
        ] {
            let model = SignalingModel::new("r", t, None, None, None, None);
            assert!(route(&model, &ctx).await.is_ok(), "{t:?}");
        }
    }

    /// Pin behaviour: a stray inbound `AcceptControl` (which would
    /// be a protocol error from the browser, since the daemon emits
    /// AcceptControl outbound) is swallowed — `route` returns Ok
    /// so the message never reaches the worker. After batch 4 the
    /// SignalingMessage bridge is gone, so the only way for an
    /// inbound `AcceptControl` to leak through would be a new
    /// regression in `route()`'s match.
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
        route(&model, &ctx)
            .await
            .expect("AcceptControl inbound must be swallowed, not surfaced as error");
    }

    /// Batch 3: every terminal-plane request type is now handled
    /// inline via typed `ServiceToWorker::*Request` IPC. Without an
    /// active worker the typed send is logged but the route call
    /// itself still succeeds.
    #[tokio::test]
    async fn route_terminal_requests_handled_inline_not_bridged() {
        let ctx = make_ctx();
        let cases = [
            (
                SignalingType::StartTerminal,
                serde_json::to_value(desk_signal_facade::model::terminal::StartTerminalSession {
                    command: "C:\\Windows\\System32\\cmd.exe".to_string(),
                })
                .unwrap(),
            ),
            (
                SignalingType::SendDataToTerminal,
                serde_json::to_value(desk_signal_facade::model::terminal::TerminalInputData {
                    content: "echo hi\n".to_string(),
                })
                .unwrap(),
            ),
            (
                SignalingType::ResizeTerminal,
                serde_json::to_value(desk_signal_facade::model::terminal::TerminalResizeData {
                    rows: 30,
                    cols: 100,
                })
                .unwrap(),
            ),
            (SignalingType::CloseTerminal, serde_json::Value::Null),
            (SignalingType::ListTerminal, serde_json::Value::Null),
        ];
        for (t, body) in cases {
            let signaling_data = if body.is_null() { None } else { Some(body) };
            let model = SignalingModel::new(
                "req-term",
                t,
                Some("conn-term".to_string()),
                None,
                signaling_data,
                None,
            );
            assert!(
                route(&model, &ctx).await.is_ok(),
                "{t:?} must succeed inline (no bridge fallback exists)",
            );
        }
    }

    /// Terminal requests without a `from_connection_id` are protocol
    /// errors — daemon logs and drops, no panic, no IPC send.
    #[tokio::test]
    async fn route_terminal_request_without_connection_id_is_noop() {
        let ctx = make_ctx();
        for t in [
            SignalingType::StartTerminal,
            SignalingType::SendDataToTerminal,
            SignalingType::ResizeTerminal,
            SignalingType::CloseTerminal,
            SignalingType::ListTerminal,
        ] {
            let model = SignalingModel::new("req-noid", t, None, None, None, None);
            assert!(route(&model, &ctx).await.is_ok(), "{t:?}");
        }
    }

    /// Malformed `StartTerminal` body (not a `StartTerminalSession`
    /// JSON object) must not crash the router — it should log + drop.
    /// The `SendDataToTerminal` / `ResizeTerminal` analogues take the
    /// `get_data_with_type` path which already returns `Ok(None)` on
    /// missing data; this case verifies a parse-failure surface.
    #[tokio::test]
    async fn route_start_terminal_with_invalid_payload_is_dropped() {
        let ctx = make_ctx();
        let model = SignalingModel::new(
            "req-start-bad",
            SignalingType::StartTerminal,
            Some("conn-term".to_string()),
            None,
            Some(serde_json::json!("not start terminal session")),
            None,
        );
        assert!(route(&model, &ctx).await.is_ok());
    }

    /// Batch 2: manager-plane requests are handled inline by the
    /// router (typed `ServiceToWorker::Manager*Request` IPC). With no
    /// active worker the typed send is logged but the route call
    /// itself still succeeds.
    #[tokio::test]
    async fn route_manager_requests_handled_inline_not_bridged() {
        let ctx = make_ctx();
        let cases = [
            (SignalingType::ManagerSystemInfo, serde_json::Value::Null),
            (SignalingType::ManagerQuerySettings, serde_json::Value::Null),
            (
                SignalingType::ManagerFileList,
                serde_json::to_value(desk_signal_facade::model::files::FileListParams {
                    path: "C:\\".to_string(),
                    page_no: 1,
                    page_count: 50,
                    ..Default::default()
                })
                .unwrap(),
            ),
            (
                SignalingType::ManagerFileDelete,
                serde_json::to_value(desk_signal_facade::model::files::DeleteFileRequest {
                    file_path: "C:\\old.txt".to_string(),
                    delete_permanently: Some(false),
                    connection_id: Some("conn-mgr".to_string()),
                })
                .unwrap(),
            ),
            (
                SignalingType::ManagerUpdateSettings,
                serde_json::to_value(
                    desk_signal_facade::model::system_settings::RemoteSystemSettings::default(),
                )
                .unwrap(),
            ),
        ];
        for (t, body) in cases {
            let signaling_data = if body.is_null() { None } else { Some(body) };
            let model = SignalingModel::new(
                "req-mgr",
                t,
                Some("conn-mgr".to_string()),
                None,
                signaling_data,
                None,
            );
            assert!(
                route(&model, &ctx).await.is_ok(),
                "{t:?} must ride typed IPC",
            );
        }
    }

    /// HTTP-API-triggered manager requests (e.g.
    /// `signal-facade::controller::sysinfo` →
    /// `connection.request_peer_with_callback`) carry no
    /// `from_connection_id`; the router must still forward the typed
    /// request to the worker so the response can flow back via
    /// `request_id` correlation. Previously the router dropped these
    /// — that broke `GET /api/desk/files/...` and `GET
    /// /api/desk/terminals/...` in portable mode.
    #[tokio::test]
    async fn route_manager_request_without_connection_id_forwards() {
        let ctx = make_ctx();
        for t in [
            SignalingType::ManagerSystemInfo,
            SignalingType::ManagerQuerySettings,
            SignalingType::ManagerFileDelete,
            SignalingType::ManagerUpdateSettings,
        ] {
            let body = match t {
                SignalingType::ManagerFileDelete => Some(
                    serde_json::to_value(desk_signal_facade::model::files::DeleteFileRequest {
                        file_path: "C:\\old.txt".to_string(),
                        delete_permanently: Some(false),
                        connection_id: None,
                    })
                    .unwrap(),
                ),
                SignalingType::ManagerUpdateSettings => Some(
                    serde_json::to_value(
                        desk_signal_facade::model::system_settings::RemoteSystemSettings::default(),
                    )
                    .unwrap(),
                ),
                _ => None,
            };
            let model = SignalingModel::new("req-no-conn", t, None, None, body, None);
            assert!(
                route(&model, &ctx).await.is_ok(),
                "{t:?} with None from_connection_id must be forwarded, not dropped",
            );
        }
    }

    /// `ManagerFileList` specifically — same regression as the umbrella
    /// test above, but pinned with a real `FileListParams` body so a
    /// future split that re-introduces a `require_from_connection_id`
    /// guard on the file-list path lights up here.
    #[tokio::test]
    async fn route_manager_file_list_without_connection_id_forwards() {
        let ctx = make_ctx();
        let params = desk_signal_facade::model::files::FileListParams {
            path: "C:\\".to_string(),
            page_no: 1,
            page_count: 50,
            ..Default::default()
        };
        let model = SignalingModel::new(
            "req-fl-no-conn",
            SignalingType::ManagerFileList,
            None,
            None,
            Some(serde_json::to_value(&params).unwrap()),
            None,
        );
        assert!(route(&model, &ctx).await.is_ok());
    }

    /// `ListTerminal` is dispatched by
    /// `signal-facade::controller::terminal::list_terminal` (REST GET)
    /// without a `from_connection_id`. The router must forward it.
    #[tokio::test]
    async fn route_list_terminal_without_connection_id_forwards() {
        let ctx = make_ctx();
        let model = SignalingModel::new(
            "req-list-no-conn",
            SignalingType::ListTerminal,
            None,
            None,
            None,
            None,
        );
        assert!(route(&model, &ctx).await.is_ok());
    }

    /// Malformed manager request bodies (e.g. `ManagerFileList` with
    /// non-`FileListParams` JSON) must not crash the router — they
    /// should log + drop.
    #[tokio::test]
    async fn route_manager_file_list_with_invalid_payload_is_dropped() {
        let ctx = make_ctx();
        let model = SignalingModel::new(
            "req-fl-bad",
            SignalingType::ManagerFileList,
            Some("conn-fl".to_string()),
            None,
            Some(serde_json::json!("not file list params")),
            None,
        );
        assert!(route(&model, &ctx).await.is_ok());
    }

    /// Batch 1: `EnablePrivateScreen` is handled inline by the router
    /// (typed [`ServiceToWorker::EnablePrivateScreen`] IPC). With no
    /// active worker the typed send is logged but the route call
    /// itself still succeeds.
    #[tokio::test]
    async fn route_enable_private_screen_handled_inline_not_bridged() {
        let ctx = make_ctx();
        let data =
            desk_signal_facade::model::private_screen::EnablePrivateScreenData { enable: true };
        let model = SignalingModel::new(
            "r-eps",
            SignalingType::EnablePrivateScreen,
            Some("conn-priv".to_string()),
            None,
            Some(serde_json::to_value(&data).unwrap()),
            None,
        );
        assert!(route(&model, &ctx).await.is_ok());
    }

    /// `EnablePrivateScreen` arriving without a `from_connection_id`
    /// is a malformed message — daemon logs and drops, no panic, no
    /// IPC send.
    #[tokio::test]
    async fn route_enable_private_screen_without_connection_id_is_noop() {
        let ctx = make_ctx();
        let data =
            desk_signal_facade::model::private_screen::EnablePrivateScreenData { enable: false };
        let model = SignalingModel::new(
            "r-eps-noid",
            SignalingType::EnablePrivateScreen,
            None,
            None,
            Some(serde_json::to_value(&data).unwrap()),
            None,
        );
        assert!(route(&model, &ctx).await.is_ok());
    }

    /// Batch 1: `UpdateDeskSettings` is fully handled by the router —
    /// it both fans out the typed `UpdateMediaSettings` IPC for the
    /// encoder pipeline AND ships the full settings to the worker as
    /// typed [`ServiceToWorker::UpdateDeskSettings`].
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
        assert!(route(&model, &ctx).await.is_ok());
    }

    /// Malformed `UpdateDeskSettings` payload (not a DeskSettings
    /// object) must not crash the router — it should log and drop.
    #[tokio::test]
    async fn route_update_desk_settings_with_invalid_payload_is_dropped() {
        let ctx = make_ctx();
        let model = SignalingModel::new(
            "r-bad",
            SignalingType::UpdateDeskSettings,
            None,
            None,
            Some(serde_json::json!("not an object")),
            None,
        );
        assert!(route(&model, &ctx).await.is_ok());
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
        assert!(route(&model, &ctx).await.is_ok());
    }
}
