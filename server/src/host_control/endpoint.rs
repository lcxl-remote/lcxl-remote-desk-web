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
use tokio::sync::broadcast;

use super::protocol::{ClientRole, HostControlMessage};
use super::{HostControlEvent, HostControlHub, HubMode, UpstreamSessionId};

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
    /// Static auto-login token issued to Tauri shells; refresh on each connection.
    pub tauri_login_token: Arc<std::sync::Mutex<Option<String>>>,
    /// Used to assign monotonically increasing UpstreamSessionId values.
    pub next_session_id: Arc<AtomicU64>,
}

impl EndpointState {
    pub fn new(hub: Arc<HostControlHub>, ipc_token: String) -> Self {
        Self {
            hub,
            ipc_token,
            tauri_login_token: Arc::new(std::sync::Mutex::new(None)),
            next_session_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn alloc_session_id(&self) -> UpstreamSessionId {
        self.next_session_id.fetch_add(1, Ordering::AcqRel)
    }
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
    *state.tauri_login_token.lock().unwrap() = Some(new_token.clone());
    let token_msg = HostControlMessage::TauriToken { token: new_token };
    if let Ok(json) = serde_json::to_string(&token_msg)
        && session.text(json).await.is_err() {
            info!("[HostCtrl/WS] failed to send TauriToken; closing");
            return;
        }

    let mut role: Option<ClientRole> = None;
    let session_id: UpstreamSessionId = state.alloc_session_id();
    let mut outbound_rx = state.hub.subscribe_outbound();

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
            // ws stream → hub state.
            msg = msg_stream.recv() => {
                match msg {
                    Some(Ok(actix_ws::Message::Text(text))) => {
                        match serde_json::from_str::<HostControlMessage>(&text) {
                            Ok(parsed) => {
                                handle_client_message(&state, &mut role, session_id, parsed).await;
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
            | HostControlMessage::SecurityApprovalRequest { .. }
            | HostControlMessage::ServiceOp { .. },
        ) => true,
        // Forwarder-bound messages.
        (
            ClientRole::Forwarder,
            HostControlMessage::SecurityApprovalSubmit { .. }
            | HostControlMessage::SecurityApprovalCancel { .. }
            | HostControlMessage::PrivateScreenStateChangedToWorker { .. },
        ) => true,
        _ => false,
    }
}

async fn handle_client_message(
    state: &Arc<EndpointState>,
    role: &mut Option<ClientRole>,
    session_id: UpstreamSessionId,
    msg: HostControlMessage,
) {
    match msg {
        HostControlMessage::Ready { role: r, is_admin } => {
            info!("[HostCtrl/WS] Ready role={r:?} is_admin={is_admin:?} session_id={session_id}");
            *role = Some(r);
            if r == ClientRole::Tauri {
                state.hub.mark_tauri_connected();
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
            state.hub.mark_tauri_disconnected();
            // Local hub: no surviving UI means in-flight approvals must deny so
            // business doesn't hang. (Aggregator handles its own cleanup elsewhere.)
            state.hub.deny_all_pending();
        }
        (Some(ClientRole::Tauri), HubMode::Aggregator) => {
            state.hub.mark_tauri_disconnected();
        }
        (Some(ClientRole::Forwarder), HubMode::Aggregator) => {
            // Drain all approvals that this forwarder owned — and broadcast a
            // synthetic Cancel to any Tauri clients watching, so they close
            // their dialogs.
            let drained = state.hub.drain_upstream_pending(session_id);
            for req_id in drained {
                let _ = state
                    .hub
                    .send_command(HostControlMessage::SecurityApprovalCancel { req_id });
            }
        }
        _ => {}
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
        assert!(is_outbound_for_role(
            &HostControlMessage::SecurityApprovalSubmit {
                req_id: "r1".into(),
                approved: true,
                remember: false,
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
}
