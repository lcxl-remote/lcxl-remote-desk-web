//! Worker-side signaling glue.
//!
//! The WebRTC PeerConnection no longer lives in this module —
//! `daemon::pc_manager` owns the PC lifecycle and `daemon::signaling_router`
//! owns inbound dispatch. What remains here is the worker-side
//! [`DeskSession`] used by the worker IPC loop to dispatch typed-IPC
//! requests for worker-owned `SignalingType`s (private screen, terminal,
//! manager file/system queries, settings) plus a small number of helpers
//! (`should_short_circuit_*`, `resolve_mdns_host`, `LocalNodeTokenValidator`)
//! that are still consumed by daemon code.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use actix_web::web;
use bytes::Bytes;
use bytestring::ByteString;
use desk_ipc_protocol::message::{FileManagerOpenedPayload, WorkerToService};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::error::DeskSignalFacadeError;
use desk_signal_facade::model::private_screen::{
    EnablePrivateScreenData, PrivateScreenStateChangedData,
};
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{PeerSignalingSender, SignalingModel, SignalingType};

use crate::worker::connection_ceiling::ConnectionCeilingStore;
use desk_utils::error::{CustomDeskError, DeskErrorCode};

use log::{error, info};
use once_cell::sync::OnceCell;
use serde::Serialize;
use std::net::IpAddr;
use tokio::sync::mpsc;

use webrtc_mdns::{config::Config as MdnsConfig, conn::DnsConn};

use crate::host_control::HostControlHub;
use crate::model::policy_access::{CachedDecision, PolicyAccess};
use crate::model::security_approval::{
    SecurityPermissionType, check_security_permission, resolve_permission,
};
use crate::service::file_manager::{handle_manager_file_delete, handle_manager_file_list};
use crate::service::terminal::{
    RunningTerminal, force_kill_terminal_process, handle_list_terminals,
    handle_manager_terminal_close, handle_manager_terminal_data, handle_manager_terminal_resize,
    handle_manager_terminal_start,
};
use crate::{error::DeskError, model::settings::SharedSettings};
use desk_input_injection::host_control::host_control_factory::create_host_control_helper;
use desk_input_injection::model::host_control::{HostControlHelper, WhiteboardCommand};

/// Outbound channel message produced by [`DeskSession`] for the
/// surrounding IPC loop. Only `Text` carries real signaling traffic
/// now; the other variants are kept so the worker session match
/// stays exhaustive without forcing a wire-protocol change.
#[derive(Debug)]
pub enum DeskSessionMessage {
    Text(ByteString),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close,
}

#[derive(Clone)]
pub struct DeskSessionSender {
    pub sender: mpsc::UnboundedSender<DeskSessionMessage>,
}

impl PeerSignalingSender for DeskSessionSender {
    async fn send_response<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        signaling_data: &T,
    ) -> Result<(), DeskSignalFacadeError>
    where
        T: ?Sized + Serialize + Sync,
    {
        let signaling_model = SignalingModel::success_response(
            request_id,
            signaling_type,
            None,
            to_connection_id,
            Some(signaling_data),
        )?;
        let text = serde_json::to_string(&signaling_model)?;
        self.sender
            .send(DeskSessionMessage::Text(ByteString::from(text)))
            .map_err(|e| {
                DeskSignalFacadeError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to send signaling message: {}", e),
                ))
            })?;
        Ok(())
    }

    async fn send_error(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        error_code: DeskErrorCode,
        error_message: &str,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::error(
            request_id,
            signaling_type,
            None,
            to_connection_id,
            error_code,
            error_message,
        )?;
        let text = serde_json::to_string(&signaling_model)?;
        self.sender
            .send(DeskSessionMessage::Text(ByteString::from(text)))
            .map_err(|e| {
                DeskSignalFacadeError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to send error message: {}", e),
                ))
            })?;
        Ok(())
    }

    async fn send_to_peer<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: &str,
        data: T,
    ) -> Result<(), DeskSignalFacadeError>
    where
        T: Serialize + Sync + Send,
    {
        self.send_response(
            request_id,
            signaling_type,
            Some(to_connection_id.to_owned()),
            &data,
        )
        .await
    }
}

use desk_signal_facade::service::NodeTokenValidator;

pub struct LocalNodeTokenValidator {
    pub settings: web::Data<SharedSettings>,
}

impl NodeTokenValidator for LocalNodeTokenValidator {
    fn validate_node_token<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        let token = token.to_string();
        let settings = self.settings.clone();
        Box::pin(async move {
            // Reject empty tokens immediately
            if token.is_empty() {
                return false;
            }
            let local_signaling_token = settings.read().await.system.local_signaling_token.clone();
            if let Some(local_token) = local_signaling_token
                && !local_token.is_empty()
            {
                return crate::constant_time_eq(local_token.as_bytes(), token.as_bytes());
            }
            false
        })
    }
}

static MDNS_CONN: OnceCell<std::sync::Arc<DnsConn>> = OnceCell::new();

async fn get_mdns_conn() -> Result<std::sync::Arc<DnsConn>, webrtc_mdns::Error> {
    if let Some(conn) = MDNS_CONN.get() {
        return Ok(conn.clone());
    }

    let mut cfg = MdnsConfig::default();
    cfg.query_interval = Duration::from_millis(200);

    // Bind to an ephemeral port to avoid conflicts with any existing mDNS listener.
    let conn = DnsConn::server("0.0.0.0:0".parse().expect("valid mdns bind"), cfg)?;
    let conn = std::sync::Arc::new(conn);
    let _ = MDNS_CONN.set(conn.clone());
    Ok(conn)
}

pub(crate) async fn resolve_mdns_host(host: &str) -> Option<IpAddr> {
    let conn = get_mdns_conn().await.ok()?;
    let (_close_tx, close_rx) = mpsc::channel(1);

    // Timeout quickly to avoid blocking ICE too long.
    match tokio::time::timeout(Duration::from_millis(800), conn.query(host, close_rx)).await {
        Ok(Ok((_answer, addr))) => Some(addr.ip()),
        Ok(Err(e)) => {
            log::warn!("mDNS query failed for {}: {:?}", host, e);
            None
        }
        Err(_) => {
            log::warn!("mDNS query timed out for {}", host);
            None
        }
    }
}

/// Worker-side signaling dispatcher.
///
/// Used by the worker IPC loop ([`crate::worker::session::WorkerSession`])
/// to dispatch typed-IPC requests forwarded by the daemon for
/// worker-owned `SignalingType`s. The PeerConnection, SDP/ICE handling,
/// `RequireControl` / `CloseControl` and all media capture live in
/// `daemon::pc_manager` instead — see that module for the daemon-side
/// counterpart.
pub struct DeskSession {
    pub settings: web::Data<SharedSettings>,
    pub session: DeskSessionSender,
    pub user: CurrentUser,
    /// Terminal map: from_connection_id -> RunningTerminal
    pub terminal_map: HashMap<String, RunningTerminal>,
    /// System setting helper
    pub host_control_helper: Box<dyn HostControlHelper + Send + Sync>,
    /// Whiteboard command sender bridged onto the host control hub. Always
    /// present because the host-control bridge is always configured — the bridge thread silently drops
    /// messages when no Tauri client is connected.
    pub whiteboard_cmd_sender: std::sync::mpsc::Sender<WhiteboardCommand>,
    /// Unified host-control hub for approval prompts and overlay commands.
    /// All approval flow + private-screen / whiteboard / service-op traffic
    /// routes through here.
    pub host_control_hub: Arc<HostControlHub>,
    /// Per-connection capability ceilings registered by the daemon for
    /// redeemed-grant sessions. The worker-side permission gates meet a
    /// connection's ceiling with the host global so a grant can only be tightened;
    /// owner sessions carry no ceiling and use the global verbatim. Shared (Arc)
    /// with the worker session loop that populates it on `SetConnectionCeiling`.
    pub connection_ceilings: ConnectionCeilingStore,
    /// The host security policy as this process reaches it. In a worker that is
    /// the daemon's published copy, never the settings clone this session also
    /// holds.
    pub policy: Arc<PolicyAccess>,
    /// Cached file-browse decisions keyed by controller connection, each tagged
    /// with the policy it was decided under.
    pub file_browse_permissions: HashMap<String, CachedDecision>,
    /// Cached file-delete decisions keyed by controller connection, tagged the
    /// same way.
    pub file_delete_permissions: HashMap<String, CachedDecision>,
    /// Connections for which FileManagerOpened was already emitted.
    pub opened_file_managers: HashSet<String>,
    /// Direct worker event lane used for authoritative file activity facts.
    pub worker_event_sender: mpsc::UnboundedSender<WorkerToService>,
}

impl DeskSession {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        settings: web::Data<SharedSettings>,
        session: DeskSessionSender,
        user: CurrentUser,
        host_control_hub: Arc<HostControlHub>,
        connection_ceilings: ConnectionCeilingStore,
        policy: Arc<PolicyAccess>,
        worker_event_sender: mpsc::UnboundedSender<WorkerToService>,
    ) -> Result<Self, DeskError> {
        let desk_settings = settings.read().await.clone().desk;

        // Bridge senders adapt the legacy `desk_input_injection` mpsc API onto
        // the unified host control hub.
        let ps_cmd_sender = crate::host_control::bridge::bridge_private_screen_to_hub(Arc::clone(
            &host_control_hub,
        ));
        let helper = create_host_control_helper(&desk_settings, Some(ps_cmd_sender))?;

        let whiteboard_cmd_sender =
            crate::host_control::bridge::bridge_whiteboard_to_hub(Arc::clone(&host_control_hub));

        // Forward private-screen visibility changes from the Tauri shell back into
        // the WebRTC signaling stream as `PrivateScreenStateChanged` messages.
        // The hub's state broadcast is the single source of truth across all
        // deployment modes (Local / Forwarder).
        let mut state_rx = host_control_hub.subscribe_state();
        {
            let session_clone = session.clone();
            tokio::spawn(async move {
                use crate::host_control::HostControlEvent;
                use tokio::sync::broadcast::error::RecvError;
                loop {
                    match state_rx.recv().await {
                        Ok(HostControlEvent::PrivateScreenVisibilityChanged {
                            connection_id,
                            visible,
                        }) => {
                            let data = PrivateScreenStateChangedData {
                                visible,
                                is_supported: true,
                                error_msg: None,
                            };
                            if let Ok(model) = SignalingModel::new_request(
                                SignalingType::PrivateScreenStateChanged,
                                Some(connection_id),
                                Some(&data),
                            ) && let Ok(text) = serde_json::to_string(&model)
                            {
                                let _ = session_clone.sender.send(DeskSessionMessage::Text(
                                    bytestring::ByteString::from(text),
                                ));
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            log::warn!("[DeskSession] state subscription lagged by {n}");
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            });
        }

        Ok(Self {
            settings,
            session,
            user,
            terminal_map: HashMap::new(),
            host_control_helper: helper,
            whiteboard_cmd_sender,
            host_control_hub,
            connection_ceilings,
            policy,
            file_browse_permissions: HashMap::new(),
            file_delete_permissions: HashMap::new(),
            opened_file_managers: HashSet::new(),
            worker_event_sender,
        })
    }

    /// The effective permission for `connection_id` on one capability dimension:
    /// the connection's grant ceiling met with the host `global` (an owner session
    /// carries no ceiling and uses the global verbatim). Worker-side gates call
    /// this instead of reading the global directly, so a redeemed-grant session is
    /// capped by its ceiling.
    pub async fn effective_permission(
        &self,
        connection_id: &str,
        global: Option<bool>,
        dim: impl Fn(&SecuritySettings) -> Option<bool>,
    ) -> Option<bool> {
        let ceiling = self.connection_ceilings.get(connection_id).await;
        crate::model::security_approval::effective_permission(ceiling.as_ref(), global, dim)
    }

    /// Resolve and cache FileBrowse for one controller connection.
    pub async fn file_browse_permission(&mut self, connection_id: &str) -> bool {
        let capability = SecurityPermissionType::FileBrowse;
        let generation = self.policy.changed_at(capability);
        let ceiling = self.connection_ceilings.get(connection_id).await;
        let effective = crate::model::security_approval::effective_permission(
            ceiling.as_ref(),
            self.policy.permission(capability),
            |settings| settings.allow_file_browse,
        );
        if effective == Some(false) {
            self.file_browse_permissions.remove(connection_id);
            return false;
        }
        // Only approvals are cached: a denial is the user declining this one
        // request, and the next one asks again.
        if let Some(cached) = self.file_browse_permissions.get(connection_id)
            && cached.is_current(generation)
        {
            return cached.approved;
        }
        let resolved = resolve_permission(
            &self.policy,
            &self.host_control_hub,
            effective,
            capability,
            Some(connection_id.to_string()),
            ceiling.is_some(),
        )
        .await;
        if let Some(decided_at) = resolved.cacheable_at
            && resolved.approved
        {
            self.file_browse_permissions.insert(
                connection_id.to_string(),
                CachedDecision {
                    approved: true,
                    decided_at,
                },
            );
        }
        resolved.approved
    }

    /// Resolve and cache FileDelete for one controller connection.
    pub async fn file_delete_permission(&mut self, connection_id: &str) -> bool {
        let capability = SecurityPermissionType::FileDelete;
        let generation = self.policy.changed_at(capability);
        let ceiling = self.connection_ceilings.get(connection_id).await;
        let effective = crate::model::security_approval::effective_permission(
            ceiling.as_ref(),
            self.policy.permission(capability),
            |settings| settings.allow_file_delete,
        );
        if effective == Some(false) {
            self.file_delete_permissions.remove(connection_id);
            return false;
        }
        if let Some(cached) = self.file_delete_permissions.get(connection_id)
            && cached.is_current(generation)
        {
            return cached.approved;
        }
        let resolved = resolve_permission(
            &self.policy,
            &self.host_control_hub,
            effective,
            capability,
            Some(connection_id.to_string()),
            ceiling.is_some(),
        )
        .await;
        if let Some(decided_at) = resolved.cacheable_at
            && resolved.approved
        {
            self.file_delete_permissions.insert(
                connection_id.to_string(),
                CachedDecision {
                    approved: true,
                    decided_at,
                },
            );
        }
        resolved.approved
    }

    /// Emit the authoritative open fact once per controller connection.
    pub fn mark_file_manager_opened(&mut self, connection_id: &str) {
        if self.opened_file_managers.insert(connection_id.to_string()) {
            let _ = self
                .worker_event_sender
                .send(WorkerToService::FileManagerOpened(
                    FileManagerOpenedPayload {
                        connection_id: connection_id.to_string(),
                    },
                ));
        }
    }

    /// Clear all connection-scoped file permissions and lifecycle state.
    pub fn clear_file_permissions(&mut self, connection_id: &str) {
        self.file_browse_permissions.remove(connection_id);
        self.file_delete_permissions.remove(connection_id);
        self.opened_file_managers.remove(connection_id);
    }

    /// Shutdown the session. Only terminals are owned by the
    /// worker-side `DeskSession`; PeerConnections are managed by
    /// `daemon::pc_manager` and their teardown happens through
    /// `StopMedia` / `daemon::worker_manager` instead.
    pub async fn shutdown(self) -> Result<(), DeskError> {
        for terminal in self.terminal_map.into_values() {
            let child_arc = terminal.child.clone();
            drop(terminal);
            if let Ok(mut child) = child_arc.lock() {
                if let Some(pid) = child.process_id() {
                    force_kill_terminal_process(pid);
                }
                let result = child.kill();
                info!("Terminal session ended, result={:?}", result);
            }
        }
        Ok(())
    }

    /// Cancel all worker-owned remote activity while keeping the session object
    /// reusable for a later authenticated unlock.
    pub async fn cancel_all_remote_activity(&mut self) -> u32 {
        let terminals = std::mem::take(&mut self.terminal_map);
        let cancelled = terminals.len().min(u32::MAX as usize) as u32;
        for terminal in terminals.into_values() {
            let child_arc = terminal.child.clone();
            drop(terminal);
            if let Ok(mut child) = child_arc.lock() {
                if let Some(pid) = child.process_id() {
                    force_kill_terminal_process(pid);
                }
                let _ = child.kill();
            }
        }
        self.file_browse_permissions.clear();
        self.file_delete_permissions.clear();
        self.opened_file_managers.clear();
        cancelled
    }

    /// Dispatch a worker-owned signaling message produced by the daemon's
    /// typed-IPC fan-out. Daemon-owned types (`RequestRemote`, `Offer`,
    /// `Answer`, `Canid`, `RequireControl`, `CloseControl`,
    /// `AcceptControl`, `DenyControl`, `AudioPlaybackError`) never reach
    /// this dispatcher — `daemon::signaling_router` handles
    /// them inline against the daemon-held PC and never forwards them
    /// through the worker IPC loop.
    pub async fn handle_message(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        match signaling_model.signaling_type {
            SignalingType::UpdateDeskSettings => {
                // Encoder fps / quality live-apply runs on the
                // daemon side via `UpdateMediaSettings`; the worker
                // `DeskSession` no longer owns capture / encode state
                // and has nothing to do with the rest of the
                // `DeskSettings` payload. Keeping the typed dispatch
                // path so future stateful worker-side handling has a
                // ready hook.
                let _ = signaling_model;
            }
            SignalingType::EnablePrivateScreen => {
                let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
                if let Some(data) =
                    signaling_model.get_data_with_type::<EnablePrivateScreenData>()?
                {
                    if data.enable {
                        let global_private_screen = self
                            .policy
                            .permission(SecurityPermissionType::PrivateScreen);
                        let allow_private_screen = self
                            .effective_permission(from_connection_id, global_private_screen, |c| {
                                c.allow_private_screen
                            })
                            .await;
                        // A capped grant / code-session (it has a per-connection
                        // ceiling) must not persist its prompt to the host global.
                        let suppress_remember = self
                            .connection_ceilings
                            .get(from_connection_id)
                            .await
                            .is_some();
                        let approved = check_security_permission(
                            &self.policy,
                            &self.host_control_hub,
                            allow_private_screen,
                            SecurityPermissionType::PrivateScreen,
                            Some(from_connection_id.to_string()),
                            suppress_remember,
                        )
                        .await;

                        if !approved {
                            log::warn!(
                                "Enable private screen denied by security settings or user for {}",
                                from_connection_id
                            );
                            self.session
                                .send_error(
                                    &signaling_model.request_id,
                                    signaling_model.signaling_type,
                                    Some(from_connection_id.to_string()),
                                    DeskErrorCode::PERMISSION_ERROR,
                                    "Private screen access denied",
                                )
                                .await?;
                            return Ok(());
                        }
                    }

                    let _ = self
                        .host_control_helper
                        .enable_private_screen(from_connection_id, data.enable);
                }
            }
            SignalingType::ManagerFileList => {
                handle_manager_file_list(self, signaling_model).await?;
            }
            SignalingType::ManagerFileDelete => {
                handle_manager_file_delete(self, signaling_model).await?;
            }
            SignalingType::StartTerminal => {
                handle_manager_terminal_start(self, signaling_model).await?;
            }
            SignalingType::SendDataToTerminal => {
                handle_manager_terminal_data(self, signaling_model).await?;
            }
            SignalingType::ResizeTerminal => {
                handle_manager_terminal_resize(self, signaling_model).await?;
            }
            SignalingType::CloseTerminal => {
                handle_manager_terminal_close(self, signaling_model).await?;
            }
            SignalingType::ListTerminal => {
                handle_list_terminals(self, signaling_model).await?;
            }
            SignalingType::ManagerSystemInfo => {
                let mut sys = sysinfo::System::new_all();
                sys.refresh_all();
                let mut system_info = crate::model::info::SystemInfo::from(&sys);
                let startup_mode = { self.settings.read().await.args.startup_mode.clone() };
                system_info.startup_mode = startup_mode.clone();
                system_info.is_admin =
                    if startup_mode != crate::model::settings::StartupMode::Signaling {
                        Some(desk_utils::permission::is_admin())
                    } else {
                        None
                    };
                let facade_info = system_info.to_facade();
                self.session
                    .send_response(
                        &signaling_model.request_id,
                        SignalingType::ManagerSystemInfo,
                        signaling_model.from_connection_id.clone(),
                        &facade_info,
                    )
                    .await?;
            }
            other => {
                error!("Unknown / unrouted signaling type at worker DeskSession: {other}");
                self.session
                    .send_error(
                        &signaling_model.request_id,
                        other,
                        signaling_model.from_connection_id.clone(),
                        DeskErrorCode::UNKNOWN_SIGNALING_TYPE,
                        &format!("Failed to handle signaling type: {other}"),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}

/// `RequireControl` short-circuit decision for the control permission.
/// Returns `true` only when the browser is asking to GRANT control
/// (`asked == true`) AND control is already approved on the worker side.
///
/// Critically returns `false` for the release path (`asked == false`) so
/// `CloseControl` keeps clearing state: short-circuiting on release would
/// silently turn a "release control" request into a no-op.
pub fn should_short_circuit_control(asked: bool, currently_accepted: bool) -> bool {
    asked && currently_accepted
}

/// `RequireControl` short-circuit decision for the clipboard permission.
/// Independent of control — returns `true` only when the browser is asking
/// for clipboard AND clipboard is already approved on the worker side. We
/// never upgrade clipboard from `false` → `true` via short-circuit alone:
/// clipboard is a separate permission and the user must be re-prompted if
/// it was previously denied.
pub fn should_short_circuit_clipboard(asked: bool, currently_accepted: bool) -> bool {
    asked && currently_accepted
}

#[cfg(test)]
mod sender_tests {
    use super::*;
    use desk_signal_facade::model::signal::SignalingType;

    /// `send_response` should produce a `DeskSessionMessage::Text` whose JSON
    /// body decodes back to a `SignalingModel` with the request_id /
    /// signaling_type / to_connection_id / data we passed in. This is the
    /// reverse path `worker::session::build_outbound_payload_from_desk_text`
    /// then re-classifies into typed `WorkerToService` variants.
    #[tokio::test]
    async fn send_response_round_trips_to_text_signaling_model() {
        let (tx, mut rx) = mpsc::unbounded_channel::<DeskSessionMessage>();
        let mut sender = DeskSessionSender { sender: tx };
        sender
            .send_response(
                "req-42",
                SignalingType::ManagerSystemInfo,
                Some("conn-7".to_string()),
                &serde_json::json!({"hello": "world"}),
            )
            .await
            .expect("send_response");

        let DeskSessionMessage::Text(text) = rx.try_recv().expect("queued") else {
            panic!("expected Text variant");
        };
        let model: SignalingModel = serde_json::from_str(&text).expect("parse");
        assert_eq!(model.request_id, "req-42");
        assert!(matches!(
            model.signaling_type,
            SignalingType::ManagerSystemInfo
        ));
        assert_eq!(model.to_connection_id.as_deref(), Some("conn-7"));
        let state = model.response_state.expect("response_state");
        assert!(state.is_success());
    }

    /// `send_error` should produce a Text variant whose decoded model
    /// carries a non-success `response_state` with the given error code.
    /// This is the upstream of `WorkerToService::SignalingError`.
    #[tokio::test]
    async fn send_error_round_trips_with_error_response_state() {
        let (tx, mut rx) = mpsc::unbounded_channel::<DeskSessionMessage>();
        let mut sender = DeskSessionSender { sender: tx };
        sender
            .send_error(
                "req-9",
                SignalingType::EnablePrivateScreen,
                Some("conn-2".to_string()),
                DeskErrorCode::PERMISSION_ERROR,
                "denied",
            )
            .await
            .expect("send_error");

        let DeskSessionMessage::Text(text) = rx.try_recv().expect("queued") else {
            panic!("expected Text variant");
        };
        let model: SignalingModel = serde_json::from_str(&text).expect("parse");
        assert!(matches!(
            model.signaling_type,
            SignalingType::EnablePrivateScreen
        ));
        let state = model.response_state.expect("response_state");
        assert!(!state.is_success());
        assert_eq!(state.error_code, DeskErrorCode::PERMISSION_ERROR.code());
        assert_eq!(state.message.as_deref(), Some("denied"));
    }
}

#[cfg(test)]
mod handle_request_control_tests {
    use super::*;

    /// Grant + already accepted ⇒ short-circuit.
    #[test]
    fn control_short_circuit_when_accepted_and_asked_to_grant() {
        assert!(should_short_circuit_control(true, true));
    }

    /// Grant + not yet accepted ⇒ MUST NOT short-circuit (need real
    /// permission check / Tauri prompt).
    #[test]
    fn control_no_short_circuit_when_not_yet_accepted() {
        assert!(!should_short_circuit_control(true, false));
    }

    /// Release path ⇒ MUST NOT short-circuit even when currently accepted.
    /// Short-circuiting here would turn a CloseControl into a no-op and
    /// the worker would stay in `accept_control = true`.
    #[test]
    fn control_no_short_circuit_on_release_even_if_accepted() {
        assert!(!should_short_circuit_control(false, true));
    }

    /// Release path + not accepted ⇒ no short-circuit (idempotent release
    /// path goes through normal flow which is also a no-op).
    #[test]
    fn control_no_short_circuit_on_release_when_not_accepted() {
        assert!(!should_short_circuit_control(false, false));
    }

    /// Clipboard short-circuit ONLY when asked AND already accepted —
    /// independent of control. The asymmetric case "control already
    /// accepted, clipboard not" must NOT auto-approve clipboard.
    #[test]
    fn clipboard_short_circuit_when_accepted_and_asked() {
        assert!(should_short_circuit_clipboard(true, true));
    }

    #[test]
    fn clipboard_no_short_circuit_when_not_yet_accepted() {
        assert!(!should_short_circuit_clipboard(true, false));
    }

    #[test]
    fn clipboard_no_short_circuit_when_not_asked() {
        assert!(!should_short_circuit_clipboard(false, true));
        assert!(!should_short_circuit_clipboard(false, false));
    }
}
