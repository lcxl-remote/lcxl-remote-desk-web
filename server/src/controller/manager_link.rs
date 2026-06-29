//! Host-side status of the link to the central manager.
//!
//! When the manager fatally rejects this host's registration (device quota
//! reached, or a missing device identity), the signaling proxy stops auto-
//! reconnecting and records the reason here. The host UI reads the status to show
//! "device limit reached; remove an unused device" and offers a manual retry the
//! user invokes after freeing a slot from a control end.

use actix_web::{Error as AWError, HttpResponse, get, post, web};
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::daemon::manager_link_state::ManagerLinkState;

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct ManagerLinkStatus {
    /// True when registration is fatally blocked and auto-reconnect is paused.
    pub blocked: bool,
    /// `DeskErrorCode` of the rejection (`46` quota exceeded, `47` missing client
    /// id), when blocked.
    pub error_code: Option<i32>,
    /// Human-readable reason, when blocked.
    pub message: Option<String>,
}

#[utoipa::path(
    tag = "ManagerLink",
    summary = "Query host→manager link status",
    responses(
        (status = 200, description = "Manager link status", body = RestResponse<ManagerLinkStatus>),
    ),
)]
#[get("/manager-link/status")]
pub async fn query_manager_link_status(
    state: web::Data<Arc<ManagerLinkState>>,
) -> Result<HttpResponse, AWError> {
    let status = match state.snapshot().await {
        Some(f) => ManagerLinkStatus {
            blocked: true,
            error_code: Some(f.error_code),
            message: Some(f.message),
        },
        None => ManagerLinkStatus {
            blocked: false,
            error_code: None,
            message: None,
        },
    };
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(status)))
}

#[utoipa::path(
    tag = "ManagerLink",
    summary = "Manually retry the host→manager registration",
    responses(
        (status = 200, description = "Retry requested", body = RestResponse<bool>),
    ),
)]
#[post("/manager-link/retry")]
pub async fn retry_manager_link(
    state: web::Data<Arc<ManagerLinkState>>,
) -> Result<HttpResponse, AWError> {
    // Wake a proxy loop parked after a fatal rejection; a no-op if not blocked.
    state.request_retry();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(true)))
}
