use std::sync::Arc;

use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_signal_facade::{model::version::VersionInfo, service::NodeTokenValidator};
use desk_turn::model::TurnApiState;
use log::{error, info};

use crate::{model::SharedConnectionMap, service::handle_signaling};

#[utoipa::path(
    summary = "Open Signaling Handle, return websocket stream. NOTE: The OpenAPI generated typescript service is not right.",
    params(VersionInfo),
    responses(
        (status = 200, description = "return websocket stream"),
    ),
)]
#[get("/api/desk/signaling")]
pub async fn open_signaling_handle(
    req: HttpRequest,
    query: Option<web::Query<VersionInfo>>,
    connection_map: web::Data<SharedConnectionMap>,
    session: Session,
    stream: web::Payload,
    turn_api_state: Option<web::Data<TurnApiState>>,
    validator_opt: Option<web::Data<Arc<dyn NodeTokenValidator>>>,
) -> Result<HttpResponse, actix_web::Error> {
    info!("Incoming signaling request: {} {}", req.method(), req.uri());

    let version_info_opt = query.map(|q| q.into_inner());
    let mut user = None;

    // Check token-based node authentication first
    if let Some(ref vi) = version_info_opt
        && let Some(token) = &vi.token
            && let Some(validator) = &validator_opt {
                if validator.validate_node_token(token).await {
                    user = Some(CurrentUser::new_admin("server_node"));
                    info!("Node token validated successfully");
                } else {
                    log::warn!("Invalid node token provided");
                }
            }

    // Fallback to session authentication if no valid token
    let user = if let Some(u) = user {
        u
    } else {
        let user_opt = session.get_current_user::<CurrentUser>()?;
        if let Some(u) = user_opt {
            u
        } else {
            log::warn!("User not logged in and no valid node token provided");
            return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
        }
    };

    info!("User {} is signaling", user.name);

    let (res, session, stream) = actix_ws::handle(&req, stream)?;

    let stream = stream
        .aggregate_continuations()
        // aggregate continuation frames up to 1MiB
        .max_continuation_size(2_usize.pow(20));

    let version_info = version_info_opt.unwrap_or_else(|| VersionInfo {
        api_version: desk_server_version::SERVER_API_VERSION,
        build_number: crate::version::SIGNAL_BUILD_NUMBER,
        commit_hash: crate::version::SIGNAL_COMMIT_HASH.to_string(),
        remote_desk_type: desk_signal_facade::model::signal::RemoteDeskTypeEnum::Browser,
        operation_system: desk_signal_facade::model::os::OperationSystemEnum::default(),
        display_name: Some(user.name.clone()),
        client_id: None,
        token: None,
    });

    let ip = req
        .connection_info()
        .realip_remote_addr()
        .map(|s| s.to_string());
    let turn_settings = turn_api_state
        .as_ref()
        .map(|state| state.as_ref().settings.clone());

    // start task but don't wait for it
    rt::spawn(async move {
        // receive messages from websocket
        let result = handle_signaling(
            version_info,
            stream,
            connection_map,
            session,
            user,
            ip,
            turn_settings,
        )
        .await;
        if let Err(e) = result {
            error!("Error handling signaling: {}", e);
        } else {
            info!("Signaling handle is finished");
        }
    });

    // respond immediately with response connected to WS session
    Ok(res)
}
