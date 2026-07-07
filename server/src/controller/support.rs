//! Host-side control of the on-demand temporary-support session.
//!
//! A local user (at the machine) asks for a support code; the host opens a
//! dedicated `Support` upstream to the manager, which mints a short-lived code and
//! pushes it back. The host displays the code so the user can read it out to a
//! supporter. These endpoints are the local UI's control surface: start a session,
//! stop it ("end support"), and read the current code + expiry to render the code
//! card and countdown. The code itself arrives asynchronously over the support
//! upstream, so `start` only triggers; the UI then polls `status` (or listens for
//! the pushed event) for the issued code.

use actix_web::{Error as AWError, HttpResponse, get, post, web};
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::daemon::support_link_state::SupportLinkState;

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct SupportSessionStatus {
    /// True while a support session is live (upstream requested / open and not yet
    /// stopped or expired).
    pub active: bool,
    /// The manager-issued code, once it has arrived on the support upstream.
    pub code: Option<String>,
    /// Unix seconds at which the code / session expires, when a code is present.
    pub expires_at: Option<i64>,
}

impl SupportSessionStatus {
    async fn read(state: &SupportLinkState) -> Self {
        let snapshot = state.snapshot().await;
        Self {
            active: state.is_active(),
            code: snapshot.as_ref().map(|s| s.code.clone()),
            expires_at: snapshot.as_ref().map(|s| s.expires_at),
        }
    }
}

#[utoipa::path(
    tag = "Support",
    summary = "Start an on-demand temporary-support session",
    responses(
        (status = 200, description = "Current support session status", body = RestResponse<SupportSessionStatus>),
    ),
)]
#[post("/support/start")]
pub async fn start_support(
    state: web::Data<Arc<SupportLinkState>>,
) -> Result<HttpResponse, AWError> {
    // Idempotent: a start while a session is already active surfaces the existing
    // session rather than opening a second upstream.
    let _started = state.request_start();
    let status = SupportSessionStatus::read(&state).await;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(status)))
}

#[utoipa::path(
    tag = "Support",
    summary = "End the current temporary-support session",
    responses(
        (status = 200, description = "Stop requested", body = RestResponse<bool>),
    ),
)]
#[post("/support/stop")]
pub async fn stop_support(
    state: web::Data<Arc<SupportLinkState>>,
) -> Result<HttpResponse, AWError> {
    // Flips the session inactive; the proxy's support loop tears down the upstream
    // and any restricted PCs. A no-op if no session is active.
    state.request_stop();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(true)))
}

#[utoipa::path(
    tag = "Support",
    summary = "Query the current temporary-support session status",
    responses(
        (status = 200, description = "Current support session status", body = RestResponse<SupportSessionStatus>),
    ),
)]
#[get("/support/status")]
pub async fn support_status(
    state: web::Data<Arc<SupportLinkState>>,
) -> Result<HttpResponse, AWError> {
    let status = SupportSessionStatus::read(&state).await;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(status)))
}
