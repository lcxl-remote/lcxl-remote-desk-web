use crate::{error::DeskSignalError, model::SharedConnectionMap, service::SignalingContext, version};
use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_signal_facade::model::{
    signal::{ForwardSignalingSender, RemoteDeskTypeEnum, SignalingModel, SignalingType},
    terminal::{ListTerminalPath, StartTerminalPath, StartTerminalSession, TerminalList},
    version::VersionInfo,
};
use desk_turn::model::TurnApiState;
use desk_utils::error::DeskErrorCode;
use log::{error, info};

use uuid::Uuid;

#[utoipa::path(
    summary = "List terminal",
    params(ListTerminalPath),
    responses(
        (status = 200, description = "return terminal command list", body = TerminalList),

    ),
)]
#[get("/terminals/{connection_id}")]
pub async fn list_terminal(
    connection_map: web::Data<SharedConnectionMap>,
    path: web::Path<ListTerminalPath>,
) -> Result<HttpResponse, DeskSignalError> {
    let response = {
        let connection_map = connection_map.read().await;
        if let Some(connection) = connection_map.get(&path.connection_id) {
            connection
                .request_peer_with_callback::<()>(SignalingType::ListTerminal, None, None)
                .await?
        } else {
            return DeskSignalError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Connection {} is not found to list terminal", path.connection_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state {
        if response_state.error_code != 0 {
            return DeskSignalError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }
    }

    let terminal_list_response: TerminalList = response.get_data()?;
    Ok(HttpResponse::Ok().json(terminal_list_response))
}

#[utoipa::path(
    summary = "Open terminal connection",
    params(StartTerminalSession, StartTerminalPath),
    responses(
        (status = 200, description = "return websocket stream"),
    ),
)]
#[get("/terminal/{connection_id}")]
pub async fn open_terminal_session(
    req: HttpRequest,
    path: web::Path<StartTerminalPath>,
    query_list: web::Query<StartTerminalSession>,
    connection_map: web::Data<SharedConnectionMap>,
    session: Session,
    stream: web::Payload,
    turn_api_state: web::Data<TurnApiState>,
) -> Result<HttpResponse, actix_web::Error> {
    let user_opt = session.get_current_user::<CurrentUser>()?;

    let user = if let Some(user) = user_opt {
        user
    } else {
        return Err(actix_web::error::ErrorUnauthorized("User not logged in"));
    };
    if query_list.command.is_empty() {
        return Err(actix_web::error::ErrorBadRequest(
            "No terminal command provided",
        ));
    }

    let to_connection_id = path.connection_id.clone();

    info!("Proxying terminal connection to desk: {}", to_connection_id);
    let (res, ws_session, stream) = actix_ws::handle(&req, stream)?;
    let stream = stream
        .aggregate_continuations()
        .max_continuation_size(2_usize.pow(20));

    let start_terminal_session = query_list.clone().into_inner();

    let connection_map_clone = connection_map.clone();
    let ip = req
        .connection_info()
        .realip_remote_addr()
        .map(|s| s.to_string());

    // the web socket is from browser
    let client_version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        version::SIGNAL_BUILD_NUMBER,
        version::SIGNAL_COMMIT_HASH.to_owned(),
        RemoteDeskTypeEnum::Browser,
        None,
        None,
    );

    let random_uuid = Uuid::new_v4();
    let connection_id = String::from(random_uuid);
    // Handle signaling logic here
    let mut signaling_context = SignalingContext::init(
        connection_id,
        client_version_info,
        connection_map_clone,
        ws_session,
        user,
        ip,
        turn_api_state.into_inner().as_ref().settings.clone(),
    )
    .await?;

    // send start terminal command
    let start_terminal_command = SignalingModel::new_request(
        SignalingType::StartTerminal,
        Some(to_connection_id.clone()),
        Some(&start_terminal_session),
    )?;
    signaling_context
        .forward_to_peer(&start_terminal_command, false)
        .await?;
    signaling_context
        .connection_state
        .terminal_connection_ids
        .write()
        .await
        .insert(signaling_context.connection_state.model.connection_id.clone());

    log::info!(
        "Sent start terminal command from {} to peer: {}",
        signaling_context.connection_state.model.connection_id,
        to_connection_id
    );
    rt::spawn(async move {
        let result = signaling_context.do_handle_signaling(stream).await;
        if let Err(e) = result {
            error!("Error handling terminal signaling: {}", e);
        } else {
            info!("Terminal signaling handle is finished");
        }
        // send close terminal command
        let close_terminal_command = SignalingModel::new_request::<()>(
            SignalingType::CloseTerminal,
            Some(to_connection_id.clone()),
            None,
        );

        if let Ok(command) = close_terminal_command {
            signaling_context
                .connection_state
                .terminal_connection_ids
                .write()
                .await
                .remove(&signaling_context.connection_state.model.connection_id);
            if let Err(e) = signaling_context.forward_to_peer(&command, false).await {
                error!("Failed to send close terminal command: {}", e);
            } else {
                info!("Sent close terminal command to peer: {}", to_connection_id);
            }
        }
    });

    return Ok(res);
}
