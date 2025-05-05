use std::{sync::Arc, time::Instant};

use actix_web::{HttpResponse, delete, get, web};
use log::info;
use turn_server::{
    observer::Observer,
    statistics::{Statistics, prometheus::generate_metrics},
    turn::{PortAllocatePools, Service, SessionAddr},
};

use crate::{
    desk_error::DeskError,
    model::{
        settings::Settings,
        turn::{ApiState, TurnInfo, TurnInterface, TurnQueryParams, TurnSession, TurnSessionStatistics},
    },
};

#[rustfmt::skip]
static SOFTWARE: &str = concat!(
    "lcxl-web-remote-desk-turn-rs.",
    env!("CARGO_PKG_VERSION")
);

/// Starts the TURN server with the provided settings.
pub async fn startup_turn_server(settings: &Settings) -> Result<ApiState, DeskError> {
    let config = Arc::new(settings.to_turn_server_config()?);

    info!("Starting turn server with config {:?}", config);

    let statistics = Statistics::default();
    let service = Service::new(
        SOFTWARE.to_string(),
        config.turn.realm.clone(),
        config.turn.get_externals(),
        Observer::new(config.clone(), statistics.clone()).await?,
    );

    turn_server::server::start(&config, &statistics, &service).await?;
    let api_state = ApiState {
        config: config.clone(),
        uptime: Instant::now(),
        service,
        statistics,
    };

    info!("Turn server starteds successfully.");
    Ok(api_state)
}

#[utoipa::path(
    summary = "Get turn server info",
    responses(
        (status = 200, description = "Turn server info", body = TurnInfo),
    ),
)]
#[get("/turn/info")]
pub async fn get_turn_info(api_state: web::Data<ApiState>) -> Result<HttpResponse, DeskError> {
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
#[get("/turn/session")]
pub async fn get_turn_session(
    api_state: web::Data<ApiState>,
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
#[get("/turn/session/statistics")]
pub async fn get_turn_session_statistics(
    api_state: web::Data<ApiState>,
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
#[delete("/turn/session")]
pub async fn delete_turn_session(
    api_state: web::Data<ApiState>,
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
#[get("/turn/metrics")]
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
