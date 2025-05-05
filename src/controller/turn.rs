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
        turn::{ApiState, TurnInfo, TurnQueryParams, TurnSession, TurnSessionStatistics},
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

#[get("/turn/info")]
pub async fn get_turn_info(api_state: web::Data<ApiState>) -> Result<HttpResponse, DeskError> {
    let sessions = api_state.service.get_sessions();
    let turn_info = TurnInfo {
        software: SOFTWARE.to_string(),
        uptime: api_state.uptime.elapsed().as_secs(),
        interfaces: api_state.config.turn.interfaces.clone(),
        port_capacity: PortAllocatePools::capacity(),
        port_allocated: sessions.allocated(),
    };

    return Ok(HttpResponse::Ok().json(turn_info));
}

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
