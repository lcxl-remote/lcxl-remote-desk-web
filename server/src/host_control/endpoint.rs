//! Actix WebSocket endpoint for the Host Control Hub.
//!
//! Two routes are exposed:
//! - `/ws/tauri_ipc` — Tauri shell connects here. Receives outbound commands
//!   (broadcast from hub) and sends back state events / approval submits.
//! - `/ws/host_upstream` — only registered on the Aggregator; worker forwarders
//!   connect here.
//!
//! Both endpoints share the same wire protocol (`HostControlMessage`) and the
//! same query-string token authentication.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use actix_web::{HttpRequest, HttpResponse, web};
use log::{debug, info, warn};
use tokio::sync::{broadcast, mpsc};

use super::protocol::{ClientRole, HostControlMessage};
use super::{HostControlEvent, HostControlHub, HubMode, UpstreamSessionId};
use crate::{TauriIsAdminOverride, TauriLoginToken, model::settings::SharedSettings};

/// Constant-time byte comparison for query-string tokens. Returns `false` on
/// length mismatch and never short-circuits on the first differing byte.
pub fn verify_ws_token(provided: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let p = provided.as_bytes();
    let e = expected.as_bytes();
    if p.len() != e.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in p.iter().zip(e.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Reasons the ws handshake may be rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsAuthError {
    MissingToken,
    InvalidToken,
}

/// Validate the `?token=` query parameter against `expected`. Pure helper used
/// by the actix handlers; isolated for unit testing.
pub fn check_query_token(
    query: &HashMap<String, String>,
    expected: &str,
) -> Result<(), WsAuthError> {
    let provided = query.get("token").map(String::as_str).unwrap_or("");
    if provided.is_empty() {
        return Err(WsAuthError::MissingToken);
    }
    if !verify_ws_token(provided, expected) {
        return Err(WsAuthError::InvalidToken);
    }
    Ok(())
}

/// Server-side state for a single Tauri or forwarder ws session.
#[derive(Clone)]
pub struct EndpointState {
    pub hub: Arc<HostControlHub>,
    pub ipc_token: String,
    /// Auto-login token shared with the HTTP `/api/auth/tauri-login` route; refreshed on
    /// every Tauri ws connect so the next webview load can auto-authenticate.
    pub tauri_login_token: TauriLoginToken,
    /// Used to assign monotonically increasing UpstreamSessionId values.
    pub next_session_id: Arc<AtomicU64>,
    /// Optional override populated from Tauri's `Ready { is_admin }` so HTTP
    /// handlers can report the elevation status of the Tauri process. Only
    /// wired by the daemon (Aggregator); portable / DeskServer leave it None.
    pub tauri_is_admin: Option<TauriIsAdminOverride>,
    /// Shared settings is installed by production servers. Keeping it optional
    /// lets protocol-only endpoint tests construct minimal state.
    pub settings: Option<web::Data<SharedSettings>>,
    /// The host's durable commit path, installed alongside the settings. The
    /// native shell changes the locale through it rather than writing the
    /// settings itself, so its change reaches the worker like any other.
    pub settings_coordinator: Option<Arc<crate::model::settings_coordinator::SettingsCoordinator>>,
    /// Native REST bearer token → owning WS session. Tokens are never
    /// broadcast and are revoked when that exact session disconnects.
    native_bridge_sessions: Arc<Mutex<HashMap<String, UpstreamSessionId>>>,
}

impl EndpointState {
    pub fn new(
        hub: Arc<HostControlHub>,
        ipc_token: String,
        tauri_login_token: TauriLoginToken,
    ) -> Self {
        Self {
            hub,
            ipc_token,
            tauri_login_token,
            next_session_id: Arc::new(AtomicU64::new(1)),
            tauri_is_admin: None,
            settings: None,
            settings_coordinator: None,
            native_bridge_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_settings_coordinator(
        mut self,
        coordinator: Arc<crate::model::settings_coordinator::SettingsCoordinator>,
    ) -> Self {
        self.settings_coordinator = Some(coordinator);
        self
    }

    pub fn with_tauri_is_admin(mut self, override_data: TauriIsAdminOverride) -> Self {
        self.tauri_is_admin = Some(override_data);
        self
    }

    pub fn with_settings(mut self, settings: web::Data<SharedSettings>) -> Self {
        self.settings = Some(settings);
        self
    }

    fn alloc_session_id(&self) -> UpstreamSessionId {
        self.next_session_id.fetch_add(1, Ordering::AcqRel)
    }
}

/// Register the host-control WebSocket routes on `cfg`. Both portable and
/// daemon embed this so the wire surface is identical regardless of mode.
///
/// `/ws/tauri_ipc` always accepts Tauri shells. `/ws/host_upstream` is mounted
/// in all modes for routing simplicity but rejects connections (404) from any
/// non-Aggregator hub — see `ws_upstream_handler`.
pub fn register_routes(cfg: &mut web::ServiceConfig, state: Arc<EndpointState>) {
    cfg.app_data(web::Data::from(state))
        .route("/ws/tauri_ipc", web::get().to(ws_handler))
        .route("/ws/host_upstream", web::get().to(ws_upstream_handler))
        .route("/api/native/locale", web::get().to(get_native_locale))
        .route("/api/native/locale", web::put().to(put_native_locale));
}

#[derive(serde::Serialize)]
struct NativeLocaleResponse {
    locale: String,
}

#[derive(serde::Deserialize)]
struct NativeLocaleRequest {
    locale: String,
}

fn native_bridge_authorized(state: &EndpointState, req: &HttpRequest) -> bool {
    let Some(value) = req
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    state
        .native_bridge_sessions
        .lock()
        .unwrap()
        .contains_key(token)
}

async fn get_native_locale(state: web::Data<EndpointState>, req: HttpRequest) -> HttpResponse {
    if !native_bridge_authorized(&state, &req) {
        return HttpResponse::Unauthorized().finish();
    }
    let Some(settings) = state.settings.as_ref() else {
        return HttpResponse::ServiceUnavailable().finish();
    };
    let locale = settings
        .read()
        .await
        .system
        .locale
        .clone()
        .unwrap_or_else(|| crate::locale::DEFAULT_LOCALE.to_string());
    HttpResponse::Ok().json(NativeLocaleResponse { locale })
}

async fn put_native_locale(
    state: web::Data<EndpointState>,
    req: HttpRequest,
    payload: web::Json<NativeLocaleRequest>,
) -> HttpResponse {
    if !native_bridge_authorized(&state, &req) {
        return HttpResponse::Unauthorized().finish();
    }
    let Some(locale) = crate::locale::canonicalize(&payload.locale) else {
        return HttpResponse::BadRequest().body("unsupported locale");
    };
    let Some(coordinator) = state.settings_coordinator.as_ref() else {
        return HttpResponse::ServiceUnavailable().finish();
    };

    // The commit persists the locale, applies it process-wide and tells the
    // worker; only the local shell still has to be told separately.
    if let Err(error) = coordinator
        .commit(|settings| {
            settings.system.locale = Some(locale.to_string());
            Ok(())
        })
        .await
    {
        warn!("[NativeLocale] failed to persist locale: {error}");
        return HttpResponse::InternalServerError().finish();
    }

    let _ = state
        .hub
        .send_command(HostControlMessage::GlobalLocaleChanged {
            locale: locale.to_string(),
        });

    HttpResponse::Ok().json(NativeLocaleResponse {
        locale: locale.to_string(),
    })
}

/// Actix handler for `/ws/tauri_ipc`.
///
/// Accepts both Tauri shells (default role) and — on the aggregator — also
/// forwarders. The role is determined by the first inbound `Ready` message.
pub async fn ws_handler(
    state: web::Data<EndpointState>,
    req: HttpRequest,
    stream: web::Payload,
    query: web::Query<HashMap<String, String>>,
) -> Result<HttpResponse, actix_web::Error> {
    if let Err(err) = check_query_token(&query, &state.ipc_token) {
        warn!("[HostCtrl/WS] auth rejected: {err:?}");
        return Ok(HttpResponse::Unauthorized().finish());
    }

    let (response, session, msg_stream) = actix_ws::handle(&req, stream)?;
    let state_inner = state.into_inner();
    actix_web::rt::spawn(run_ws_session(state_inner, session, msg_stream));
    Ok(response)
}

/// Actix handler for `/ws/host_upstream`. Only valid on the Aggregator. Body
/// identical to `ws_handler`; kept separate so daemon can register them with
/// distinct paths and the role check can be enforced earlier in the future.
pub async fn ws_upstream_handler(
    state: web::Data<EndpointState>,
    req: HttpRequest,
    stream: web::Payload,
    query: web::Query<HashMap<String, String>>,
) -> Result<HttpResponse, actix_web::Error> {
    if state.hub.mode() != HubMode::Aggregator {
        return Ok(HttpResponse::NotFound().finish());
    }
    ws_handler(state, req, stream, query).await
}

async fn run_ws_session(
    state: Arc<EndpointState>,
    mut session: actix_ws::Session,
    mut msg_stream: actix_ws::MessageStream,
) {
    info!(
        "[HostCtrl/WS] client connected (mode={:?})",
        state.hub.mode()
    );

    // Issue a fresh tauri_login_token for this session and immediately push it.
    let new_token = uuid::Uuid::new_v4().to_string();
    state.tauri_login_token.refresh(new_token.clone());
    let token_msg = HostControlMessage::TauriToken { token: new_token };
    if let Ok(json) = serde_json::to_string(&token_msg)
        && session.text(json).await.is_err()
    {
        info!("[HostCtrl/WS] failed to send TauriToken; closing");
        return;
    }

    let mut role: Option<ClientRole> = None;
    let session_id: UpstreamSessionId = state.alloc_session_id();
    let mut outbound_rx = state.hub.subscribe_outbound();
    // Per-session directional mpsc — the hub writes here when it needs to address
    // a specific forwarder (e.g. SecurityApprovalSubmit routed via pending_routes).
    // For Tauri sessions the receiver is created but never written to.
    let (session_tx, mut session_rx) = mpsc::unbounded_channel::<HostControlMessage>();
    let mut native_bridge_token: Option<String> = None;

    loop {
        tokio::select! {
            // Hub → ws sink (filter forwarder-only messages out of the Tauri stream).
            next = outbound_rx.recv() => {
                match next {
                    Ok(msg) => {
                        if !is_outbound_for_role(&msg, role) {
                            continue;
                        }
                        let json = match serde_json::to_string(&msg) {
                            Ok(j) => j,
                            Err(e) => {
                                warn!("[HostCtrl/WS] serialize: {e}");
                                continue;
                            }
                        };
                        if session.text(json).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("[HostCtrl/WS] outbound lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Hub → ws sink (directional). Used by Aggregator for SubmitApproval
            // routed to a specific forwarder via `route_to_forwarder`.
            direct = session_rx.recv() => {
                let Some(msg) = direct else { break };
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        warn!("[HostCtrl/WS] serialize (direct): {e}");
                        continue;
                    }
                };
                if session.text(json).await.is_err() {
                    break;
                }
            }
            // ws stream → hub state.
            msg = msg_stream.recv() => {
                match msg {
                    Some(Ok(actix_ws::Message::Text(text))) => {
                        match serde_json::from_str::<HostControlMessage>(&text) {
                            Ok(parsed) => {
                                handle_client_message(
                                    &state,
                                    &mut role,
                                    session_id,
                                    &session_tx,
                                    &mut native_bridge_token,
                                    parsed,
                                )
                                .await;
                            }
                            Err(e) => warn!("[HostCtrl/WS] parse: {e} ({text})"),
                        }
                    }
                    Some(Ok(actix_ws::Message::Ping(data))) => {
                        let _ = session.pong(&data).await;
                    }
                    Some(Ok(actix_ws::Message::Close(reason))) => {
                        debug!("[HostCtrl/WS] close: {reason:?}");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        warn!("[HostCtrl/WS] ws err: {e}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    on_disconnect(&state, role, session_id, native_bridge_token.as_deref());
}

/// Filter: only deliver to a session if the message is meant for its role.
fn is_outbound_for_role(msg: &HostControlMessage, role: Option<ClientRole>) -> bool {
    let Some(r) = role else {
        // Pre-Ready: only allow the initial TauriToken (which we already pushed
        // synchronously before subscribing). Suppress everything else.
        return false;
    };
    match (r, msg) {
        // Tauri-bound commands.
        (
            ClientRole::Tauri,
            HostControlMessage::TauriToken { .. }
            | HostControlMessage::PrivateScreenShow { .. }
            | HostControlMessage::PrivateScreenHide { .. }
            | HostControlMessage::WhiteboardShow { .. }
            | HostControlMessage::WhiteboardDraw { .. }
            | HostControlMessage::WhiteboardHide { .. }
            | HostControlMessage::SecurityApprovalRequest { .. }
            | HostControlMessage::SecurityApprovalFinished { .. }
            | HostControlMessage::ServiceOp { .. }
            | HostControlMessage::HostAccessSnapshot { .. }
            | HostControlMessage::GlobalLocaleChanged { .. },
        ) => true,
        // Forwarder-bound broadcast: only Cancel + state changes.
        // SecurityApprovalSubmit is dispatched via per-session mpsc (directional)
        // so it never reaches the broadcast path — see `Hub::route_to_forwarder`.
        (
            ClientRole::Forwarder,
            HostControlMessage::SecurityApprovalCancel { .. }
            | HostControlMessage::PrivateScreenStateChangedToWorker { .. },
        ) => true,
        _ => false,
    }
}

async fn handle_client_message(
    state: &Arc<EndpointState>,
    role: &mut Option<ClientRole>,
    session_id: UpstreamSessionId,
    session_tx: &mpsc::UnboundedSender<HostControlMessage>,
    native_bridge_token: &mut Option<String>,
    msg: HostControlMessage,
) {
    match msg {
        HostControlMessage::Ready { role: r, is_admin } => {
            info!("[HostCtrl/WS] Ready role={r:?} is_admin={is_admin:?} session_id={session_id}");
            *role = Some(r);
            match r {
                ClientRole::Tauri => {
                    state.hub.mark_tauri_connected();
                    if let Some(override_data) = state.tauri_is_admin.as_ref() {
                        *override_data.lock().unwrap() = is_admin;
                    }
                    // Replay any pending approvals that arrived before this client connected.
                    for replay in state.hub.replay_messages_for_tauri() {
                        if let Ok(_json) = serde_json::to_string(&replay) {
                            // We don't have direct session access here; the broadcast
                            // path will deliver to this client (since it subscribed).
                            // Pump it through the hub's broadcast so all current Tauri
                            // clients see consistent state.
                            let _ = state.hub.send_command(replay);
                        }
                    }
                    let _ = session_tx.send(HostControlMessage::HostAccessSnapshot {
                        snapshot: state.hub.host_activity().snapshot(),
                    });
                    if native_bridge_token.is_none() {
                        let token = uuid::Uuid::new_v4().to_string();
                        state
                            .native_bridge_sessions
                            .lock()
                            .unwrap()
                            .insert(token.clone(), session_id);
                        let (locale, locale_persisted) =
                            if let Some(settings) = state.settings.as_ref() {
                                let settings = settings.read().await;
                                (
                                    settings.system.locale.clone().unwrap_or_else(|| {
                                        crate::locale::DEFAULT_LOCALE.to_string()
                                    }),
                                    settings.system.locale.is_some(),
                                )
                            } else {
                                (crate::locale::current_locale(), false)
                            };
                        let _ = session_tx.send(HostControlMessage::NativeBridgeReady {
                            token: token.clone(),
                            locale,
                            locale_persisted,
                        });
                        *native_bridge_token = Some(token);
                    }
                }
                ClientRole::Forwarder => {
                    if state.hub.mode() == HubMode::Aggregator {
                        state
                            .hub
                            .register_forwarder_session(session_id, session_tx.clone());
                    }
                }
            }
        }
        HostControlMessage::PrivateScreenStateChanged {
            connection_id,
            visible,
        } => {
            state
                .hub
                .publish_state(HostControlEvent::PrivateScreenVisibilityChanged {
                    connection_id,
                    visible,
                });
        }
        HostControlMessage::SecurityApprovalResolved { req_id } => {
            // Forwarder → Aggregator: the originating worker resolved this request
            // locally (e.g. its authoritative timeout fired). Clean up routing /
            // replay and close the Tauri dialog, but only if this session owns the
            // req_id (ownership checked inside).
            if state.hub.mode() == HubMode::Aggregator {
                state.hub.resolve_upstream_request(&req_id, session_id);
            }
        }
        // Forwarder → Aggregator upstream messages (only valid on aggregator).
        HostControlMessage::SecurityApprovalRequest {
            req_id,
            permission_type,
            from_connection_id,
        } if state.hub.mode() == HubMode::Aggregator => {
            state.hub.handle_upstream_approval_request(
                req_id,
                session_id,
                permission_type,
                from_connection_id,
            );
        }
        // Generic forwarder commands re-broadcast on aggregator.
        msg @ (HostControlMessage::PrivateScreenShow { .. }
        | HostControlMessage::PrivateScreenHide { .. }
        | HostControlMessage::WhiteboardShow { .. }
        | HostControlMessage::WhiteboardDraw { .. }
        | HostControlMessage::WhiteboardHide { .. }
        | HostControlMessage::ServiceOp { .. })
            if state.hub.mode() == HubMode::Aggregator =>
        {
            let _ = state.hub.send_command(msg);
        }
        other => {
            debug!("[HostCtrl/WS] ignoring client message: {other:?}");
        }
    }
}

fn on_disconnect(
    state: &Arc<EndpointState>,
    role: Option<ClientRole>,
    session_id: UpstreamSessionId,
    native_bridge_token: Option<&str>,
) {
    if let Some(token) = native_bridge_token {
        state.native_bridge_sessions.lock().unwrap().remove(token);
    }
    info!("[HostCtrl/WS] client disconnected (role={role:?} session_id={session_id})");
    match (role, state.hub.mode()) {
        (Some(ClientRole::Tauri), HubMode::Local) => {
            let remaining = state.hub.mark_tauri_disconnected();
            if remaining == 0 {
                if let Some(override_data) = state.tauri_is_admin.as_ref() {
                    *override_data.lock().unwrap() = None;
                }
                // Local hub: no surviving UI means in-flight approvals must deny
                // so business doesn't hang.
                state.hub.deny_all_pending();
            }
        }
        (Some(ClientRole::Tauri), HubMode::Aggregator) => {
            let remaining = state.hub.mark_tauri_disconnected();
            if remaining == 0 {
                if let Some(override_data) = state.tauri_is_admin.as_ref() {
                    *override_data.lock().unwrap() = None;
                }
                // Last Tauri client gone: cancel every in-flight approval so
                // the workers don't sit blocked on an unanswerable dialog.
                let cancelled = state.hub.cancel_all_for_tauri_loss();
                if !cancelled.is_empty() {
                    debug!(
                        "[HostCtrl/WS] Tauri lost; cancelled {} pending req(s): {cancelled:?}",
                        cancelled.len()
                    );
                }
            }
        }
        (Some(ClientRole::Forwarder), HubMode::Aggregator) => {
            // drain_upstream_pending also unregisters the forwarder mpsc so any
            // race-condition late submit returns false instead of dispatching.
            let drained = state.hub.drain_upstream_pending(session_id);
            if !drained.is_empty() {
                debug!(
                    "[HostCtrl/WS] forwarder session_id={session_id} drained {} pending req(s): {drained:?}",
                    drained.len()
                );
            }
        }
        _ => {
            // Pre-Ready disconnects or forwarders on a non-Aggregator hub: nothing
            // to clean up (forwarder sessions are only registered on Aggregator).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // U-15: bad token rejected.
    #[test]
    fn u15_check_query_token_bad() {
        let mut q = HashMap::new();
        q.insert("token".to_string(), "wrong".to_string());
        assert_eq!(
            check_query_token(&q, "right"),
            Err(WsAuthError::InvalidToken)
        );
    }

    #[test]
    fn u15_check_query_token_missing() {
        let q = HashMap::new();
        assert_eq!(check_query_token(&q, "x"), Err(WsAuthError::MissingToken));
    }

    #[test]
    fn u15_check_query_token_empty_provided() {
        let mut q = HashMap::new();
        q.insert("token".to_string(), "".to_string());
        assert_eq!(check_query_token(&q, "x"), Err(WsAuthError::MissingToken));
    }

    // Good token accepted.
    #[test]
    fn check_query_token_good() {
        let mut q = HashMap::new();
        q.insert("token".to_string(), "secret".to_string());
        assert!(check_query_token(&q, "secret").is_ok());
    }

    // verify_ws_token rejects empty expected (defence in depth).
    #[test]
    fn verify_token_empty_expected() {
        assert!(!verify_ws_token("anything", ""));
    }

    // verify_ws_token is length-mismatch safe.
    #[test]
    fn verify_token_length_mismatch() {
        assert!(!verify_ws_token("a", "ab"));
        assert!(!verify_ws_token("ab", "a"));
    }

    #[test]
    fn role_filter_tauri() {
        use crate::model::security_approval::SecurityPermissionType;
        let role = Some(ClientRole::Tauri);

        assert!(is_outbound_for_role(
            &HostControlMessage::PrivateScreenShow {
                connection_id: "c1".into()
            },
            role
        ));
        assert!(is_outbound_for_role(
            &HostControlMessage::SecurityApprovalRequest {
                req_id: "r1".into(),
                permission_type: SecurityPermissionType::RemoteControl,
                from_connection_id: None,
            },
            role
        ));
        assert!(is_outbound_for_role(
            &HostControlMessage::SecurityApprovalFinished {
                req_id: "r1".into(),
            },
            role
        ));
        assert!(!is_outbound_for_role(
            &HostControlMessage::SecurityApprovalSubmit {
                req_id: "r1".into(),
                approved: true,
                remember: false,
            },
            role
        ));
    }

    #[test]
    fn role_filter_forwarder() {
        let role = Some(ClientRole::Forwarder);
        // SecurityApprovalSubmit is directional (per-session mpsc); it must NOT
        // be delivered via the broadcast path.
        assert!(!is_outbound_for_role(
            &HostControlMessage::SecurityApprovalSubmit {
                req_id: "r1".into(),
                approved: true,
                remember: false,
            },
            role
        ));
        // Cancel and state changes are still legitimate broadcast traffic.
        assert!(is_outbound_for_role(
            &HostControlMessage::SecurityApprovalCancel {
                req_id: "r1".into()
            },
            role
        ));
        // Finished is a Tauri-bound notification; it must not leak to forwarder
        // sessions on the broadcast path.
        assert!(!is_outbound_for_role(
            &HostControlMessage::SecurityApprovalFinished {
                req_id: "r1".into(),
            },
            role
        ));
        assert!(!is_outbound_for_role(
            &HostControlMessage::PrivateScreenShow {
                connection_id: "c1".into()
            },
            role
        ));
        assert!(!is_outbound_for_role(
            &HostControlMessage::HostAccessSnapshot {
                snapshot: crate::host_control::HostAccessSnapshot {
                    epoch: "epoch-1".into(),
                    revision: 1,
                    indicator_enabled: true,
                    total_session_count: 0,
                    sessions: Vec::new(),
                    remote_access: crate::host_control::HostRemoteAccessStatus::default(),
                },
            },
            role
        ));
    }

    #[tokio::test]
    async fn tauri_ready_receives_current_host_access_snapshot_directly() {
        let hub = Arc::new(HostControlHub::new_local());
        hub.host_activity().set_indicator_enabled(false);
        let state = Arc::new(EndpointState::new(
            hub,
            "secret".to_string(),
            TauriLoginToken::empty(),
        ));
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut role = None;
        let mut bridge_token = None;

        handle_client_message(
            &state,
            &mut role,
            1,
            &session_tx,
            &mut bridge_token,
            HostControlMessage::Ready {
                role: ClientRole::Tauri,
                is_admin: Some(false),
            },
        )
        .await;

        let message = session_rx.recv().await.expect("snapshot");
        match message {
            HostControlMessage::HostAccessSnapshot { snapshot } => {
                assert!(!snapshot.indicator_enabled);
                assert!(snapshot.sessions.is_empty());
            }
            other => panic!("expected host snapshot, got {other:?}"),
        }
        assert!(matches!(
            session_rx.recv().await,
            Some(HostControlMessage::NativeBridgeReady { .. })
        ));
        assert!(bridge_token.is_some());
    }

    #[tokio::test]
    async fn native_bridge_tokens_are_unique_per_ws_session_and_never_broadcast() {
        let state = Arc::new(EndpointState::new(
            Arc::new(HostControlHub::new_local()),
            "secret".to_string(),
            TauriLoginToken::empty(),
        ));
        let (first_tx, mut first_rx) = mpsc::unbounded_channel();
        let (second_tx, mut second_rx) = mpsc::unbounded_channel();
        let mut first_role = None;
        let mut second_role = None;
        let mut first_token = None;
        let mut second_token = None;

        for (session_id, role, token, tx) in [
            (1, &mut first_role, &mut first_token, &first_tx),
            (2, &mut second_role, &mut second_token, &second_tx),
        ] {
            handle_client_message(
                &state,
                role,
                session_id,
                tx,
                token,
                HostControlMessage::Ready {
                    role: ClientRole::Tauri,
                    is_admin: None,
                },
            )
            .await;
        }

        let _ = first_rx.recv().await.expect("first snapshot");
        let first_ready = first_rx.recv().await.expect("first bridge token");
        let _ = second_rx.recv().await.expect("second snapshot");
        let second_ready = second_rx.recv().await.expect("second bridge token");
        let (first, second) = match (first_ready, second_ready) {
            (
                HostControlMessage::NativeBridgeReady { token: first, .. },
                HostControlMessage::NativeBridgeReady { token: second, .. },
            ) => (first, second),
            other => panic!("expected two direct bridge-ready messages, got {other:?}"),
        };
        assert_ne!(first, second);
        assert!(!is_outbound_for_role(
            &HostControlMessage::NativeBridgeReady {
                token: "must-not-broadcast".into(),
                locale: "en-US".into(),
                locale_persisted: true,
            },
            Some(ClientRole::Tauri),
        ));
    }

    #[test]
    fn native_locale_rest_accepts_only_a_registered_ws_session_token() {
        let state = Arc::new(EndpointState::new(
            Arc::new(HostControlHub::new_local()),
            "secret".to_string(),
            TauriLoginToken::empty(),
        ));
        state
            .native_bridge_sessions
            .lock()
            .unwrap()
            .insert("registered".to_string(), 7);

        let valid = actix_web::test::TestRequest::default()
            .insert_header(("Authorization", "Bearer registered"))
            .to_http_request();
        let invalid = actix_web::test::TestRequest::default()
            .insert_header(("Authorization", "Bearer other-session"))
            .to_http_request();
        let absent = actix_web::test::TestRequest::default().to_http_request();

        assert!(native_bridge_authorized(&state, &valid));
        assert!(!native_bridge_authorized(&state, &invalid));
        assert!(!native_bridge_authorized(&state, &absent));
        on_disconnect(&state, None, 7, Some("registered"));
        assert!(!native_bridge_authorized(&state, &valid));
    }

    #[test]
    fn role_filter_pre_ready_suppresses() {
        let role: Option<ClientRole> = None;
        assert!(!is_outbound_for_role(
            &HostControlMessage::PrivateScreenShow {
                connection_id: "c1".into()
            },
            role
        ));
    }

    // U-15 (extra): bad token rejects with 401 on /ws/tauri_ipc.
    #[actix_web::test]
    async fn ws_handler_rejects_bad_token() {
        use actix_web::{App, test};
        let hub = Arc::new(HostControlHub::new_local());
        let state = Arc::new(EndpointState::new(
            hub,
            "secret".to_string(),
            TauriLoginToken::empty(),
        ));
        let app =
            test::init_service(App::new().configure(|cfg| register_routes(cfg, state.clone())))
                .await;

        // Plain GET (not ws upgrade) — auth runs before upgrade so missing token
        // is the first failure.
        let req = test::TestRequest::get()
            .uri("/ws/tauri_ipc?token=wrong")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    // /ws/host_upstream returns 404 on Local hubs (only valid on Aggregator).
    #[actix_web::test]
    async fn ws_upstream_handler_404_on_non_aggregator() {
        use actix_web::{App, test};
        let hub = Arc::new(HostControlHub::new_local());
        let state = Arc::new(EndpointState::new(
            hub,
            "secret".to_string(),
            TauriLoginToken::empty(),
        ));
        let app =
            test::init_service(App::new().configure(|cfg| register_routes(cfg, state.clone())))
                .await;
        let req = test::TestRequest::get()
            .uri("/ws/host_upstream?token=secret")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    // EndpointState::with_tauri_is_admin attaches the override and propagates it.
    #[test]
    fn endpoint_state_with_tauri_is_admin_sets_field() {
        use std::sync::{Arc, Mutex};
        let hub = Arc::new(HostControlHub::new_aggregator());
        let override_data: TauriIsAdminOverride = Arc::new(Mutex::new(None));
        let state = EndpointState::new(hub, "secret".to_string(), TauriLoginToken::empty())
            .with_tauri_is_admin(override_data.clone());
        assert!(state.tauri_is_admin.is_some());
        // Mutating via the original Arc is visible through the state's clone.
        *override_data.lock().unwrap() = Some(true);
        assert_eq!(
            *state.tauri_is_admin.as_ref().unwrap().lock().unwrap(),
            Some(true)
        );
    }

    // /ws/host_upstream is reachable on Aggregator (auth runs to completion;
    // missing ws upgrade headers will then short-circuit, but the route itself
    // is mounted and not 404).
    #[actix_web::test]
    async fn ws_upstream_handler_reachable_on_aggregator() {
        use actix_web::{App, test};
        let hub = Arc::new(HostControlHub::new_aggregator());
        let state = Arc::new(EndpointState::new(
            hub,
            "secret".to_string(),
            TauriLoginToken::empty(),
        ));
        let app =
            test::init_service(App::new().configure(|cfg| register_routes(cfg, state.clone())))
                .await;
        let req = test::TestRequest::get()
            .uri("/ws/host_upstream?token=secret")
            .to_request();
        let resp = test::call_service(&app, req).await;
        // Without proper ws upgrade headers we expect 400 (Bad Request) rather
        // than 404 — proves the route exists and the aggregator-mode check passed.
        assert_ne!(resp.status(), 404);
    }

    /// Regression: when the daemon hand-rolled `/ws/tauri_ipc` registration as
    /// `web::Data::new(Arc::clone(&endpoint_state))`, the `Data` wrapper had
    /// type `Data<Arc<EndpointState>>` while the handler extractor expected
    /// `Data<EndpointState>` — a TypeId mismatch that made every request
    /// short-circuit with a 500 ("Failed to extract `Data<EndpointState>`").
    /// `register_routes` uses `Data::from(Arc<T>)` (which yields `Data<T>`),
    /// so anyone going through the helper is safe. This test asserts the helper
    /// stays correct: a request that reaches the handler with a good token
    /// must NOT collapse to 500.
    #[actix_web::test]
    async fn ws_handler_extracts_endpoint_state_through_register_routes() {
        use actix_web::{App, test};
        let hub = Arc::new(HostControlHub::new_local());
        let state = Arc::new(EndpointState::new(
            hub,
            "secret".to_string(),
            TauriLoginToken::empty(),
        ));
        let app =
            test::init_service(App::new().configure(|cfg| register_routes(cfg, state.clone())))
                .await;
        // Token is correct so auth passes; no ws upgrade headers will then
        // short-circuit. Either way the response must not be a 500.
        let req = test::TestRequest::get()
            .uri("/ws/tauri_ipc?token=secret")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_ne!(
            resp.status(),
            500,
            "endpoint state extraction failed — Data<T> wrapping likely drifted"
        );
    }
}
