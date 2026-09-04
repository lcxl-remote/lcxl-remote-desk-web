use std::sync::Arc;

use actix_web::web;
use actix_ws::{AggregatedMessageStream, Session};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::{
    error::DeskSignalFacadeError,
    model::{
        connection::SharedConnectionMap,
        signal::{RemoteDeskTypeEnum, TurnProvider},
        version::VersionInfo,
    },
    service::{DeviceCodeService, SignalingHandler},
};
use uuid::Uuid;

use crate::error::DeskSignalError;

struct SignalDeviceCodeService;

impl DeviceCodeService for SignalDeviceCodeService {
    async fn get_or_create_device_code(
        &self,
        client_id: &str,
    ) -> Result<Option<String>, DeskSignalFacadeError> {
        let db = crate::db::get_db();
        use crate::entity::device_code;
        use sea_orm::*;

        let db_model_opt = device_code::Entity::find()
            .filter(device_code::Column::ClientId.eq(client_id.to_string()))
            .one(db)
            .await
            .map_err(|e| {
                DeskSignalFacadeError::new_custom_error(
                    desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
                    &e.to_string(),
                )
            })?;

        if let Some(db_model) = db_model_opt {
            Ok(Some(db_model.device_code))
        } else {
            let new_code = desk_utils::string::generate_device_code(6);
            let new_model = device_code::ActiveModel {
                client_id: Set(client_id.to_string()),
                device_code: Set(new_code.clone()),
                created_at: Set(chrono::Utc::now()),
                updated_at: Set(chrono::Utc::now()),
                ..Default::default()
            };

            if let Err(e) = new_model.insert(db).await {
                log::error!("Failed to generate device_code: {}", e);
                Ok(None)
            } else {
                Ok(Some(new_code))
            }
        }
    }
}

/// Removes a `connection_id → device_code` binding from the usage map when the
/// signaling connection ends, regardless of how `handle_signaling` returns.
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

/// Resolve the single-account auth context for a connection's adjudicated role on
/// the OSS signal. Roles are already server-adjudicated upstream (a `Server` only
/// via a valid node token; everything else via the session cookie), so the auth
/// kind follows the role:
///
/// - `Server` — token-authenticated as the target device (it registers a device
///   code and is the control target).
/// - Everything else — the cookie-authenticated single account. This deliberately
///   includes the host's **`Support`** upstream: on a plain signal that role has
///   **no** temp-code / central-brain privileges (only the manager attaches those),
///   so it is ordinary single-account routing-only, exactly like a `Browser`. Made
///   an explicit, tested seam so the role is never silently mishandled.
pub(crate) fn single_account_auth_context(
    remote_desk_type: RemoteDeskTypeEnum,
) -> desk_signal_facade::model::auth_context::AuthContext {
    use desk_signal_facade::model::auth_context::AuthContext;
    match remote_desk_type {
        RemoteDeskTypeEnum::Server => AuthContext::token_auth(
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            crate::control_authorizer::SINGLE_ACCOUNT_TOKEN_ID,
            RemoteDeskTypeEnum::Server,
        ),
        RemoteDeskTypeEnum::Browser
        | RemoteDeskTypeEnum::Signal
        | RemoteDeskTypeEnum::Manager
        | RemoteDeskTypeEnum::Support => AuthContext::cookie(
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            remote_desk_type,
        ),
    }
}

/// Map a resolved browser identity to its `AuthContext`, the single source of truth
/// shared by the main signaling connection and the terminal WS connection (so a
/// code-session vs owner stamp can never drift between them, per the design's shared
/// resolver requirement). A redeemed code-session resolves to its own principal
/// (never the single-account owner) so the capability-ceiling stamps use the code's
/// ceiling rather than full control.
pub(crate) fn browser_auth_context(
    code_session: Option<&desk_signal_facade::model::code_session::CodeSessionCookie>,
    remote_desk_type: RemoteDeskTypeEnum,
) -> desk_signal_facade::model::auth_context::AuthContext {
    match code_session {
        Some(cs) => desk_signal_facade::model::auth_context::AuthContext::code_session(
            cs.code_session_id.clone(),
        ),
        None => single_account_auth_context(remote_desk_type),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_signaling(
    client_version_info: VersionInfo,
    stream: AggregatedMessageStream,
    connection_map: web::Data<SharedConnectionMap>,
    ws_session: Session,
    user: CurrentUser,
    ip: Option<String>,
    turn: Option<std::sync::Arc<dyn TurnProvider>>,
    conn_device_map: Option<Arc<crate::turn_usage::ConnectionDeviceMap>>,
    code_session: Option<desk_signal_facade::model::code_session::CodeSessionCookie>,
) -> Result<(), DeskSignalError> {
    log::info!("Handling signaling");
    let random_uuid = Uuid::new_v4();
    let connection_id = String::from(random_uuid);

    let device_code_service = SignalDeviceCodeService;
    let device_code = if client_version_info.remote_desk_type == RemoteDeskTypeEnum::Server {
        if let Some(client_id) = &client_version_info.client_id {
            device_code_service
                .get_or_create_device_code(client_id)
                .await?
        } else {
            None
        }
    } else {
        None
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

    // Signal is the OSS single-account central brain: it resolves the connection's
    // identity so its control-frame authorizer can stamp a trusted actor/target.
    // A capability-scoped code-session resolves to its own principal (never the
    // single-account owner) so the RequestRemoteAccess authorizer stamps the redeemed
    // code's ceiling rather than full control.
    let auth_context =
        browser_auth_context(code_session.as_ref(), client_version_info.remote_desk_type);

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
            crate::device_assistant_gate::global_device_assistant_gate(),
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
        connection_map,
        ws_session,
        user,
        ip,
        turn,
        device_code,
        auth_context,
        desk_signal_facade::service::CredentialPolicy::Plain,
        desk_server_version::SERVER_API_VERSION,
    )
    .await?
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
    .with_remote_access_admission_authorizer(remote_access_control.clone())
    .with_host_remote_access_controller(remote_access_control);

    handler.do_handle_signaling(stream).await?;
    Ok(())
}

pub type SignalingContext<T> = SignalingHandler<T>;

#[cfg(test)]
mod tests {
    use super::single_account_auth_context;
    use desk_signal_facade::model::auth_context::AuthKind;
    use desk_signal_facade::model::signal::RemoteDeskTypeEnum;

    #[test]
    fn server_role_is_token_authenticated_target() {
        let ctx = single_account_auth_context(RemoteDeskTypeEnum::Server);
        assert_eq!(ctx.auth_kind, AuthKind::TokenAuth);
        assert_eq!(ctx.remote_desk_type, RemoteDeskTypeEnum::Server);
        assert_eq!(
            ctx.user_id,
            Some(crate::control_authorizer::SINGLE_ACCOUNT_USER_ID)
        );
    }

    #[test]
    fn support_role_is_routing_only_single_account() {
        // On a plain signal the host's Support upstream carries no central-brain /
        // temp-code privileges: it resolves to the cookie-authenticated single
        // account, exactly like a Browser, and never token-auth (so it binds no
        // device and issues no code). This locks that the role is handled, not
        // silently absorbed or mistaken for a Server.
        let support = single_account_auth_context(RemoteDeskTypeEnum::Support);
        assert_eq!(support.auth_kind, AuthKind::CookieAuth);
        assert_eq!(support.remote_desk_type, RemoteDeskTypeEnum::Support);
        assert_eq!(support.bound_device_id, None);

        let browser = single_account_auth_context(RemoteDeskTypeEnum::Browser);
        // Same auth kind / account as a Browser — routing-only parity.
        assert_eq!(support.auth_kind, browser.auth_kind);
        assert_eq!(support.user_id, browser.user_id);
    }
}
