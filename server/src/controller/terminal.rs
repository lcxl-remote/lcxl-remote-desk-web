use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::service::SessionExt;
use desk_signal::{
    model::SharedSessionMap,
    service::SignalingContext,
};
use desk_signal_facade::model::{
    signal::{ForwardSignalingSender, RemoteDeskTypeEnum, SignalingModel, SignalingType},
    terminal::{ListTerminalPath, StartTerminalSession, TerminalList},
    version::VersionInfo,
};
use desk_utils::error::DeskErrorCode;
use log::{error, info};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use uuid::Uuid;

use crate::{
    error::DeskError,
    model::settings::SharedSettings,
    service::terminal::handle_terminal,
};

#[utoipa::path(
    summary = "List terminal",
    responses(
        (status = 200, description = "return terminal command list", body = TerminalList),

    ),
)]
#[get("/terminals/{session_id}")]
pub async fn list_terminal(
    session_map: web::Data<SharedSessionMap>,
    path: web::Path<ListTerminalPath>,
) -> Result<HttpResponse, DeskError> {

    let response = {
        let session_map = session_map.read().await;
        if let Some(session) = session_map.get(&path.session_id) {
            session
                .request_peer_with_callback(
                    SignalingType::ListTerminal,
                    &path.into_inner(),
                    None,
                )
                .await?
        } else {
            return DeskError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Session {} not found", path.session_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state {
        if response_state.error_code != 0 {
            return DeskError::custom_error(
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
    settings: web::Data<SharedSettings>,
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

    // Check if we are in proxy mode (desk_session_id provided)
    if let Some(desk_session_id) = &query_list.session_id {
        info!("Proxying terminal session to desk: {}", desk_session_id);
        let (res, session, stream) = actix_ws::handle(&req, stream)?;
        let stream = stream
            .aggregate_continuations()
            .max_continuation_size(2_usize.pow(20));

        let start_terminal_session = query_list.clone().into_inner();

        let session_map_clone = session_map.clone();
        let desk_session_id = desk_session_id.clone();
        let ip = req
            .connection_info()
            .realip_remote_addr()
            .map(|s| s.to_string());

        rt::spawn(async move {
            // the web socket is from browser
            let client_version_info = VersionInfo::new(
                desk_server_version::SERVER_API_VERSION,
                desk_signal::version::SIGNAL_BUILD_NUMBER,
                desk_signal::version::SIGNAL_COMMIT_HASH.to_owned(),
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
                &start_terminal_session,
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

    // Local mode
    info!(
        "User {} is starting local terminal session, command: {:?}",
        user.name, query_list.command
    );
    let terminal_command = query_list.command.clone();
    // split the terminal command into a list of arguments
    let terminal_command_list: Vec<&str> = terminal_command.split(",").collect();

    {
        // save current terminal command to settings
        settings.write().await.terminal.current_terminal = Some(
            terminal_command_list
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }

    let execute_file_path = terminal_command_list[0];
    let args_list = &terminal_command_list[1..];

    // Create a new PTY system
    let pty_system = native_pty_system();

    // Create a new PTY pair
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| {
            actix_web::error::ErrorInternalServerError(format!("Failed to open pty: {}", e))
        })?;

    let mut cmd = CommandBuilder::new(execute_file_path);
    cmd.args(args_list);

    if std::path::Path::new("Cargo.toml").exists() {
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(parent) = cwd.parent() {
                cmd.cwd(parent);
            }
        }
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| {
        actix_web::error::ErrorInternalServerError(format!("Failed to spawn command in pty: {}", e))
    })?;

    let (res, session, stream) = actix_ws::handle(&req, stream)?;

    let stream = stream
        .aggregate_continuations()
        .max_continuation_size(2_usize.pow(20));

    // start task but don't wait for it
    rt::spawn(async move {
        // receive messages from websocket
        let result = handle_terminal(settings, stream, session, user, pair.master, child).await;
        if let Err(e) = result {
            error!("Error handling terminal: {:?}", e);
        } else {
            info!("Closed terminal session successfully");
        }
    });

    // respond immediately with response connected to WS session
    Ok(res)
}
