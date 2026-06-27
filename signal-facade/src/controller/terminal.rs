use actix_web::{HttpResponse, get, web};

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::signal::SignalingType;
use crate::model::terminal::{ListTerminalPath, TerminalList};
use crate::service::request_on_local_connection;

pub const TAG: &str = "Terminal";

#[utoipa::path(
    tag = TAG,
    summary = "List terminal commands on remote desk",
    params(ListTerminalPath),
    responses(
        (status = 200, description = "Return terminal command list", body = TerminalList),
    ),
)]
#[get("/terminals/{connection_id}")]
pub async fn list_terminal(
    connection_map: web::Data<SharedConnectionMap>,
    path: web::Path<ListTerminalPath>,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let not_found = format!(
        "Connection {} is not found to list terminal",
        path.connection_id
    );
    let response = request_on_local_connection::<()>(
        &connection_map,
        &path.connection_id,
        SignalingType::ListTerminal,
        None,
        &not_found,
    )
    .await?;

    let terminal_list_response: TerminalList = response.get_data()?;
    Ok(HttpResponse::Ok().json(terminal_list_response))
}

// NOTE: open_terminal_session is NOT extracted here because it requires
// creating a SignalingHandler which needs a concrete user type (generic U: SignalingUser).
// Each consumer (signal / manager) must implement their own open_terminal_session
// using their specific user type and version info. The common SignalingHandler in
// service.rs is already shared via signal-facade.
