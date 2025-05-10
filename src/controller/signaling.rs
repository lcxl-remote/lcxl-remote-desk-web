use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use log::info;

use crate::service::{signaling::handle_signaling, user::SessionExt};

#[utoipa::path(
    summary = "Signaling Handler",
    responses(
        (status = 200, description = "return websocket stream")
    ),
)]
#[get("/signaling")]
pub async fn signaling_handler(
    req: HttpRequest,
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
        handle_signaling(stream, session).await;
    });

    // respond immediately with response connected to WS session
    Ok(res)
}
