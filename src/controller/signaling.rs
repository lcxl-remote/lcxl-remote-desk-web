use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use log::{error, info};

use crate::{
    model::{
        record_audio::SelectedAudioDevice,
        settings::SharedSettings,
        signaling::{InitSignalingData, SignalingErrorData, SignalingModel},
    },
    service::{signaling::handle_signaling, user::SessionExt},
};

#[utoipa::path(
    summary = "Open Signaling Handle, return websocket stream. NOTE: The OpenAPI generated typescript service is not right.",
    responses(
        (status = 200, description = "return websocket stream", body = SignalingModel),
        (status = 500, description = "return websocket stream", body = SignalingErrorData),
        (status = 201, description = "init signaling data", body = InitSignalingData),
        (status = 202, description = "init signaling data", body = SelectedAudioDevice),
    ),
)]
#[get("/signaling")]
pub async fn open_signaling_handle(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
    session: Session,
    stream: web::Payload,
) -> Result<HttpResponse, actix_web::Error> {
    let user = session.get_current_user()?;

    if user.is_none() {
        return Err(actix_web::error::ErrorUnauthorized("User not logged in"));
    }
    let user = user.unwrap();
    info!("User {} is signaling", user.name);

    let (res, session, stream) = actix_ws::handle(&req, stream)?;

    let stream = stream
        .aggregate_continuations()
        // aggregate continuation frames up to 1MiB
        .max_continuation_size(2_usize.pow(20));

    // start task but don't wait for it
    rt::spawn(async move {
        // receive messages from websocket
        let result = handle_signaling(settings, stream, session, user).await;
        if let Err(e) = result {
            error!("Error handling signaling: {}", e);
        } else {
            info!("Signaling handled successfully");
        }
    });

    // respond immediately with response connected to WS session
    Ok(res)
}
