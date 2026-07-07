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
use desk_turn::model::TurnSettings;
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
fn single_account_auth_context(
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

#[allow(clippy::too_many_arguments)]
pub async fn handle_signaling(
    client_version_info: VersionInfo,
    stream: AggregatedMessageStream,
    connection_map: web::Data<SharedConnectionMap>,
    ws_session: Session,
    user: CurrentUser,
    ip: Option<String>,
    turn: Option<TurnSettings>,
    conn_device_map: Option<Arc<crate::turn_usage::ConnectionDeviceMap>>,
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
    let auth_context = single_account_auth_context(client_version_info.remote_desk_type);

    // The central-brain injection points: a single-account policy decision point
    // that authorizes/orchestrates the control-end AI frames, and a collect
    // observer that feeds inbound evidence responses back into the diagnosis. Both
    // share the process-global pending store (the portable signal is single-node).
    let collect_pending = crate::diagnose_orchestrator::global_pending_store();
    let control_authorizer =
        std::sync::Arc::new(crate::control_authorizer::SignalControlAuthorizer::new(
            crate::db::get_db().clone(),
            collect_pending,
            connection_map.clone(),
        ));
    let collect_observer = std::sync::Arc::new(
        crate::diagnose_orchestrator::SignalCollectObserver::new(connection_map.clone()),
    );

    let mut handler = SignalingHandler::init(
        connection_id,
        client_version_info,
        connection_map,
        ws_session,
        user,
        ip,
        turn.map(|v| std::sync::Arc::new(v) as std::sync::Arc<dyn TurnProvider>),
        device_code,
        auth_context,
        desk_server_version::SERVER_API_VERSION,
    )
    .await?
    .with_control_authorizer(control_authorizer)
    .with_collect_observer(collect_observer);

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
