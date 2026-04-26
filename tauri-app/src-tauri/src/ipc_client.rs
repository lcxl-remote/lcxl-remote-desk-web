use awc::Client;
use futures_util::{SinkExt, StreamExt};
use lcxl_remote_desk_server::{
    ServiceOp,
    daemon::tauri_ipc::{DaemonToTauriMsg, TauriToDaemonMsg},
    model::{
        security_approval::{
            SecurityApprovalCommand, SecurityApprovalRequest, SecurityPermissionType,
        },
    },
};
use desk_input_injection::model::host_control::{HostControlEventType, PrivateScreenCommand, WhiteboardCommand};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;

const DAEMON_WS_BASE: &str = "ws://127.0.0.1:8082/ws/tauri_ipc";

/// Runs the IPC WebSocket client loop, reconnecting indefinitely on disconnect.
pub async fn run_ipc_loop(
    ipc_token: String,
    ps_cmd_tx: std::sync::mpsc::Sender<PrivateScreenCommand>,
    wb_cmd_tx: std::sync::mpsc::Sender<WhiteboardCommand>,
    sa_tx: std::sync::mpsc::Sender<SecurityApprovalCommand>,
    svc_op_tx: std::sync::mpsc::SyncSender<ServiceOp>,
    mut state_rx: UnboundedReceiver<HostControlEventType>,
    token_holder: Arc<Mutex<Option<String>>>,
) {
    let ws_url = format!("{DAEMON_WS_BASE}?token={ipc_token}");

    loop {
        log::info!("[IpcClient] Connecting to daemon at {DAEMON_WS_BASE}...");
        let client = Client::default();
        match client.ws(&ws_url).connect().await {
            Ok((_resp, framed)) => {
                log::info!("[IpcClient] Connected");
                let (mut sink, mut stream) = framed.split();

                // Announce admin status to daemon
                let is_admin = desk_utils::permission::is_admin();
                let ready_json = match serde_json::to_string(&TauriToDaemonMsg::Ready { is_admin })
                {
                    Ok(j) => j,
                    Err(e) => {
                        log::error!("[IpcClient] Failed to serialize Ready: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        continue;
                    }
                };
                if sink
                    .send(awc::ws::Message::Text(ready_json.into()))
                    .await
                    .is_err()
                {
                    log::error!("[IpcClient] Failed to send Ready message");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }

                // Main select loop
                'session: loop {
                    tokio::select! {
                        ws_msg = stream.next() => {
                            match ws_msg {
                                Some(Ok(awc::ws::Frame::Text(bytes))) => {
                                    let text = String::from_utf8_lossy(&bytes);
                                    match serde_json::from_str::<DaemonToTauriMsg>(&text) {
                                        Ok(msg) => handle_daemon_msg(
                                            msg,
                                            &ps_cmd_tx,
                                            &wb_cmd_tx,
                                            &sa_tx,
                                            &svc_op_tx,
                                            &token_holder,
                                        ),
                                        Err(e) => {
                                            log::warn!("[IpcClient] Parse error: {e} — raw: {text}");
                                        }
                                    }
                                }
                                Some(Ok(awc::ws::Frame::Ping(data))) => {
                                    let _ = sink.send(awc::ws::Message::Pong(data)).await;
                                }
                                Some(Ok(awc::ws::Frame::Close(reason))) => {
                                    log::info!("[IpcClient] Close frame from daemon: {reason:?}");
                                    break 'session;
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => {
                                    log::error!("[IpcClient] WS error: {e}");
                                    break 'session;
                                }
                                None => {
                                    log::warn!("[IpcClient] Stream closed");
                                    break 'session;
                                }
                            }
                        }
                        state_event = state_rx.recv() => {
                            if let Some(event) = state_event {
                                if let Some(msg) = map_state_event(event) {
                                    if let Ok(json) = serde_json::to_string(&msg) {
                                        if sink.send(awc::ws::Message::Text(json.into())).await.is_err() {
                                            log::warn!("[IpcClient] Failed to send state event");
                                            break 'session;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                log::warn!("[IpcClient] Session ended, reconnecting in 3 s...");
            }
            Err(e) => {
                log::warn!("[IpcClient] Connection failed: {e:?}, retrying in 3 s...");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

fn handle_daemon_msg(
    msg: DaemonToTauriMsg,
    ps_cmd_tx: &std::sync::mpsc::Sender<PrivateScreenCommand>,
    wb_cmd_tx: &std::sync::mpsc::Sender<WhiteboardCommand>,
    sa_tx: &std::sync::mpsc::Sender<SecurityApprovalCommand>,
    svc_op_tx: &std::sync::mpsc::SyncSender<ServiceOp>,
    token_holder: &Arc<Mutex<Option<String>>>,
) {
    match msg {
        DaemonToTauriMsg::TauriToken { token } => {
            log::info!("[IpcClient] Received TauriToken from daemon");
            *token_holder.lock().unwrap() = Some(token);
        }
        DaemonToTauriMsg::PrivateScreenShow { connection_id } => {
            let _ = ps_cmd_tx.send(PrivateScreenCommand::Show(connection_id));
        }
        DaemonToTauriMsg::PrivateScreenHide { connection_id } => {
            let _ = ps_cmd_tx.send(PrivateScreenCommand::Hide(connection_id));
        }
        DaemonToTauriMsg::WhiteboardShow { connection_id } => {
            let _ = wb_cmd_tx.send(WhiteboardCommand::Show(connection_id));
        }
        DaemonToTauriMsg::WhiteboardDraw { message, .. } => {
            let json_str = serde_json::to_string(&message).unwrap_or_default();
            let _ = wb_cmd_tx.send(WhiteboardCommand::DrawMessage(json_str));
        }
        DaemonToTauriMsg::SecurityApprovalRequest {
            req_id,
            permission_type,
            from_connection_id,
        } => {
            if let Some(perm) = parse_permission_type(&permission_type) {
                let req = SecurityApprovalRequest {
                    req_id,
                    permission_type: perm,
                    from_connection_id,
                };
                let _ = sa_tx.send(SecurityApprovalCommand::Request(req));
            } else {
                log::warn!("[IpcClient] Unknown permission type: {permission_type}");
            }
        }
        DaemonToTauriMsg::ServiceOp { op, install_path } => {
            let svc_op = match op.as_str() {
                "install" => install_path.map(|path| ServiceOp::Install { install_path: path }),
                "uninstall" => Some(ServiceOp::Uninstall),
                _ => {
                    log::warn!("[IpcClient] Unknown service op: {op}");
                    None
                }
            };
            if let Some(svc_op) = svc_op {
                if let Err(e) = svc_op_tx.try_send(svc_op) {
                    log::warn!("[IpcClient] ServiceOp channel full: {e}");
                }
            }
        }
    }
}

fn map_state_event(event: HostControlEventType) -> Option<TauriToDaemonMsg> {
    match event {
        HostControlEventType::PrivateScreenVisibleChanged(connection_id, visible) => {
            Some(TauriToDaemonMsg::PrivateScreenStateChanged {
                connection_id,
                visible,
            })
        }
        _ => None,
    }
}

fn parse_permission_type(s: &str) -> Option<SecurityPermissionType> {
    match s {
        "RemoteControl" => Some(SecurityPermissionType::RemoteControl),
        "ClipboardSync" => Some(SecurityPermissionType::ClipboardSync),
        "PrivateScreen" => Some(SecurityPermissionType::PrivateScreen),
        "Whiteboard" => Some(SecurityPermissionType::Whiteboard),
        "Terminal" => Some(SecurityPermissionType::Terminal),
        "FileBrowse" => Some(SecurityPermissionType::FileBrowse),
        "FileTransfer" => Some(SecurityPermissionType::FileTransfer),
        _ => None,
    }
}
