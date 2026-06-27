use actix_web::{HttpResponse, get, web};

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::signal::SignalingType;
use crate::model::terminal::{ListTerminalPath, TerminalList};
use crate::service::request_on_local_connection;

pub const TAG: &str = "Terminal";

/// Run the terminal-list request against a connection held in the local map and
/// build the HTTP response. Addressing is decoupled from the path so cross-instance
/// callers (the manager) reuse the same core (rule 22 dual-target parity).
pub async fn list_terminal_core(
    connection_map: &SharedConnectionMap,
    connection_id: &str,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let not_found = format!("Connection {connection_id} is not found to list terminal");
    let response = request_on_local_connection::<()>(
        connection_map,
        connection_id,
        SignalingType::ListTerminal,
        None,
        &not_found,
    )
    .await?;

    let terminal_list_response: TerminalList = response.get_data()?;
    Ok(HttpResponse::Ok().json(terminal_list_response))
}

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
    list_terminal_core(&connection_map, &path.connection_id).await
}

// NOTE: open_terminal_session is NOT extracted here because it requires
// creating a SignalingHandler which needs a concrete user type (generic U: SignalingUser).
// Each consumer (signal / manager) must implement their own open_terminal_session
// using their specific user type and version info. The common SignalingHandler in
// service.rs is already shared via signal-facade.
