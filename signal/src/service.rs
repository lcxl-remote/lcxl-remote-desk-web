use desk_signal_facade::model::peer::SignalingPeer;
use std::sync::Arc;

use actix_web::web;
use actix_ws::{AggregatedMessageStream, Session};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::{
    model::{connection::SharedConnectionMap, signal::TurnProvider},
    service::SignalingHandler,
};
use uuid::Uuid;

use crate::error::DeskSignalError;

/// Removes a `connection_id → device_code` binding from the usage map when the
/// signaling connection ends, regardless of how `handle_signaling` returns.
#[derive(Clone)]
struct ConnectionDeviceGuard {
    map: Arc<crate::turn_usage::ConnectionDeviceMap>,
    connection_id: String,
}

impl Drop for ConnectionDeviceGuard {
    fn drop(&mut self) {
        let map = self.map.clone();
        let connection_id = self.connection_id.clone();
        // The map is async-locked; hop onto the runtime to remove the entry.
        // Best-effort: a missed removal only leaves a stale entry that the next
        // collector pass tolerates (it resolves by lookup, not by liveness).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                map.write().await.remove(&connection_id);
            });
        }
    }
}

/// Wakes any owner-confirmed exec waiters bound to a signaling peer when that
/// peer disconnects. An operator disconnect before approval therefore cannot
/// leave a command eligible for later dispatch, while a target disconnect wakes
/// the result waiter into the conservative unknown outcome.
#[derive(Clone)]
struct CentralPendingConnectionGuard {
    pending: Arc<crate::agent_exec::SignalAgentExecPending>,
    computer_use_readiness: Arc<crate::computer_use_readiness::ComputerUseReadinessCache>,
    remote_tool_pending: Arc<crate::remote_tool_edge::SignalRemoteToolPendingStore>,
    computer_action_pending: Arc<crate::remote_tool_edge::SignalComputerActionPendingStore>,
    connection_id: String,
}

impl Drop for CentralPendingConnectionGuard {
    fn drop(&mut self) {
        self.pending.drain_for_connection(&self.connection_id);
        self.computer_use_readiness
            .remove_connection(&self.connection_id);
        self.remote_tool_pending
            .drain_for_connection(&self.connection_id);
        self.computer_action_pending
            .drain_for_connection(&self.connection_id);
        let connection_id = self.connection_id.clone();
        let registry = crate::exec_pty_carrier::global_exec_pty_carriers();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                registry.disconnect_target(&connection_id).await;
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_signaling(
    client_version_info: SignalingPeer,
    stream: AggregatedMessageStream,
    connection_map: web::Data<SharedConnectionMap>,
    ws_session: Session,
    user: CurrentUser,
    ip: Option<String>,
    turn: Option<std::sync::Arc<dyn TurnProvider>>,
    conn_device_map: Option<Arc<crate::turn_usage::ConnectionDeviceMap>>,
    code_session: Option<desk_signal_facade::model::code_session::CodeSessionCookie>,
    auth_context: desk_signal_facade::model::auth_context::AuthContext,
) -> Result<(), DeskSignalError> {
    log::info!("Handling signaling");
    let random_uuid = Uuid::new_v4();
    let connection_id = String::from(random_uuid);

    let (registration, device_code) =
        if let Some(registration) = client_version_info.server.as_ref() {
            let (guard, code) = crate::device_code_lifecycle::global()
                .prepare_registration(crate::db::get_db(), &registration.client_id)
                .await?;
            (Some(guard), Some(code))
        } else {
            (None, None)
        };

    // Publish the connection's device binding before the handler enters the
    // connection map (so the TURN usage collector can resolve bytes the moment
    // the peer can be reached). The guard removes it when the connection ends.
    let _device_guard = match (&conn_device_map, &device_code) {
        (Some(map), Some(code)) => {
            map.write()
                .await
                .insert(connection_id.clone(), code.clone());
            Some(ConnectionDeviceGuard {
                map: map.clone(),
                connection_id: connection_id.clone(),
            })
        }
        _ => None,
    };

    // The central-brain injection point authorizes the single-account Device
    // Assistant and execution surfaces.
    let computer_use_readiness =
        crate::computer_use_readiness::global_computer_use_readiness_cache();
    let _central_pending_guard = CentralPendingConnectionGuard {
        pending: crate::agent_exec::global_agent_exec_pending(),
        computer_use_readiness: computer_use_readiness.clone(),
        remote_tool_pending: crate::remote_tool_edge::global_remote_tool_pending(),
        computer_action_pending: crate::remote_tool_edge::global_computer_action_pending(),
        connection_id: connection_id.clone(),
    };
    let control_authorizer =
        std::sync::Arc::new(crate::control_authorizer::SignalControlAuthorizer::new(
            crate::db::get_db().clone(),
            connection_map.clone(),
            crate::ai_assistant_gate::global_ai_assistant_gate(),
        ));
    let edge_exec_observer = std::sync::Arc::new(crate::agent_exec::SignalEdgeExecObserver::new(
        crate::agent_exec::global_agent_exec_pending(),
        crate::db::get_db().clone(),
    ));
    let exec_state_reply_observer =
        std::sync::Arc::new(crate::agent_exec::SignalExecStateReplyObserver::new(
            crate::agent_exec::global_agent_exec_pending(),
        ));
    let remote_tool_observer =
        std::sync::Arc::new(crate::remote_tool_edge::SignalRemoteToolObserver::new(
            crate::remote_tool_edge::global_remote_tool_pending(),
        ));
    let computer_action_observer =
        std::sync::Arc::new(crate::remote_tool_edge::SignalComputerActionObserver::new(
            crate::remote_tool_edge::global_computer_action_pending(),
            crate::db::get_db().clone(),
        ));
    // The single-account owner is stamped with full control; a code-session
    // (anonymous redeemer) is stamped with the redeemed code's ceiling via the
    // shared grant store. Both share the process-global store so a grant minted at
    // redeem time is visible here.
    let request_remote_authorizer = std::sync::Arc::new(
        crate::request_remote_authorizer::SignalRequestRemoteAuthorizer::new(
            crate::access_grant::global_access_grant_store(),
            std::sync::Arc::new(
                crate::request_remote_authorizer::DbDeviceGenerationLookup::new(
                    crate::db::get_db().clone(),
                ),
            ),
        ),
    );
    let remote_access_control =
        std::sync::Arc::new(crate::remote_access::SignalRemoteAccessControl::new(
            crate::db::get_db().clone(),
            connection_map.clone().into_inner(),
        ));

    let mut handler = SignalingHandler::init(
        connection_id,
        client_version_info,
        connection_map.clone(),
        ws_session,
        user,
        ip,
        turn,
        device_code,
        auth_context,
        desk_signal_facade::service::CredentialPolicy::Plain,
        desk_server_version::SERVER_API_VERSION,
        None,
    )
    .await?;
    // Registration and code edits are ordered only through publication, never
    // for the lifetime of the WebSocket or while closing a retired transport.
    drop(registration);
    handler = handler
        .with_control_authorizer(control_authorizer)
        .with_request_remote_authorizer(request_remote_authorizer)
        .with_computer_use_readiness_observer(std::sync::Arc::new(
            crate::computer_use_readiness::SignalComputerUseReadinessObserver::new(
                computer_use_readiness,
            ),
        ))
        .with_edge_exec_observer(edge_exec_observer)
        .with_binary_frame_observer(crate::exec_pty_carrier::global_exec_pty_carriers())
        .with_exec_state_reply_observer(exec_state_reply_observer)
        .with_remote_tool_observer(remote_tool_observer)
        .with_computer_action_observer(computer_action_observer)
        .with_file_recovery_observer(desk_signal_facade::service::file_recovery::global_hub())
        .with_remote_access_admission_authorizer(remote_access_control.clone())
        .with_host_remote_access_controller(remote_access_control);

    handler = handler.with_fetch_connections_resolver(Arc::new(
        crate::connection_resolver::SignalFetchConnectionsResolver::new(
            connection_map,
            code_session,
            crate::access_grant::global_access_grant_store(),
            Arc::new(
                crate::request_remote_authorizer::DbDeviceGenerationLookup::new(
                    crate::db::get_db().clone(),
                ),
            ),
        ),
    ));
    let _retirement_cleanup = watch_retirement(
        handler.connection_state.lifecycle.clone(),
        _central_pending_guard.clone(),
        _device_guard.clone(),
    );
    handler.do_handle_signaling(stream).await?;
    Ok(())
}

pub type SignalingContext<T> = SignalingHandler<T>;

// An admitted handler keeps its own result fence; retirement can release its
// connection-scoped waiters without cancelling that handler future.
fn watch_retirement(
    lifecycle: Arc<desk_signal_facade::model::connection::ConnectionLifecycle>,
    pending: CentralPendingConnectionGuard,
    device: Option<ConnectionDeviceGuard>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        lifecycle.retired().await;
        drop((pending, device));
    })
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    #[tokio::test]
    async fn retired_connections_drain_while_the_original_handler_still_owns_its_guards() {
        let turn = Arc::new(crate::turn_usage::ConnectionDeviceMap::default());
        let pending = Arc::new(crate::remote_tool_edge::SignalRemoteToolPendingStore::default());
        let mut originals = Vec::new();
        let mut waiters = Vec::new();
        for round in 0..1000 {
            let id = format!("c{round}");
            turn.write().await.insert(id.clone(), "device".into());
            let guard = CentralPendingConnectionGuard {
                pending: Arc::new(crate::agent_exec::SignalAgentExecPending::new()),
                computer_use_readiness: Arc::new(
                    crate::computer_use_readiness::ComputerUseReadinessCache::default(),
                ),
                remote_tool_pending: pending.clone(),
                computer_action_pending: Arc::new(
                    crate::remote_tool_edge::SignalComputerActionPendingStore::default(),
                ),
                connection_id: id.clone(),
            };
            let device = ConnectionDeviceGuard {
                map: turn.clone(),
                connection_id: id.clone(),
            };
            let (sender, receiver) = tokio::sync::oneshot::channel();
            assert!(pending.register(format!("request-{round}"), id, sender));
            let lifecycle =
                Arc::new(desk_signal_facade::model::connection::ConnectionLifecycle::default());
            let task = watch_retirement(lifecycle.clone(), guard.clone(), Some(device.clone()));
            // Exercise retirement before and after the watcher registers its wait.
            if round % 2 == 0 {
                tokio::task::yield_now().await;
            }
            lifecycle.retire();
            task.await.unwrap();
            waiters.push(receiver);
            originals.push((guard, device));
        }
        for waiter in waiters {
            let error = waiter.await.unwrap().unwrap_err();
            assert_eq!(
                error.kind,
                desk_agent_protocol::AgentErrorKind::TargetOffline
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !turn.read().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(pending.drain_for_connection("c999"), 0);
        drop(originals);
    }
}
