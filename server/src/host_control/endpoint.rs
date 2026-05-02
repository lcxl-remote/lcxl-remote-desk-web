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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use actix_web::{HttpRequest, HttpResponse, web};
use log::{debug, info, warn};
use tokio::sync::{broadcast, mpsc};

use super::protocol::{ClientRole, HostControlMessage};
use super::{HostControlEvent, HostControlHub, HubMode, UpstreamSessionId};
use crate::{TauriIsAdminOverride, TauriLoginToken};

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
    /// Auto-login token shared with the HTTP `/login_tauri` route; refreshed on
    /// every Tauri ws connect so the next webview load can auto-authenticate.
    pub tauri_login_token: TauriLoginToken,
    /// Used to assign monotonically increasing UpstreamSessionId values.
    pub next_session_id: Arc<AtomicU64>,
    /// Optional override populated from Tauri's `Ready { is_admin }` so HTTP
    /// handlers can report the elevation status of the Tauri process. Only
    /// wired by the daemon (Aggregator); portable / DeskServer leave it None.
    pub tauri_is_admin: Option<TauriIsAdminOverride>,
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
        }
    }

    pub fn with_tauri_is_admin(mut self, override_data: TauriIsAdminOverride) -> Self {
        self.tauri_is_admin = Some(override_data);
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
        .route("/ws/host_upstream", web::get().to(ws_upstream_handler));
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

    on_disconnect(&state, role, session_id);
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
            | HostControlMessage::ServiceOp { .. },
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
            // Forwarder-only path on Aggregator: clean up routing tables.
            if state.hub.mode() == HubMode::Aggregator {
                let _ = state.hub.pop_upstream_for_req(&req_id);
            }
        }
        // Forwarder → Aggregator upstream messages (only valid on aggregator).
        HostControlMessage::SecurityApprovalRequest {
            req_id,
            permission_type,
            from_connection_id,
        } if state.hub.mode() == HubMode::Aggregator => {
            state.hub.register_upstream_request(
                req_id.clone(),
                session_id,
                permission_type.clone(),
                from_connection_id.clone(),
            );
            // Re-publish to Tauri-facing broadcast.
            let _ = state
                .hub
                .send_command(HostControlMessage::SecurityApprovalRequest {
                    req_id,
                    permission_type,
                    from_connection_id,
                });
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
) {
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
        assert!(!is_outbound_for_role(
            &HostControlMessage::PrivateScreenShow {
                connection_id: "c1".into()
            },
            role
        ));
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
