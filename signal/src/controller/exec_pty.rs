use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use actix_ws::AggregatedMessage;
use desk_agent_protocol::exec_pty::{PtyCarrierPrepare, PtyCarrierServerMessage, PtyCloseReason};
use desk_agent_protocol::exec_pty_wire::MAX_PTY_WIRE_FRAME_BYTES;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::controller::signaling::resolve_browser_identity;
use crate::exec_pty_carrier::{BROWSER_OUTPUT_QUEUE_CAP, global_exec_pty_carriers};
use crate::model::SharedConnectionMap;

pub const TAG: &str = "Exec PTY";

#[utoipa::path(
    tag = TAG,
    summary = "Open a one-shot AI execution PTY carrier",
    responses((status = 200, description = "WebSocket stream")),
)]
#[get("/api/desk/exec-pty")]
pub async fn open_exec_pty_carrier(
    req: HttpRequest,
    connection_map: web::Data<SharedConnectionMap>,
    session: Session,
    stream: web::Payload,
) -> Result<HttpResponse, actix_web::Error> {
    let Some((_user, code_session)) = resolve_browser_identity(&session)? else {
        return Err(actix_web::error::ErrorUnauthorized("User not logged in"));
    };
    if code_session.is_some() {
        return Err(actix_web::error::ErrorForbidden(
            "Interactive execution requires the device owner",
        ));
    }
    if !same_origin(&req) {
        return Err(actix_web::error::ErrorForbidden(
            "WebSocket Origin does not match this server",
        ));
    }

    let (response, mut browser_session, browser_stream) = actix_ws::handle(&req, stream)?;
    let mut browser_stream = browser_stream
        .max_frame_size(MAX_PTY_WIRE_FRAME_BYTES)
        .aggregate_continuations()
        .max_continuation_size(MAX_PTY_WIRE_FRAME_BYTES);
    let connections = connection_map.into_inner();
    rt::spawn(async move {
        let first =
            tokio::time::timeout(std::time::Duration::from_secs(10), browser_stream.next()).await;
        let prepare = match first {
            Ok(Some(Ok(AggregatedMessage::Text(text)))) => {
                serde_json::from_str::<PtyCarrierPrepare>(&text).ok()
            }
            _ => None,
        };
        let Some(prepare) = prepare else {
            send_error(
                &mut browser_session,
                "invalid_prepare",
                "A bounded prepare message is required",
            )
            .await;
            let _ = browser_session.close(None).await;
            return;
        };
        let (output_tx, mut output_rx) = mpsc::channel(BROWSER_OUTPUT_QUEUE_CAP);
        let registry = global_exec_pty_carriers();
        let carrier_id = match registry
            .prepare(&prepare, connections.as_ref(), output_tx)
            .await
        {
            Ok(carrier_id) => carrier_id,
            Err(error) => {
                send_error(&mut browser_session, "prepare_rejected", &error.to_string()).await;
                let _ = browser_session.close(None).await;
                return;
            }
        };
        let ready = PtyCarrierServerMessage::Ready {
            carrier_id: carrier_id.clone(),
            exec_request_id: prepare.exec_request_id,
        };
        let ready = serde_json::to_string(&ready).unwrap_or_else(|_| {
            "{\"type\":\"error\",\"code\":\"encode_failed\",\"message\":\"Carrier setup failed\"}".into()
        });
        if browser_session.text(ready).await.is_err() {
            registry
                .disconnect(
                    &carrier_id,
                    connections.as_ref(),
                    PtyCloseReason::CarrierDisconnected,
                )
                .await;
            return;
        }

        loop {
            tokio::select! {
                incoming = browser_stream.next() => match incoming {
                    Some(Ok(AggregatedMessage::Binary(bytes))) => {
                        if registry
                            .forward_browser_binary(&carrier_id, bytes, connections.as_ref())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(AggregatedMessage::Ping(bytes))) => {
                        if browser_session.pong(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(AggregatedMessage::Pong(_))) => {}
                    Some(Ok(AggregatedMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(AggregatedMessage::Text(_))) => break,
                },
                output = output_rx.recv() => match output {
                    Some(bytes) => {
                        if browser_session.binary(bytes).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        registry
            .disconnect(
                &carrier_id,
                connections.as_ref(),
                PtyCloseReason::CarrierDisconnected,
            )
            .await;
        let _ = browser_session.close(None).await;
    });
    Ok(response)
}

async fn send_error(session: &mut actix_ws::Session, code: &str, message: &str) {
    let payload = PtyCarrierServerMessage::Error {
        code: code.to_string(),
        message: message.to_string(),
    };
    if let Ok(text) = serde_json::to_string(&payload) {
        let _ = session.text(text).await;
    }
}

fn same_origin(req: &HttpRequest) -> bool {
    let Some(origin) = req
        .headers()
        .get(actix_web::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let info = req.connection_info();
    let Ok(expected) = url::Url::parse(&format!("{}://{}", info.scheme(), info.host())) else {
        return false;
    };
    let Ok(actual) = url::Url::parse(origin) else {
        return false;
    };
    actual.origin() == expected.origin()
}
