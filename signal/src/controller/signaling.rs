use std::sync::Arc;

use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_signal_facade::{
    model::{signal::RemoteDeskTypeEnum, version::VersionInfo},
    service::NodeTokenValidator,
};
use desk_turn::model::TurnApiState;
use log::{error, info};

use crate::{model::SharedConnectionMap, service::handle_signaling};

pub const TAG: &str = "Signaling";

/// Result of validating the (optional) node token presented on the signaling
/// query string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenOutcome {
    /// No token was presented (empty or missing).
    Absent,
    /// A non-empty token was presented and validated.
    Valid,
    /// A non-empty token was presented but failed validation.
    Invalid,
}

/// Server-adjudicated authentication outcome for a signaling connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleDecision {
    /// Reject the connection with 401.
    Reject,
    /// Authenticated as a node via a valid token; act as the reported role
    /// (a desk-server host reports `Server`).
    Node(RemoteDeskTypeEnum),
    /// No token presented; authenticate via the session cookie and act as a
    /// `Browser` regardless of any self-reported role.
    Cookie,
}

/// Pure role adjudication: the security-critical core of signaling auth.
///
/// - A valid token authenticates a node acting as its reported role.
/// - A non-empty but invalid token is rejected (NEVER falls back to cookie auth,
///   so a stale token surfaces as 401 instead of a silent Browser downgrade).
/// - No token defers to cookie session auth, which is always `Browser`.
fn adjudicate_role(token: TokenOutcome, reported_type: RemoteDeskTypeEnum) -> RoleDecision {
    match token {
        TokenOutcome::Valid => RoleDecision::Node(reported_type),
        TokenOutcome::Invalid => RoleDecision::Reject,
        TokenOutcome::Absent => RoleDecision::Cookie,
    }
}

#[utoipa::path(
    tag = TAG,
    summary = "Open Signaling Handle, return websocket stream. NOTE: The OpenAPI generated typescript service is not right.",
    params(VersionInfo),
    responses(
        (status = 200, description = "return websocket stream"),
    ),
)]
#[get("/api/desk/signaling")]
#[allow(clippy::too_many_arguments)]
pub async fn open_signaling_handle(
    req: HttpRequest,
    query: Option<web::Query<VersionInfo>>,
    connection_map: web::Data<SharedConnectionMap>,
    session: Session,
    stream: web::Payload,
    turn_api_state: Option<web::Data<TurnApiState>>,
    validator_opt: Option<web::Data<Arc<dyn NodeTokenValidator>>>,
    conn_device_map: Option<web::Data<crate::turn_usage::ConnectionDeviceMap>>,
) -> Result<HttpResponse, actix_web::Error> {
    info!("Incoming signaling request: {} {}", req.method(), req.uri());

    let version_info_opt = query.map(|q| q.into_inner());

    // Adjudicate the connection's role server-side. A self-reported
    // `remote_desk_type` from the client is never trusted: only a node that
    // passes `NodeTokenValidator` is allowed to act as `Server`; every
    // cookie/session connection is forced to `Browser`. This mirrors the
    // manager's contract so both ends behave identically.
    let presented_token = version_info_opt
        .as_ref()
        .and_then(|vi| vi.token.as_deref())
        .filter(|t| !t.is_empty());

    let token_outcome = match presented_token {
        None => TokenOutcome::Absent,
        Some(token) => {
            // A non-empty token MUST validate. On failure we deliberately do NOT
            // fall back to cookie auth, so a stale/wrong token surfaces as 401
            // and the client can clear its cache and re-issue, instead of being
            // silently downgraded to a Browser while still holding a cookie.
            let valid = match &validator_opt {
                Some(validator) => validator.validate_node_token(token).await,
                None => false,
            };
            if valid {
                TokenOutcome::Valid
            } else {
                TokenOutcome::Invalid
            }
        }
    };

    let reported_type = version_info_opt
        .as_ref()
        .map(|vi| vi.remote_desk_type)
        .unwrap_or(RemoteDeskTypeEnum::Browser);

    let (user, adjudicated_type) = match adjudicate_role(token_outcome, reported_type) {
        RoleDecision::Reject => {
            log::warn!("Invalid node token provided");
            return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
        }
        RoleDecision::Node(role) => {
            info!("Node token validated successfully");
            (CurrentUser::new_admin("server_node"), role)
        }
        RoleDecision::Cookie => {
            // No token: authenticate via the session cookie. Such a connection is
            // always a Browser, regardless of any self-reported role.
            let Some(u) = session.get_current_user::<CurrentUser>()? else {
                log::warn!("User not logged in and no valid node token provided");
                return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
            };
            (u, RemoteDeskTypeEnum::Browser)
        }
    };

    info!("User {} is signaling", user.name);

    let (res, session, stream) = actix_ws::handle(&req, stream)?;

    let stream = stream
        .aggregate_continuations()
        // aggregate continuation frames up to 1MiB
        .max_continuation_size(2_usize.pow(20));

    let mut version_info = version_info_opt.unwrap_or_else(|| VersionInfo {
        api_version: desk_server_version::SERVER_API_VERSION,
        build_number: crate::version::SIGNAL_BUILD_NUMBER,
        commit_hash: crate::version::SIGNAL_COMMIT_HASH.to_string(),
        remote_desk_type: RemoteDeskTypeEnum::Browser,
        operation_system: desk_signal_facade::model::os::OperationSystemEnum::default(),
        display_name: Some(user.name.clone()),
        client_id: None,
        token: None,
    });
    // Overwrite any self-reported role with the server-adjudicated one so the
    // downstream handler (device registration, presence) trusts only this.
    version_info.remote_desk_type = adjudicated_type;

    let ip = req
        .connection_info()
        .realip_remote_addr()
        .map(|s| s.to_string());
    let turn_settings = turn_api_state
        .as_ref()
        .map(|state| state.as_ref().settings.clone());

    // start task but don't wait for it
    rt::spawn(async move {
        // receive messages from websocket
        let result = handle_signaling(
            version_info,
            stream,
            connection_map,
            session,
            user,
            ip,
            turn_settings,
            conn_device_map.map(|d| d.into_inner()),
        )
        .await;
        if let Err(e) = result {
            error!("Error handling signaling: {}", e);
        } else {
            info!("Signaling handle is finished");
        }
    });

    // respond immediately with response connected to WS session
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_token_authenticates_reported_server_role() {
        // A desk-server host presents a valid token and reports Server: it is
        // admitted as a Server node.
        assert_eq!(
            adjudicate_role(TokenOutcome::Valid, RemoteDeskTypeEnum::Server),
            RoleDecision::Node(RemoteDeskTypeEnum::Server)
        );
    }

    #[test]
    fn invalid_token_is_rejected_and_never_falls_back_to_cookie() {
        // Regression guard for the contract: a non-empty but invalid token must
        // surface as 401 (Reject), NOT be silently downgraded to a cookie/Browser
        // connection — even when the client also self-reports Server.
        assert_eq!(
            adjudicate_role(TokenOutcome::Invalid, RemoteDeskTypeEnum::Server),
            RoleDecision::Reject
        );
        assert_eq!(
            adjudicate_role(TokenOutcome::Invalid, RemoteDeskTypeEnum::Browser),
            RoleDecision::Reject
        );
    }

    #[test]
    fn absent_token_defers_to_cookie_even_when_self_reporting_server() {
        // No token: the connection must go through cookie auth and become a
        // Browser, regardless of a self-reported Server role. It is never
        // admitted as a Server node off a self-report alone.
        assert_eq!(
            adjudicate_role(TokenOutcome::Absent, RemoteDeskTypeEnum::Server),
            RoleDecision::Cookie
        );
        assert_eq!(
            adjudicate_role(TokenOutcome::Absent, RemoteDeskTypeEnum::Browser),
            RoleDecision::Cookie
        );
    }
}
