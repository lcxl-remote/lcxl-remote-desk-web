use std::net::SocketAddr;

use actix_web::HttpResponse;
use actix_web::mime;
use actix_web::web;

use crate::error::DeskTurnError;
use crate::model::SOFTWARE;

use crate::model::TurnApiState;
use crate::model::TurnInfo;
use crate::model::TurnQueryParams;

pub async fn get_turn_info(
    api_state: web::Data<TurnApiState>,
) -> Result<HttpResponse, DeskTurnError> {
    let turn_info = TurnInfo {
        software: SOFTWARE.to_string(),
        uptime: api_state.uptime.elapsed().as_secs(),
        interfaces: api_state.settings.interfaces.clone(),
        port_capacity: 65535, // Mock value
        port_allocated: 0,    // Mock value
    };

    Ok(HttpResponse::Ok().json(turn_info))
}

pub async fn get_turn_session(
    _api_state: web::Data<TurnApiState>,
    _query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskTurnError> {
    // Session iteration is not easily supported in webrtc::turn, mock a response or return not found.
    Ok(HttpResponse::NotFound().finish())
}

pub async fn get_turn_session_statistics(
    api_state: web::Data<TurnApiState>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskTurnError> {
    let address: SocketAddr = query
        .into_inner()
        .address
        .parse()
        .map_err(|_| DeskTurnError::IllegalTransport("Invalid address".to_string()))?;

    if let Ok(stats) = api_state.statistics.read()
        && let Some(counts) = stats.sessions.get(&address)
    {
        return Ok(HttpResponse::Ok().json(counts));
    }

    Ok(HttpResponse::NotFound().finish())
}

pub async fn delete_turn_session(
    _api_state: web::Data<TurnApiState>,
    _query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskTurnError> {
    // webrtc::turn doesn't have an API to force-close a specific session easily.
    Ok(HttpResponse::ExpectationFailed().finish())
}

pub async fn get_turn_metrics(
    api_state: web::Data<TurnApiState>,
) -> Result<HttpResponse, DeskTurnError> {
    let mut metrics = String::new();

    if let Ok(stats) = api_state.statistics.read() {
        metrics.push_str(&format!(
            "turn_server_received_bytes_total {}\n",
            stats.global.received_bytes
        ));
        metrics.push_str(&format!(
            "turn_server_sent_bytes_total {}\n",
            stats.global.send_bytes
        ));
        metrics.push_str(&format!(
            "turn_server_received_pkts_total {}\n",
            stats.global.received_pkts
        ));
        metrics.push_str(&format!(
            "turn_server_sent_pkts_total {}\n",
            stats.global.send_pkts
        ));
    }

    Ok(HttpResponse::Ok()
        .content_type(mime::TEXT_PLAIN)
        .body(metrics))
}
