use actix_web::{HttpResponse, get, web};
use desk_utils::error::DeskErrorCode;

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::signal::{ForwardSignalingSender, SignalingType};
use crate::model::terminal::{ListTerminalPath, TerminalList};

#[utoipa::path(
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
    let response = {
        let connection_map = connection_map.read().await;
        if let Some(connection) = connection_map.get(&path.connection_id) {
            connection
                .request_peer_with_callback::<()>(SignalingType::ListTerminal, None, None)
                .await?
        } else {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!(
                    "Connection {} is not found to list terminal",
                    path.connection_id
                ),
            );
        }
    };

    if let Some(ref response_state) = response.response_state {
        if response_state.error_code != 0 {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }
    }

    let terminal_list_response: TerminalList = response.get_data()?;
    Ok(HttpResponse::Ok().json(terminal_list_response))
}

// NOTE: open_terminal_session is NOT extracted here because it requires
// creating a SignalingHandler which needs a concrete user type (generic U: SignalingUser).
// Each consumer (signal / manager) must implement their own open_terminal_session
// using their specific user type and version info. The common SignalingHandler in
// service.rs is already shared via signal-facade.
