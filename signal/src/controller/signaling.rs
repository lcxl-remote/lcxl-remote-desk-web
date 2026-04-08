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
#[get("/signaling")]
pub async fn open_signaling_handle(
    req: HttpRequest,
    query_res: Result<web::Query<VersionInfo>, actix_web::Error>,
    connection_map: web::Data<SharedConnectionMap>,
    session: Session,
    stream: web::Payload,
    turn_api_state: web::Data<TurnApiState>,
    validator_opt: Option<web::Data<Arc<dyn NodeTokenValidator>>>,
) -> Result<HttpResponse, actix_web::Error> {
    info!("Incoming signaling request: {} {}", req.method(), req.uri());

    let query = match query_res {
        Ok(q) => q,
        Err(e) => {
            error!("Failed to parse signaling query params: {}", e);
            return Err(e);
        }
    };

    let mut user = None;

    // Check token-based node authentication first
    if let Some(token) = &query.0.token {
        if let Some(validator) = &validator_opt {
            if validator.validate_node_token(token).await {
                user = Some(CurrentUser::new_admin("server_node"));
                info!("Node token validated successfully");
            } else {
                log::warn!("Invalid node token provided");
            }
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

    let version_info = query.0.clone();
    let ip = req
        .connection_info()
        .realip_remote_addr()
        .map(|s| s.to_string());

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
            turn_api_state.into_inner().as_ref().settings.clone(),
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
