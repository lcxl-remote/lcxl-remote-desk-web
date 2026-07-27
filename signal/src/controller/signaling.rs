use std::sync::Arc;

use actix_session::Session;
use actix_web::{HttpRequest, HttpResponse, get, rt, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_signal_facade::{
    model::{
        code_session::{CODE_SESSION_KEY, CodeSessionCookie},
        probe::{SIGNALING_PROBE_HEADER, SIGNALING_PROBE_HEADER_VALUE, is_probe_query},
        signal::{RemoteDeskTypeEnum, TurnProvider},
        version::VersionInfo,
    },
    service::NodeTokenValidator,
};
use desk_turn::runtime::{LiveTurnProvider, TurnRuntimeView};
use log::{error, info};

use crate::{model::SharedConnectionMap, service::handle_signaling};

pub const TAG: &str = "Signaling";

/// Resolve a cookie-authenticated browser identity from the session: the
/// single-account owner (`CurrentUser`), else a capability-scoped code-session
/// (`CodeSessionCookie`, synthesized into a device-scoped routing identity), else
/// `None` (unauthenticated). The single source of truth for browser identity,
/// shared by the main signaling connection and the terminal WS connection so their
/// owner-vs-code-session adjudication cannot drift (a code-session's ceiling must be
/// enforced identically on both). Returns the routing `CurrentUser` plus the raw
/// `CodeSessionCookie` (so the caller can derive the `AuthContext` via
/// [`crate::service::browser_auth_context`]).
pub(crate) fn resolve_browser_identity(
    session: &Session,
) -> Result<Option<(CurrentUser, Option<CodeSessionCookie>)>, actix_web::Error> {
    if let Some(u) = session.get_current_user::<CurrentUser>()? {
        Ok(Some((u, None)))
    } else if let Some(cs) = session.get::<CodeSessionCookie>(CODE_SESSION_KEY)? {
        // Synthesize a routing identity that pins signaling to the redeemed target
        // (reusing the device-scoped `forward_to_peer` isolation). Its authority is
        // *not* owner: the capability ceiling is enforced via the code-session
        // `AuthContext` principal downstream, and it is never stored back as a
        // `CurrentUser`, so it cannot reach the owner-only REST surface.
        let mut u = CurrentUser::new_admin("code_session");
        u.access = Some("device_user".to_string());
        u.target_connection_id = Some(cs.target_connection_id.clone());
        Ok(Some((u, Some(cs))))
    } else {
        Ok(None)
    }
}

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

/// Resolve the only browser display name that may enter a host authorization
/// stamp. Account names come from the authenticated session; capability-scoped
/// code sessions intentionally remain unnamed.
pub(crate) fn trusted_browser_display_name(
    authenticated_user_name: &str,
    is_code_session: bool,
) -> Option<String> {
    (!is_code_session).then(|| authenticated_user_name.to_string())
}

fn apply_server_adjudicated_identity(
    version_info: &mut VersionInfo,
    adjudicated_type: RemoteDeskTypeEnum,
    authenticated_user_name: &str,
    is_code_session: bool,
) {
    version_info.remote_desk_type = adjudicated_type;
    if adjudicated_type == RemoteDeskTypeEnum::Browser {
        version_info.display_name =
            trusted_browser_display_name(authenticated_user_name, is_code_session);
    }
}

#[utoipa::path(
    tag = TAG,
    summary = "Open signaling handle and return a WebSocket stream",
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
    turn_runtime: Option<web::Data<TurnRuntimeView>>,
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

    // Zero-side-effect connection-verify probe: a `?probe=1` request is answered
    // here, after the token has been authenticated but before any side effect —
    // no WebSocket upgrade, no `handle_signaling` spawn, no device registration,
    // no presence / quota. The marker header proves the response came from a
    // genuine desk signaling endpoint; a valid token yields 200, anything else
    // 401. Cookies are never consulted (the wizard runs before login).
    if is_probe_query(req.query_string()) {
        let status = match token_outcome {
            TokenOutcome::Valid => actix_web::http::StatusCode::OK,
            TokenOutcome::Invalid | TokenOutcome::Absent => {
                actix_web::http::StatusCode::UNAUTHORIZED
            }
        };
        return Ok(HttpResponse::build(status)
            .insert_header((SIGNALING_PROBE_HEADER, SIGNALING_PROBE_HEADER_VALUE))
            .finish());
    }

    let reported_type = version_info_opt
        .as_ref()
        .map(|vi| vi.remote_desk_type)
        .unwrap_or(RemoteDeskTypeEnum::Browser);

    let (user, adjudicated_type, code_session) = match adjudicate_role(token_outcome, reported_type)
    {
        RoleDecision::Reject => {
            log::warn!("Invalid node token provided");
            return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
        }
        RoleDecision::Node(role) => {
            info!("Node token validated successfully");
            (CurrentUser::new_admin("server_node"), role, None)
        }
        RoleDecision::Cookie => {
            // No token: authenticate via the session cookie. Such a connection is
            // always a Browser, regardless of any self-reported role. Owner or
            // code-session is resolved by the shared `resolve_browser_identity`;
            // anything else is rejected — a bare anonymous connection is never
            // admitted here.
            match resolve_browser_identity(&session)? {
                Some((u, code_session)) => (u, RemoteDeskTypeEnum::Browser, code_session),
                None => {
                    log::warn!("User not logged in and no valid node token provided");
                    return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
                }
            }
        }
    };

    info!("User {} is signaling", user.name);

    let (res, session, stream) = actix_ws::handle(&req, stream)?;

    let stream = stream
        // Raise the per-frame ceiling above the actix-ws 64 KiB default: a chunked
        // diagnose `CollectResponse` rides the signaling socket as a single
        // (unfragmented) WS text frame up to `COLLECT_CHUNK_PAYLOAD_LIMIT`, which
        // the default would reject with `ProtocolError::Overflow` before
        // continuation aggregation, dropping the edge connection. Mirrors the
        // manager signaling endpoint.
        .max_frame_size(desk_agent_protocol::diagnose::SIGNALING_FRAME_LIMIT)
        .aggregate_continuations()
        .max_continuation_size(desk_agent_protocol::diagnose::SIGNALING_FRAME_LIMIT);

    let mut version_info = version_info_opt.unwrap_or_else(|| VersionInfo {
        api_version: desk_server_version::SERVER_API_VERSION,
        build_number: crate::version::SIGNAL_BUILD_NUMBER,
        commit_hash: crate::version::SIGNAL_COMMIT_HASH.to_string(),
        remote_desk_type: RemoteDeskTypeEnum::Browser,
        operation_system: desk_signal_facade::model::os::OperationSystemEnum::default(),
        display_name: Some(user.name.clone()),
        client_id: None,
        token: None,
        debug_build: false,
        repository_url: None,
        available_exec_shells: None,
        max_ai_command_runtime_ms: None,
    });
    // Overwrite browser identity fields with server-adjudicated values before
    // the connection enters the shared map. Node display names remain device
    // metadata, but browser names may later enter a host authorization stamp and
    // therefore must never come from the signaling query.
    apply_server_adjudicated_identity(
        &mut version_info,
        adjudicated_type,
        &user.name,
        code_session.is_some(),
    );

    let ip = req
        .connection_info()
        .realip_remote_addr()
        .map(|s| s.to_string());
    // A provider that resolves the runtime per call, so a connection opened
    // before a settings change and one opened after both get credentials the
    // server actually validates against.
    let turn_provider = turn_runtime.as_ref().map(|view| {
        std::sync::Arc::new(LiveTurnProvider::new(view.as_ref().clone()))
            as std::sync::Arc<dyn TurnProvider>
    });

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
            turn_provider,
            conn_device_map.map(|d| d.into_inner()),
            code_session,
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
    use actix_web::App;
    // NOTE: do not `use actix_web::test` — it also imports the `test` attribute
    // macro, which shadows the built-in `#[test]` used by the pure-function tests
    // below. Qualify `actix_web::test::*` at call sites instead.

    /// Test double: accepts exactly one known-good token.
    struct MockValidator {
        good: String,
    }

    impl NodeTokenValidator for MockValidator {
        fn validate_node_token<'a>(
            &'a self,
            token: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            let ok = token == self.good;
            Box::pin(async move { ok })
        }
    }

    fn probe_uri(token: Option<&str>) -> String {
        let mut version_info = VersionInfo::new(
            desk_server_version::SERVER_API_VERSION,
            0,
            "test".to_string(),
            RemoteDeskTypeEnum::Server,
            Some("probe-host".to_string()),
            Some("client-1".to_string()),
        );
        version_info.token = token.map(|t| t.to_string());
        let query = serde_urlencoded::to_string(&version_info).expect("encode version info");
        format!("/api/desk/signaling?{query}&probe=1")
    }

    async fn call_probe(
        connection_map: web::Data<SharedConnectionMap>,
        token: Option<&str>,
    ) -> actix_web::dev::ServiceResponse {
        let validator: Arc<dyn NodeTokenValidator> = Arc::new(MockValidator {
            good: "goodtoken".to_string(),
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(connection_map)
                .app_data(web::Data::new(validator))
                .service(open_signaling_handle),
        )
        .await;
        let req = actix_web::test::TestRequest::get()
            .uri(&probe_uri(token))
            .to_request();
        actix_web::test::call_service(&app, req).await
    }

    #[actix_web::test]
    async fn probe_valid_token_returns_200_marker_and_no_side_effects() {
        let connection_map = web::Data::new(SharedConnectionMap::new());
        let resp = call_probe(connection_map.clone(), Some("goodtoken")).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get(SIGNALING_PROBE_HEADER).unwrap(),
            SIGNALING_PROBE_HEADER_VALUE
        );
        // Zero side effects: the probe short-circuits before the WS upgrade /
        // `handle_signaling` spawn, so no connection (and thus no device_code /
        // presence entry) is ever registered.
        assert!(connection_map.read().await.is_empty());
    }

    #[actix_web::test]
    async fn probe_invalid_token_returns_401_marker() {
        let connection_map = web::Data::new(SharedConnectionMap::new());
        let resp = call_probe(connection_map.clone(), Some("wrongtoken")).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get(SIGNALING_PROBE_HEADER).unwrap(),
            SIGNALING_PROBE_HEADER_VALUE
        );
        assert!(connection_map.read().await.is_empty());
    }

    #[actix_web::test]
    async fn probe_absent_token_returns_401_marker() {
        let connection_map = web::Data::new(SharedConnectionMap::new());
        let resp = call_probe(connection_map.clone(), None).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get(SIGNALING_PROBE_HEADER).unwrap(),
            SIGNALING_PROBE_HEADER_VALUE
        );
    }

    /// A chunked diagnose `CollectResponse` rides the signaling socket as a
    /// single (unfragmented) WS text frame up to `COLLECT_CHUNK_PAYLOAD_LIMIT`.
    /// The endpoint raises the codec frame ceiling to `SIGNALING_FRAME_LIMIT`;
    /// the actix-ws 64 KiB default rejects such a frame with
    /// `ProtocolError::Overflow` *before* continuation aggregation, which used to
    /// drop the edge connection and fail the diagnosis. Pin the codec contract:
    /// the default rejects the max chunk frame, the raised ceiling accepts it.
    #[test]
    fn signaling_codec_ceiling_accepts_max_diagnose_chunk_frame() {
        use actix_http::ws::{Codec, Frame, Message, ProtocolError};
        use bytes::BytesMut;
        use desk_agent_protocol::diagnose::{COLLECT_CHUNK_PAYLOAD_LIMIT, SIGNALING_FRAME_LIMIT};
        use tokio_util::codec::{Decoder, Encoder};

        // Encode a maximally-sized client text frame onto the wire (client frames
        // are masked, matching what the edge sends).
        let payload = "A".repeat(COLLECT_CHUNK_PAYLOAD_LIMIT);
        let mut client = Codec::new().client_mode();
        let mut wire = BytesMut::new();
        client
            .encode(Message::Text(payload.into()), &mut wire)
            .expect("encode client frame");

        // The actix-ws default ceiling (64 KiB) rejects it before aggregation.
        let mut default_codec = Codec::new();
        let mut default_buf = wire.clone();
        assert!(
            matches!(
                default_codec.decode(&mut default_buf),
                Err(ProtocolError::Overflow)
            ),
            "default 64 KiB codec must reject a {COLLECT_CHUNK_PAYLOAD_LIMIT}-byte frame"
        );

        // The endpoint's raised ceiling accepts the full frame.
        let mut raised = Codec::new().max_size(SIGNALING_FRAME_LIMIT);
        let mut raised_buf = wire;
        match raised
            .decode(&mut raised_buf)
            .expect("decode under raised ceiling")
        {
            Some(Frame::Text(bytes)) => {
                assert_eq!(bytes.len(), COLLECT_CHUNK_PAYLOAD_LIMIT)
            }
            other => panic!("expected a complete text frame, got {other:?}"),
        }
    }

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
    fn valid_token_admits_reported_support_role() {
        // A desk-server's dedicated temporary-support upstream presents a valid
        // token and reports Support: it is admitted as a Support node (routing-only
        // on a plain signal), not rejected and not coerced to Browser.
        assert_eq!(
            adjudicate_role(TokenOutcome::Valid, RemoteDeskTypeEnum::Support),
            RoleDecision::Node(RemoteDeskTypeEnum::Support)
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

    #[test]
    fn authenticated_browser_name_overrides_self_report() {
        let mut version_info = VersionInfo::new(
            1,
            1,
            "test".to_string(),
            RemoteDeskTypeEnum::Browser,
            Some("spoofed-owner".to_string()),
            None,
        );
        apply_server_adjudicated_identity(
            &mut version_info,
            RemoteDeskTypeEnum::Browser,
            "real-owner",
            false,
        );
        assert_eq!(version_info.display_name.as_deref(), Some("real-owner"));
    }

    #[test]
    fn code_session_browser_has_no_display_name() {
        let mut version_info = VersionInfo::new(
            1,
            1,
            "test".to_string(),
            RemoteDeskTypeEnum::Browser,
            Some("spoofed-owner".to_string()),
            None,
        );
        apply_server_adjudicated_identity(
            &mut version_info,
            RemoteDeskTypeEnum::Browser,
            "code_session",
            true,
        );
        assert_eq!(version_info.display_name, None);
    }

    #[test]
    fn authenticated_node_keeps_device_metadata_name() {
        let mut version_info = VersionInfo::new(
            1,
            1,
            "test".to_string(),
            RemoteDeskTypeEnum::Server,
            Some("device-name".to_string()),
            Some("client-1".to_string()),
        );
        apply_server_adjudicated_identity(
            &mut version_info,
            RemoteDeskTypeEnum::Server,
            "server_node",
            false,
        );
        assert_eq!(version_info.display_name.as_deref(), Some("device-name"));
    }
}
