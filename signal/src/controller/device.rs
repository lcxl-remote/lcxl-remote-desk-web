use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::{signal::RemoteDeskTypeEnum, version::VersionInfo};
use desk_turn::model::TurnApiState;
use log::{error, info, warn};

use crate::{model::SharedConnectionMap, service::handle_signaling};

#[utoipa::path(
    summary = "Open Device Signaling Handle for Server, return websocket stream.",
    params(VersionInfo),
    responses(
        (status = 200, description = "return websocket stream"),
    ),
)]
#[get("/api/desk/signaling_device")]
pub async fn open_device_signaling_handle(
    req: HttpRequest,
    query: web::Query<VersionInfo>,
    connection_map: web::Data<SharedConnectionMap>,
    _session: Session,
    stream: web::Payload,
    turn_api_state: web::Data<TurnApiState>,
) -> Result<HttpResponse, actix_web::Error> {
    let version_info = query.0.clone();

    // Only allow Server type to connect to this endpoint
    if version_info.remote_desk_type != RemoteDeskTypeEnum::Server {
        warn!(
            "Non-Server RemoteDeskTypeEnum trying to connect signaling_device endpoint: {:?}",
            version_info.remote_desk_type
        );
        return Err(actix_web::error::ErrorForbidden("Only Server is allowed"));
    }

    // Must provide client_id
    if version_info.client_id.is_none() {
        warn!("Server trying to connect signaling_device endpoint without client_id");
        return Err(actix_web::error::ErrorBadRequest("client_id is required"));
    }

    info!(
        "Device Server (client_id: {:?}) is signaling",
        version_info.client_id
    );

    // Create a virtual "device_user" to pass the handle_signaling requirement
    let mut virtual_user = CurrentUser::new_admin("device_server");
    virtual_user.access = Some("device_user".to_string());

    let (res, actix_session, stream) = actix_ws::handle(&req, stream)?;

    let stream = stream
        .aggregate_continuations()
        // aggregate continuation frames up to 1MiB
        .max_continuation_size(2_usize.pow(20));

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
            actix_session,
            virtual_user,
            ip,
            turn_api_state.into_inner().as_ref().settings.clone(),
        )
        .await;
        if let Err(e) = result {
            error!("Error handling device signaling: {}", e);
        } else {
            info!("Device signaling handle is finished");
        }
    });

    // respond immediately with response connected to WS session
    Ok(res)
}
