use crate::host_access_status::HostAccessStatusCommand;
use awc::Client;
use desk_input_injection::model::host_control::{
    HostControlEventType, PrivateScreenCommand, WhiteboardCommand,
};
use futures_util::{SinkExt, StreamExt};
use lcxl_remote_desk_server::{
    ServiceOp,
    host_control::{ClientRole, HostControlMessage, ServiceOpKind},
    model::security_approval::{SecurityApprovalCommand, SecurityApprovalRequest},
};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeBridgeEvent {
    Ready {
        token: String,
        locale: String,
        locale_persisted: bool,
    },
    LocaleChanged {
        locale: String,
    },
}

/// Runs the IPC WebSocket client loop, reconnecting indefinitely on disconnect.
///
/// The same loop serves both portable (connect to embedded server's
/// `/ws/tauri_ipc`) and service-shell (connect to daemon's `/ws/tauri_ipc`)
/// configurations — only the URL differs. `state_rx` is `Some` when the GUI
/// manager owns the state-event channel and wants its events forwarded over ws
/// (service-shell mode); for portable mode in the transitional period, the
/// channel is consumed by the embedded server directly so we pass `None`.
pub async fn run_ipc_loop(
    daemon_ws_url: String,
    ipc_token: String,
    ps_cmd_tx: std::sync::mpsc::Sender<PrivateScreenCommand>,
    wb_cmd_tx: std::sync::mpsc::Sender<WhiteboardCommand>,
    sa_tx: std::sync::mpsc::Sender<SecurityApprovalCommand>,
    svc_op_tx: std::sync::mpsc::SyncSender<ServiceOp>,
    host_access_tx: std::sync::mpsc::Sender<HostAccessStatusCommand>,
    mut state_rx: Option<UnboundedReceiver<HostControlEventType>>,
    token_holder: Arc<Mutex<Option<String>>>,
    native_bridge_tx: std::sync::mpsc::Sender<NativeBridgeEvent>,
) {
    let ws_url = format!("{daemon_ws_url}?token={ipc_token}");

    loop {
        log::info!("[IpcClient] Connecting to {daemon_ws_url}...");
        let client = Client::default();
        match client.ws(&ws_url).connect().await {
            Ok((_resp, framed)) => {
                log::info!("[IpcClient] Connected");
                let (mut sink, mut stream) = framed.split();

                // Announce admin status to daemon / hub.
                let is_admin = desk_utils::permission::is_admin();
                let ready_msg = HostControlMessage::Ready {
                    role: ClientRole::Tauri,
                    is_admin: Some(is_admin),
                };
                let ready_json = match serde_json::to_string(&ready_msg) {
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
                                    match serde_json::from_str::<HostControlMessage>(&text) {
                                        Ok(msg) => handle_server_msg(
                                            msg,
                                            &ps_cmd_tx,
                                            &wb_cmd_tx,
                                            &sa_tx,
                                            &svc_op_tx,
                                            &host_access_tx,
                                            &token_holder,
                                            Some(&native_bridge_tx),
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
                                    log::info!("[IpcClient] Close frame from server: {reason:?}");
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
                        state_event = next_state(&mut state_rx) => {
                            let Some(event) = state_event else { continue };
                            let Some(msg) = map_state_event(event) else { continue };
                            let Ok(json) = serde_json::to_string(&msg) else { continue };
                            if sink.send(awc::ws::Message::Text(json.into())).await.is_err() {
                                log::warn!("[IpcClient] Failed to send state event");
                                break 'session;
                            }
                        }
                    }
                }

                log::warn!("[IpcClient] Session ended, reconnecting in 3 s...");
                // The server-side hub may have lost (or restarted past) any
                // pending approvals from this session, so no Finish will ever
                // match the req_ids we already reported to the manager.
                // Telling it to reset releases always-on-top instead of
                // leaving the window pinned until the next live dialog.
                let _ = sa_tx.send(SecurityApprovalCommand::Reset);
                let _ = host_access_tx.send(HostAccessStatusCommand::Reset);
            }
            Err(e) => {
                log::warn!("[IpcClient] Connection failed: {e:?}, retrying in 3 s...");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// Helper: returns a future that resolves only when the optional state receiver
/// has a message; pends forever if the receiver is `None`.
async fn next_state(
    rx: &mut Option<UnboundedReceiver<HostControlEventType>>,
) -> Option<HostControlEventType> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn handle_server_msg(
    msg: HostControlMessage,
    ps_cmd_tx: &std::sync::mpsc::Sender<PrivateScreenCommand>,
    wb_cmd_tx: &std::sync::mpsc::Sender<WhiteboardCommand>,
    sa_tx: &std::sync::mpsc::Sender<SecurityApprovalCommand>,
    svc_op_tx: &std::sync::mpsc::SyncSender<ServiceOp>,
    host_access_tx: &std::sync::mpsc::Sender<HostAccessStatusCommand>,
    token_holder: &Arc<Mutex<Option<String>>>,
    native_bridge_tx: Option<&std::sync::mpsc::Sender<NativeBridgeEvent>>,
) {
    match msg {
        HostControlMessage::TauriToken { token } => {
            log::info!("[IpcClient] Received TauriToken from server");
            *token_holder.lock().unwrap() = Some(token);
        }
        HostControlMessage::NativeBridgeReady {
            token,
            locale,
            locale_persisted,
        } => {
            if let Some(tx) = native_bridge_tx {
                let _ = tx.send(NativeBridgeEvent::Ready {
                    token,
                    locale,
                    locale_persisted,
                });
            }
        }
        HostControlMessage::GlobalLocaleChanged { locale } => {
            if let Some(tx) = native_bridge_tx {
                let _ = tx.send(NativeBridgeEvent::LocaleChanged { locale });
            }
        }
        HostControlMessage::PrivateScreenShow {
            connection_id,
            request_id,
        } => {
            let _ = ps_cmd_tx.send(PrivateScreenCommand::Show {
                connection_id,
                request_id,
            });
        }
        HostControlMessage::PrivateScreenHide {
            connection_id,
            request_id,
        } => {
            let _ = ps_cmd_tx.send(PrivateScreenCommand::Hide {
                connection_id,
                request_id,
            });
        }
        HostControlMessage::WhiteboardShow { connection_id } => {
            let _ = wb_cmd_tx.send(WhiteboardCommand::Show(connection_id));
        }
        HostControlMessage::WhiteboardDraw { message, .. } => {
            let json_str = serde_json::to_string(&message).unwrap_or_default();
            let _ = wb_cmd_tx.send(WhiteboardCommand::DrawMessage(json_str));
        }
        HostControlMessage::WhiteboardHide { connection_id } => {
            let _ = wb_cmd_tx.send(WhiteboardCommand::Hide(connection_id));
        }
        HostControlMessage::SecurityApprovalRequest {
            req_id,
            permission_type,
            from_connection_id,
        } => {
            let req = SecurityApprovalRequest {
                req_id,
                permission_type,
                from_connection_id,
            };
            let _ = sa_tx.send(SecurityApprovalCommand::Request(req));
        }
        HostControlMessage::SecurityApprovalFinished { req_id } => {
            let _ = sa_tx.send(SecurityApprovalCommand::Finish { req_id });
        }
        HostControlMessage::ServiceOp {
            op,
            install_path,
            install_idd_driver,
        } => {
            let svc_op = match op {
                ServiceOpKind::Install => install_path.map(|path| ServiceOp::Install {
                    install_path: path,
                    install_idd_driver,
                }),
                ServiceOpKind::Uninstall => Some(ServiceOp::Uninstall),
            };
            if let Some(svc_op) = svc_op
                && let Err(e) = svc_op_tx.try_send(svc_op)
            {
                log::warn!("[IpcClient] ServiceOp channel full: {e}");
            }
        }
        HostControlMessage::HostAccessSnapshot { snapshot } => {
            let _ = host_access_tx.send(HostAccessStatusCommand::Snapshot(snapshot));
        }
        // Aggregator → forwarder messages: ignored on Tauri client.
        HostControlMessage::SecurityApprovalSubmit { .. }
        | HostControlMessage::SecurityApprovalCancel { .. }
        | HostControlMessage::PrivateScreenStateChangedToWorker { .. } => {}
        // Client → server frames; receiving is unexpected but harmless.
        HostControlMessage::Ready { .. }
        | HostControlMessage::PrivateScreenStateChanged { .. }
        | HostControlMessage::SecurityApprovalResolved { .. } => {}
    }
}

fn map_state_event(event: HostControlEventType) -> Option<HostControlMessage> {
    match event {
        HostControlEventType::PrivateScreenVisibleChanged {
            connection_id,
            request_id,
            visible,
        } => Some(HostControlMessage::PrivateScreenStateChanged {
            connection_id,
            request_id,
            visible,
            is_supported: true,
            error_msg: None,
        }),
        HostControlEventType::PrivateScreenUnknownError {
            connection_id: Some(connection_id),
            request_id,
            message,
        } => Some(HostControlMessage::PrivateScreenStateChanged {
            connection_id,
            request_id,
            visible: false,
            is_supported: true,
            error_msg: Some(message),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke test: build the message dispatch pipeline with no panics.
    // Ensures HostControlMessage <-> mpsc senders compile and route correctly.
    #[test]
    fn handle_server_msg_dispatches_private_screen() {
        let (ps_tx, ps_rx) = std::sync::mpsc::channel::<PrivateScreenCommand>();
        let (wb_tx, _wb_rx) = std::sync::mpsc::channel::<WhiteboardCommand>();
        let (sa_tx, _sa_rx) = std::sync::mpsc::channel::<SecurityApprovalCommand>();
        let (svc_tx, _svc_rx) = std::sync::mpsc::sync_channel::<ServiceOp>(1);
        let (status_tx, _status_rx) = std::sync::mpsc::channel();
        let token_holder = Arc::new(Mutex::new(None));

        handle_server_msg(
            HostControlMessage::PrivateScreenShow {
                connection_id: "c1".to_string(),
                request_id: "r1".to_string(),
            },
            &ps_tx,
            &wb_tx,
            &sa_tx,
            &svc_tx,
            &status_tx,
            &token_holder,
            None,
        );
        match ps_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
        {
            PrivateScreenCommand::Show {
                connection_id,
                request_id,
            } => {
                assert_eq!(connection_id, "c1");
                assert_eq!(request_id, "r1");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn handle_server_msg_writes_token_holder() {
        let (ps_tx, _) = std::sync::mpsc::channel();
        let (wb_tx, _) = std::sync::mpsc::channel();
        let (sa_tx, _) = std::sync::mpsc::channel();
        let (svc_tx, _) = std::sync::mpsc::sync_channel::<ServiceOp>(1);
        let (status_tx, _) = std::sync::mpsc::channel();
        let token_holder = Arc::new(Mutex::new(None));

        handle_server_msg(
            HostControlMessage::TauriToken {
                token: "tok-xyz".to_string(),
            },
            &ps_tx,
            &wb_tx,
            &sa_tx,
            &svc_tx,
            &status_tx,
            &token_holder,
            None,
        );
        assert_eq!(token_holder.lock().unwrap().as_deref(), Some("tok-xyz"));
    }

    #[test]
    fn handle_server_msg_dispatches_host_access_snapshot() {
        let (ps_tx, _) = std::sync::mpsc::channel();
        let (wb_tx, _) = std::sync::mpsc::channel();
        let (sa_tx, _) = std::sync::mpsc::channel();
        let (svc_tx, _) = std::sync::mpsc::sync_channel::<ServiceOp>(1);
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let token_holder = Arc::new(Mutex::new(None));
        let snapshot = lcxl_remote_desk_server::host_control::HostAccessSnapshot {
            epoch: "epoch-1".to_string(),
            revision: 4,
            indicator_enabled: false,
            total_session_count: 0,
            sessions: Vec::new(),
            remote_access: lcxl_remote_desk_server::host_control::HostRemoteAccessStatus::default(),
        };

        handle_server_msg(
            HostControlMessage::HostAccessSnapshot {
                snapshot: snapshot.clone(),
            },
            &ps_tx,
            &wb_tx,
            &sa_tx,
            &svc_tx,
            &status_tx,
            &token_holder,
            None,
        );

        match status_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .expect("status command")
        {
            HostAccessStatusCommand::Snapshot(received) => assert_eq!(received, snapshot),
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn native_bridge_messages_are_forwarded_to_the_shell_event_loop() {
        let (ps_tx, _) = std::sync::mpsc::channel();
        let (wb_tx, _) = std::sync::mpsc::channel();
        let (sa_tx, _) = std::sync::mpsc::channel();
        let (svc_tx, _) = std::sync::mpsc::sync_channel::<ServiceOp>(1);
        let (status_tx, _) = std::sync::mpsc::channel();
        let token_holder = Arc::new(Mutex::new(None));
        let (native_tx, native_rx) = std::sync::mpsc::channel();

        handle_server_msg(
            HostControlMessage::NativeBridgeReady {
                token: "session-token".to_string(),
                locale: "en-US".to_string(),
                locale_persisted: true,
            },
            &ps_tx,
            &wb_tx,
            &sa_tx,
            &svc_tx,
            &status_tx,
            &token_holder,
            Some(&native_tx),
        );

        assert_eq!(
            native_rx.recv().unwrap(),
            NativeBridgeEvent::Ready {
                token: "session-token".to_string(),
                locale: "en-US".to_string(),
                locale_persisted: true,
            }
        );
    }

    #[test]
    fn handle_server_msg_dispatches_service_op_install() {
        let (ps_tx, _) = std::sync::mpsc::channel();
        let (wb_tx, _) = std::sync::mpsc::channel();
        let (sa_tx, _) = std::sync::mpsc::channel();
        let (svc_tx, svc_rx) = std::sync::mpsc::sync_channel::<ServiceOp>(1);
        let (status_tx, _) = std::sync::mpsc::channel();
        let token_holder = Arc::new(Mutex::new(None));

        handle_server_msg(
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Install,
                install_path: Some("C:/foo".to_string()),
                install_idd_driver: true,
            },
            &ps_tx,
            &wb_tx,
            &sa_tx,
            &svc_tx,
            &status_tx,
            &token_holder,
            None,
        );
        match svc_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
        {
            ServiceOp::Install {
                install_path,
                install_idd_driver,
            } => {
                assert_eq!(install_path, "C:/foo");
                assert!(
                    install_idd_driver,
                    "IDD flag must reach the elevation sender"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_state_event_translates_visibility_change() {
        let msg = map_state_event(HostControlEventType::PrivateScreenVisibleChanged {
            connection_id: "c1".to_string(),
            request_id: Some("r1".to_string()),
            visible: true,
        })
        .expect("should produce");
        match msg {
            HostControlMessage::PrivateScreenStateChanged {
                connection_id,
                request_id,
                visible,
                ..
            } => {
                assert_eq!(connection_id, "c1");
                assert_eq!(request_id.as_deref(), Some("r1"));
                assert!(visible);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
