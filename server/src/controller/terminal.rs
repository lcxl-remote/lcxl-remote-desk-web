use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::service::SessionExt;
use desk_signal_facade::model::signal::SignalingModel;
use log::{error, info};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::{
    error::DeskError,
    model::{
        settings::SharedSettings,
        terminal::{StartTerminalSession, TerminalList},
    },
    service::terminal::{fetch_terminal_list, handle_terminal},
};

#[utoipa::path(
    summary = "List terminal",
    responses(
        (status = 200, description = "return terminal command list", body = TerminalList),

    ),
)]
#[get("/terminals")]
pub async fn list_terminal(settings: web::Data<SharedSettings>) -> Result<HttpResponse, DeskError> {
    let result = fetch_terminal_list(settings).await?;
    return Ok(HttpResponse::Ok().json(result));
}

#[utoipa::path(
    summary = "Open terminal session",
    params(StartTerminalSession),

    responses(
        (status = 200, description = "return websocket stream", body = SignalingModel),

    ),
)]
#[get("/terminal")]
pub async fn open_terminal_session(
    req: HttpRequest,
    query_list: web::Query<StartTerminalSession>,
    settings: web::Data<SharedSettings>,
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
    info!(
        "User {} is starting terminal session, command: {:?}",
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
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }).map_err(|e| actix_web::error::ErrorInternalServerError(format!("Failed to open pty: {}", e)))?;

    let mut cmd = CommandBuilder::new(execute_file_path);
    cmd.args(args_list);
    
    // Check if we are in the source directory (development mode)
    // If so, set the current directory to the parent of the server directory (project root)
    // This is useful for development
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
        // aggregate continuation frames up to 1MiB
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
