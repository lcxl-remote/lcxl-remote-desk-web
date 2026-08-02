use std::net::SocketAddr;

use actix_web::HttpResponse;
use actix_web::mime;
use actix_web::web;
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;

use crate::error::DeskTurnError;

use crate::model::TurnQueryParams;
use crate::model::TurnSessionStatistics;
use crate::runtime::TurnRuntimeView;

pub async fn get_turn_info(
    view: web::Data<TurnRuntimeView>,
) -> Result<HttpResponse, DeskTurnError> {
    // Always a success: "this host is not relaying" is the answer to the
    // question, not a failure to answer it.
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(view.info().await)))
}

pub async fn get_turn_session_statistics(
    view: web::Data<TurnRuntimeView>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskTurnError> {
    let Some(runtime) = view.runtime() else {
        return Ok(HttpResponse::Ok().json(
            RestResponse::<TurnSessionStatistics>::failed_with_data(
                DeskErrorCode::FEATURE_UNAVAILABLE,
                Some("No TURN runtime is serving on this host".to_string()),
                None,
            ),
        ));
    };

    let address: SocketAddr = query
        .into_inner()
        .address
        .parse()
        .map_err(|_| DeskTurnError::IllegalTransport("Invalid address".to_string()))?;

    if let Ok(stats) = runtime.statistics.read()
        && let Some(counts) = stats.sessions.get(&address)
    {
        return Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(counts.clone())));
    }

    // A runtime is up but has never seen this address: absent statistics, not an
    // absent service.
    Ok(
        HttpResponse::Ok().json(RestResponse::<TurnSessionStatistics>::failed_with_data(
            DeskErrorCode::SESSION_NOT_FOUND,
            Some("No TURN session statistics for that address".to_string()),
            None,
        )),
    )
}

pub async fn get_turn_metrics(
    view: web::Data<TurnRuntimeView>,
) -> Result<HttpResponse, DeskTurnError> {
    // Metrics stay `text/plain` in every case, including this one: a scraper
    // parses the body as text and would choke on a JSON envelope. The status
    // code is what tells it the target is not serving.
    let Some(runtime) = view.runtime() else {
        return Ok(HttpResponse::ServiceUnavailable()
            .content_type(mime::TEXT_PLAIN)
            .body("# no TURN runtime is serving on this host\n"));
    };

    let mut metrics = String::new();

    if let Ok(stats) = runtime.statistics.read() {
        for (class, c) in [
            ("relay", &stats.global.relay),
            ("control", &stats.global.control),
        ] {
            metrics.push_str(&format!(
                "turn_server_received_bytes_total{{class=\"{class}\"}} {}\n",
                c.received_bytes
            ));
            metrics.push_str(&format!(
                "turn_server_sent_bytes_total{{class=\"{class}\"}} {}\n",
                c.send_bytes
            ));
            metrics.push_str(&format!(
                "turn_server_received_pkts_total{{class=\"{class}\"}} {}\n",
                c.received_pkts
            ));
            metrics.push_str(&format!(
                "turn_server_sent_pkts_total{{class=\"{class}\"}} {}\n",
                c.send_pkts
            ));
        }
    }

    Ok(HttpResponse::Ok()
        .content_type(mime::TEXT_PLAIN)
        .body(metrics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TurnRuntimeState, TurnTrafficClass};
    use crate::runtime::TurnIntent;
    use crate::supervisor::DesiredState;
    use crate::test_support::{loopback_params, loopback_supervisor, wait_for_runtime};
    use actix_web::{App, test};
    use serde_json::Value;

    /// The `/turn` endpoints under test, wired the way the servers wire them.
    fn app_config(
        view: TurnRuntimeView,
    ) -> impl FnOnce(&mut actix_web::web::ServiceConfig) + 'static {
        move |cfg: &mut actix_web::web::ServiceConfig| {
            cfg.app_data(web::Data::new(view))
                .route("/info", web::get().to(get_turn_info))
                .route(
                    "/session/statistics",
                    web::get().to(get_turn_session_statistics),
                )
                .route("/metrics", web::get().to(get_turn_metrics));
        }
    }

    const QUERY: &str = "?address=127.0.0.1:5000&interface=udp";

    /// Every supported endpoint in one case, because the failure mode this
    /// guards against is a sweeping "TURN is unavailable" response that erases
    /// the distinction between runtime state, statistics and scraper output.
    #[actix_web::test]
    async fn a_host_with_no_runtime_answers_each_endpoint_in_its_own_way() {
        let app =
            test::init_service(App::new().configure(app_config(TurnRuntimeView::unsupported())))
                .await;

        // Depends on the runtime: says there is none, and why.
        let res =
            test::call_service(&app, test::TestRequest::get().uri("/info").to_request()).await;
        assert_eq!(res.status(), 200);
        let body: Value = test::read_body_json(res).await;
        assert_eq!(
            body["success"], true,
            "not relaying is an answer, not a fault"
        );
        assert_eq!(body["data"]["state"], "unsupported");

        // Depends on the runtime: a business code, not a transport error.
        let res = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/session/statistics{QUERY}"))
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), 200);
        let body: Value = test::read_body_json(res).await;
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], DeskErrorCode::FEATURE_UNAVAILABLE.code());

        // Depends on the runtime, but is scraped as text: the status code
        // carries the news, the body stays parseable by a metrics collector.
        let res =
            test::call_service(&app, test::TestRequest::get().uri("/metrics").to_request()).await;
        assert_eq!(res.status(), 503);
        assert_eq!(res.headers().get("content-type").unwrap(), "text/plain");
    }

    /// With a relay up, the supported endpoints report it.
    #[actix_web::test]
    async fn a_running_host_reports_its_runtime() {
        let (supervisor, view, _intent_tx) = loopback_supervisor(
            TurnIntent::Run,
            DesiredState {
                revision: 1,
                params: Some(loopback_params("s")),
            },
        );
        let runtime = wait_for_runtime(&view, true).await.expect("running");
        let app = test::init_service(App::new().configure(app_config(view.clone()))).await;

        let res =
            test::call_service(&app, test::TestRequest::get().uri("/info").to_request()).await;
        let body: Value = test::read_body_json(res).await;
        assert_eq!(body["data"]["state"], "running");
        assert!(body["data"]["uptime_secs"].is_number());
        assert_eq!(body["data"]["interfaces"].as_array().unwrap().len(), 1);

        // An address the runtime has never seen: absent statistics, not an
        // absent service — the two must not collapse into one answer, and
        // neither is a transport failure.
        let res = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/session/statistics{QUERY}"))
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), 200);
        let body: Value = test::read_body_json(res).await;
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], DeskErrorCode::SESSION_NOT_FOUND.code());

        // Once it has seen it, the counters come back in the success envelope.
        // Written the way the socket wrapper writes them: the per-address table
        // and the process-wide totals the metrics endpoint reports.
        {
            let mut stats = runtime.statistics.write().unwrap();
            let addr = "127.0.0.1:5000".parse().unwrap();
            stats.global.add_recv(42, TurnTrafficClass::Relay);
            stats
                .sessions
                .entry(addr)
                .or_default()
                .add_recv(42, TurnTrafficClass::Relay);
        }
        let res = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/session/statistics{QUERY}"))
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), 200);
        let body: Value = test::read_body_json(res).await;
        assert_eq!(body["success"], true);
        assert_eq!(body["data"]["relay"]["received_bytes"], 42);

        let res =
            test::call_service(&app, test::TestRequest::get().uri("/metrics").to_request()).await;
        assert_eq!(res.status(), 200);
        let body = test::read_body(res).await;
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("turn_server_received_bytes_total{class=\"relay\"} 42"));

        supervisor.shutdown().await;
        // The relay is gone; the same endpoint now reports the reason rather
        // than the runtime it was serving a moment ago.
        let res =
            test::call_service(&app, test::TestRequest::get().uri("/info").to_request()).await;
        let body: Value = test::read_body_json(res).await;
        assert_ne!(body["data"]["state"], "running");
        // Nothing went wrong here — the runtime was taken away. Reporting a
        // failure would put a state on the card that the document's own
        // `last_error` cannot explain.
        let info = view.info().await;
        assert_eq!(info.state, TurnRuntimeState::Starting);
        assert!(
            info.last_error.is_none(),
            "a failure is only reported when there is one to report"
        );
    }
}
