use actix_web::{HttpResponse, delete, get, web};
use turn_server::{
    statistics::prometheus::generate_metrics,
    turn::{PortAllocatePools, SessionAddr},
};

use crate::{
    desk_error::DeskError,
    model::turn::{
        TurnApiState, TurnInfo, TurnInterface, TurnQueryParams, TurnSession, TurnSessionStatistics,
    },
    service::turn::SOFTWARE,
};

#[utoipa::path(
    summary = "Get turn server info",
    responses(
        (status = 200, description = "Turn server info", body = TurnInfo),
    ),
)]
#[get("/info")]
pub async fn get_turn_info(api_state: web::Data<TurnApiState>) -> Result<HttpResponse, DeskError> {
    let sessions = api_state.service.get_sessions();
    let mut interfaces = Vec::new();
    for interface in api_state.config.turn.interfaces.iter() {
        let bind = interface.bind.clone();
        let external = interface.external.clone();
        let transport = interface.transport.clone();
        let turn_interface = TurnInterface {
            transport: transport.into(),
            bind: bind.to_string(),
            external: external.to_string(),
        };

        interfaces.push(turn_interface);
    }
    let turn_info = TurnInfo {
        software: SOFTWARE.to_string(),
        uptime: api_state.uptime.elapsed().as_secs(),
        interfaces,
        port_capacity: PortAllocatePools::capacity(),
        port_allocated: sessions.allocated(),
    };

    return Ok(HttpResponse::Ok().json(turn_info));
}

#[utoipa::path(
    summary = "Get turn server session",
    params(TurnQueryParams),
    responses(
        (status = 200, description = "Turn server session", body = TurnSession),
    ),
)]
#[get("/session")]
pub async fn get_turn_session(
    api_state: web::Data<TurnApiState>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskError> {
    if let Some(session) = api_state
        .service
        .get_sessions()
        .get_session(&query.into_inner().into())
        .get_ref()
    {
        let turn_session = TurnSession {
            username: session.auth.username.clone(),
            permissions: session.permissions.clone(),
            channels: session.allocate.channels.clone(),
            port: session.allocate.port,
            expires: session.expires,
        };
        return Ok(HttpResponse::Ok().json(turn_session));
    } else {
        Ok(HttpResponse::NotFound().finish())
    }
}

#[utoipa::path(
    summary = "Get turn server session statistics",
    params(TurnQueryParams),
    responses(
        (status = 200, description = "Turn server session statistics", body = TurnSessionStatistics),
        (status = 404, description = "Turn server session not found"),
    ),
)]
#[get("/session/statistics")]
pub async fn get_turn_session_statistics(
    api_state: web::Data<TurnApiState>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskError> {
    let addr: SessionAddr = query.into_inner().into();
    if let Some(counts) = api_state.statistics.get(&addr) {
        let turn_session_statistics = TurnSessionStatistics {
            received_bytes: counts.received_bytes,
            send_bytes: counts.send_bytes,
            received_pkts: counts.received_pkts,
            send_pkts: counts.send_pkts,
            error_pkts: counts.error_pkts,
        };
        return Ok(HttpResponse::Ok().json(turn_session_statistics));
    } else {
        Ok(HttpResponse::NotFound().finish())
    }
}

#[utoipa::path(
    summary = "Delete turn server session",
    params(TurnQueryParams),
    responses(
        (status = 200, description = "Deleted turn server session"),
        (status = 417, description = "Expectation failed"),
    ),
)]
#[delete("/session")]
pub async fn delete_turn_session(
    api_state: web::Data<TurnApiState>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskError> {
    if api_state
        .service
        .get_sessions()
        .refresh(&query.into_inner().into(), 0)
    {
        return Ok(HttpResponse::Ok().finish());
    } else {
        Ok(HttpResponse::ExpectationFailed().finish())
    }
}

#[utoipa::path(
    summary = "Turn server metrics",
    responses(
        (status = 200, description = "turn server metrics", body = String),
        (status = 417, description = "Expectation failed"),
    ),
)]
#[get("/metrics")]
pub async fn get_turn_metrics() -> Result<HttpResponse, DeskError> {
    let mut metrics_bytes = Vec::with_capacity(4096);
    if generate_metrics(&mut metrics_bytes).is_err() {
        return Ok(HttpResponse::ExpectationFailed().finish());
    } else {
        Ok(HttpResponse::Ok()
            .content_type(mime::TEXT_PLAIN)
            .body(metrics_bytes))
    }
}
