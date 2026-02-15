use crate::{error::DeskSignalError, model::SharedSessionMap, service::SignalingContext, version};
use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::service::SessionExt;
use desk_signal_facade::model::{
    signal::{ForwardSignalingSender, RemoteDeskTypeEnum, SignalingModel, SignalingType},
    terminal::{ListTerminalPath, StartTerminalSession, TerminalList},
    version::VersionInfo,
};
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
#[get("/terminals/{session_id}")]
pub async fn list_terminal(
    session_map: web::Data<SharedSessionMap>,
    path: web::Path<ListTerminalPath>,
) -> Result<HttpResponse, DeskSignalError> {
    let response = {
        let session_map = session_map.read().await;
        if let Some(session) = session_map.get(&path.session_id) {
            session
                .request_peer_with_callback::<()>(SignalingType::ListTerminal, None, None)
                .await?
        } else {
            return DeskSignalError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Session {} not found", path.session_id),
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
    summary = "Open terminal session",
    params(StartTerminalSession),
    responses(
        (status = 200, description = "return websocket stream"),
    ),
)]
#[get("/terminal")]
pub async fn open_terminal_session(
    req: HttpRequest,
    query_list: web::Query<StartTerminalSession>,
    session_map: web::Data<SharedSessionMap>,
    session: Session,
    stream: web::Payload,
) -> Result<HttpResponse, actix_web::Error> {
    let user_opt = session.get_current_user()?;

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

    let desk_session_id = if let Some(desk_session_id) = &query_list.session_id {
        desk_session_id.clone()
    } else {
        return Err(actix_web::error::ErrorBadRequest(
            "No desk session id provided",
        ));
    };

    info!("Proxying terminal session to desk: {}", desk_session_id);
    let (res, session, stream) = actix_ws::handle(&req, stream)?;
    let stream = stream
        .aggregate_continuations()
        .max_continuation_size(2_usize.pow(20));

    let start_terminal_session = query_list.clone().into_inner();

    let session_map_clone = session_map.clone();
    let ip = req
        .connection_info()
        .realip_remote_addr()
        .map(|s| s.to_string());

    rt::spawn(async move {
        // the web socket is from browser
        let client_version_info = VersionInfo::new(
            desk_server_version::SERVER_API_VERSION,
            version::SIGNAL_BUILD_NUMBER,
            version::SIGNAL_COMMIT_HASH.to_owned(),
            RemoteDeskTypeEnum::Browser,
            None,
        );

        log::info!("Handling terminal proxy signaling");
        let random_uuid = Uuid::new_v4();
        let session_id = String::from(random_uuid);
        // Handle signaling logic here
        let mut signaling_context = match SignalingContext::init(
            session_id,
            client_version_info,
            session_map_clone,
            session,
            user,
            ip,
        )
        .await
        {
            Ok(context) => context,
            Err(e) => {
                error!("Error handling terminal proxy signaling: {:?}", e);
                return;
            }
        };

        // send start terminal command
        let start_terminal_command = match SignalingModel::new_request(
            SignalingType::StartTerminal,
            Some(desk_session_id),
            Some(&start_terminal_session),
        ) {
            Ok(command) => command,
            Err(e) => {
                error!("Error creating start terminal command: {:?}", e);
                return;
            }
        };
        if let Err(e) = signaling_context
            .send_request(&start_terminal_command)
            .await
        {
            error!("Error sending start terminal command: {:?}", e);
            return;
        }

        let result = signaling_context.do_handle_signaling(stream).await;
        if let Err(e) = result {
            error!("Error handling signaling: {:?}", e);
        } else {
            info!("Signaling handled successfully");
        }
        // TODO close terminal
    });

    return Ok(res);
}
