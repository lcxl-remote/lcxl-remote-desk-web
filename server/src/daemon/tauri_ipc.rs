use crate::model::host_control::{HostControlEventType, PrivateScreenCommand, WhiteboardCommand};
use crate::model::security_approval::{
    PENDING_APPROVALS, SecurityApprovalCommand, SecurityApprovalResponse,
};
use crate::{ExternalChannels, ServiceOp, TauriIsAdminOverride, TauriLoginToken};
use actix_web::{HttpRequest, HttpResponse, web};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// IPC message protocol
// ---------------------------------------------------------------------------

/// Messages sent from the ServiceDaemon to the Tauri shell over /ws/tauri_ipc.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum DaemonToTauriMsg {
    /// One-time auto-login token; issued on every fresh WS connection.
    TauriToken {
        token: String,
    },
    PrivateScreenShow {
        connection_id: String,
    },
    PrivateScreenHide {
        connection_id: String,
    },
    WhiteboardShow {
        connection_id: String,
    },
    WhiteboardDraw {
        connection_id: String,
        message: serde_json::Value,
    },
    SecurityApprovalRequest {
        req_id: String,
        permission_type: String,
        from_connection_id: Option<String>,
    },
    ServiceOp {
        op: String,
        install_path: Option<String>,
    },
}

/// Messages received from the Tauri shell over /ws/tauri_ipc.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum TauriToDaemonMsg {
    /// First message after WS connect; carries Tauri process admin status.
    Ready { is_admin: bool },
    PrivateScreenStateChanged {
        connection_id: String,
        visible: bool,
    },
}

// ---------------------------------------------------------------------------
// TauriIpcBridge
// ---------------------------------------------------------------------------

/// Cross-process bridge between the ServiceDaemon and the Tauri UI shell.
///
/// Exposes a WebSocket endpoint (`/ws/tauri_ipc`) that the Tauri shell connects
/// to.  Also synthesises an `ExternalChannels` whose senders proxy their
/// commands across the WS link so the rest of the server code can remain
/// unmodified.
pub struct TauriIpcBridge {
    ws_tx: broadcast::Sender<String>,
    /// Tauri-reported `is_admin` value (None until Tauri connects and sends Ready).
    pub tauri_is_admin: TauriIsAdminOverride,
    /// Shared auto-login token; refreshed on each new Tauri WS connection.
    /// TauriLoginToken is Clone with shared interior Arc<Mutex> so the HTTP
    /// server's copy and the bridge's copy stay in sync.
    pub tauri_login_token: TauriLoginToken,
    state_tx: tokio::sync::mpsc::UnboundedSender<HostControlEventType>,
    /// Expected value of the `?token=` query param on /ws/tauri_ipc connections.
    ipc_token: String,
}

impl TauriIpcBridge {
    /// Create the bridge together with the `ExternalChannels` to register with
    /// the daemon's embedded HTTP server.
    pub fn new(ipc_token: String) -> (Arc<Self>, ExternalChannels) {
        let (ws_tx, _) = broadcast::channel::<String>(256);
        let tauri_is_admin: TauriIsAdminOverride = Arc::new(Mutex::new(None));
        let initial_auto_token = Uuid::new_v4().to_string();
        let tauri_login_token = TauriLoginToken::new(initial_auto_token.clone());

        let (ps_cmd_tx, ps_cmd_rx) = std::sync::mpsc::channel::<PrivateScreenCommand>();
        let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel::<HostControlEventType>();
        let (wb_cmd_tx, wb_cmd_rx) = std::sync::mpsc::channel::<WhiteboardCommand>();
        let (sec_tx, sec_rx) = std::sync::mpsc::channel::<SecurityApprovalCommand>();
        let (svc_tx, svc_rx) = std::sync::mpsc::sync_channel::<ServiceOp>(8);

        let bridge = Arc::new(TauriIpcBridge {
            ws_tx: ws_tx.clone(),
            tauri_is_admin,
            tauri_login_token,
            state_tx,
            ipc_token,
        });

        // --- background forwarder: PrivateScreenCommand → WS ---
        let ws_tx_ps = ws_tx.clone();
        std::thread::spawn(move || {
            while let Ok(cmd) = ps_cmd_rx.recv() {
                let msg = match cmd {
                    PrivateScreenCommand::Show(id) => {
                        DaemonToTauriMsg::PrivateScreenShow { connection_id: id }
                    }
                    PrivateScreenCommand::Hide(id) => {
                        DaemonToTauriMsg::PrivateScreenHide { connection_id: id }
                    }
                    PrivateScreenCommand::Quit => break,
                };
                forward_to_ws(&ws_tx_ps, &msg);
            }
        });

        // --- background forwarder: WhiteboardCommand → WS ---
        let ws_tx_wb = ws_tx.clone();
        std::thread::spawn(move || {
            while let Ok(cmd) = wb_cmd_rx.recv() {
                let msg = match cmd {
                    WhiteboardCommand::Show(id) => {
                        DaemonToTauriMsg::WhiteboardShow { connection_id: id }
                    }
                    WhiteboardCommand::DrawMessage(json_str) => {
                        let message = serde_json::from_str::<serde_json::Value>(&json_str)
                            .unwrap_or(serde_json::Value::String(json_str));
                        DaemonToTauriMsg::WhiteboardDraw {
                            connection_id: String::new(),
                            message,
                        }
                    }
                    WhiteboardCommand::Hide(_) | WhiteboardCommand::Quit => break,
                };
                forward_to_ws(&ws_tx_wb, &msg);
            }
        });

        // --- background forwarder: SecurityApprovalCommand → WS ---
        // Response comes back via HTTP POST to /api/desk/security_approval/submit
        let ws_tx_sec = ws_tx.clone();
        std::thread::spawn(move || {
            while let Ok(cmd) = sec_rx.recv() {
                if let SecurityApprovalCommand::Request(req) = cmd {
                    let msg = DaemonToTauriMsg::SecurityApprovalRequest {
                        req_id: req.req_id,
                        permission_type: format!("{:?}", req.permission_type),
                        from_connection_id: req.from_connection_id,
                    };
                    forward_to_ws(&ws_tx_sec, &msg);
                }
            }
        });

        // --- background forwarder: ServiceOp → WS ---
        let ws_tx_svc = ws_tx.clone();
        std::thread::spawn(move || {
            while let Ok(op) = svc_rx.recv() {
                let msg = match op {
                    ServiceOp::Install { install_path } => DaemonToTauriMsg::ServiceOp {
                        op: "install".to_string(),
                        install_path: Some(install_path),
                    },
                    ServiceOp::Uninstall => DaemonToTauriMsg::ServiceOp {
                        op: "uninstall".to_string(),
                        install_path: None,
                    },
                };
                forward_to_ws(&ws_tx_svc, &msg);
            }
        });

        let channels = ExternalChannels {
            private_screen_cmd_sender: Some(ps_cmd_tx),
            private_screen_state_receiver: Some(state_rx),
            tauri_login_token: Some(initial_auto_token),
            whiteboard_cmd_sender: Some(wb_cmd_tx),
            security_approval_sender: Some(sec_tx),
            service_op_sender: Some(svc_tx),
        };

        (bridge, channels)
    }

    /// Actix WebSocket handler — register at `/ws/tauri_ipc`.
    pub async fn ws_handler(
        bridge: web::Data<Arc<TauriIpcBridge>>,
        req: HttpRequest,
        stream: web::Payload,
        query: web::Query<HashMap<String, String>>,
    ) -> Result<HttpResponse, actix_web::Error> {
        let provided = query.get("token").map(String::as_str).unwrap_or("");
        if !crate::constant_time_eq(bridge.ipc_token.as_bytes(), provided.as_bytes()) {
            warn!("[TauriIpc] Rejected WS connection: invalid token");
            return Ok(HttpResponse::Unauthorized().finish());
        }

        let (response, session, msg_stream) = actix_ws::handle(&req, stream)?;
        let bridge_inner = Arc::clone(bridge.as_ref());
        actix_web::rt::spawn(run_ws_session(bridge_inner, session, msg_stream));
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Per-connection WebSocket session
// ---------------------------------------------------------------------------

async fn run_ws_session(
    bridge: Arc<TauriIpcBridge>,
    mut session: actix_ws::Session,
    mut msg_stream: actix_ws::MessageStream,
) {
    info!("[TauriIpc] Tauri shell connected");

    // Issue a fresh auto-login token for this connection
    let new_token = Uuid::new_v4().to_string();
    bridge.tauri_login_token.refresh(new_token.clone());
    let token_msg = DaemonToTauriMsg::TauriToken { token: new_token };
    if let Ok(json) = serde_json::to_string(&token_msg) {
        if session.text(json).await.is_err() {
            info!("[TauriIpc] Failed to send TauriToken, closing");
            on_disconnect(&bridge);
            return;
        }
    }

    let mut ws_rx = bridge.ws_tx.subscribe();

    loop {
        tokio::select! {
            // Outbound: bridge broadcast → Tauri
            result = ws_rx.recv() => {
                match result {
                    Ok(msg) => {
                        if session.text(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("[TauriIpc] Outbound channel lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Inbound: Tauri → bridge
            msg = msg_stream.recv() => {
                match msg {
                    Some(Ok(actix_ws::Message::Text(text))) => {
                        match serde_json::from_str::<TauriToDaemonMsg>(&text) {
                            Ok(TauriToDaemonMsg::Ready { is_admin }) => {
                                info!("[TauriIpc] Tauri ready, is_admin={is_admin}");
                                *bridge.tauri_is_admin.lock().unwrap() = Some(is_admin);
                            }
                            Ok(TauriToDaemonMsg::PrivateScreenStateChanged {
                                connection_id,
                                visible,
                            }) => {
                                debug!(
                                    "[TauriIpc] PrivateScreenStateChanged: conn={connection_id}, visible={visible}"
                                );
                                let _ = bridge.state_tx.send(
                                    HostControlEventType::PrivateScreenVisibleChanged(
                                        connection_id,
                                        visible,
                                    ),
                                );
                            }
                            Err(e) => {
                                warn!("[TauriIpc] Failed to parse incoming message: {e}");
                            }
                        }
                    }
                    Some(Ok(actix_ws::Message::Ping(data))) => {
                        let _ = session.pong(&data).await;
                    }
                    Some(Ok(actix_ws::Message::Close(reason))) => {
                        debug!("[TauriIpc] Close frame: {reason:?}");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        error!("[TauriIpc] WS error: {e}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    on_disconnect(&bridge);
}

fn on_disconnect(bridge: &TauriIpcBridge) {
    *bridge.tauri_is_admin.lock().unwrap() = None;
    deny_all_pending_approvals();
    info!("[TauriIpc] Tauri shell disconnected");
}

/// Deny all outstanding security approval requests so callers are not left blocked.
fn deny_all_pending_approvals() {
    let mut pending = PENDING_APPROVALS.lock().unwrap();
    if !pending.is_empty() {
        warn!(
            "[TauriIpc] Denying {} pending approval(s) due to Tauri disconnect",
            pending.len()
        );
        for (_, tx) in pending.drain() {
            let _ = tx.send(SecurityApprovalResponse {
                approved: false,
                remember: false,
            });
        }
    }
}

fn forward_to_ws(tx: &broadcast::Sender<String>, msg: &DaemonToTauriMsg) {
    match serde_json::to_string(msg) {
        Ok(json) => {
            let _ = tx.send(json);
        }
        Err(e) => {
            error!("[TauriIpc] Failed to serialize message: {e}");
        }
    }
}
