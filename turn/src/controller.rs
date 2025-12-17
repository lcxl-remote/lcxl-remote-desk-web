use actix_web::HttpResponse;
use actix_web::mime;
use actix_web::web;
use turn_server::statistics::prometheus::generate_metrics;
use turn_server::turn::Observer;
use turn_server::turn::PortAllocatePools;
use turn_server::turn::SessionAddr;

use crate::error::DeskTurnError;
use crate::model::SOFTWARE;

use crate::model::TurnApiState;
use crate::model::TurnInfo;
use crate::model::TurnInterface;
use crate::model::TurnQueryParams;
use crate::model::TurnSession;
use crate::model::TurnSessionStatistics;

// can't use get macro due to issue described here:
// https://github.com/actix/actix-web/issues/2866
//#[get("/info")]
pub async fn get_turn_info<T>(
    api_state: web::Data<TurnApiState<T>>,
) -> Result<HttpResponse, DeskTurnError>
where
    T: Clone + Observer + 'static,
{
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

pub async fn get_turn_session<T>(
    api_state: web::Data<TurnApiState<T>>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskTurnError>
where
    T: Clone + Observer + 'static,
{
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

pub async fn get_turn_session_statistics<T>(
    api_state: web::Data<TurnApiState<T>>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskTurnError>
where
    T: Clone + Observer + 'static,
{
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

pub async fn delete_turn_session<T>(
    api_state: web::Data<TurnApiState<T>>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskTurnError>
where
    T: Clone + Observer + 'static,
{
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

pub async fn get_turn_metrics() -> Result<HttpResponse, DeskTurnError> {
    let mut metrics_bytes = Vec::with_capacity(4096);
    if generate_metrics(&mut metrics_bytes).is_err() {
        return Ok(HttpResponse::ExpectationFailed().finish());
    } else {
        Ok(HttpResponse::Ok()
            .content_type(mime::TEXT_PLAIN)
            .body(metrics_bytes))
    }
}
