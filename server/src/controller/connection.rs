//! Connection-verify endpoint.
//!
//! The desk server dials the remote signaling server / manager itself, so the
//! onboarding wizard and the desk-connection settings page need a way to check a
//! target *before* committing it — reachability, TLS support (for the bare-host
//! `wss`→`ws` scheme resolution), and whether the API token authenticates. A
//! browser cannot open a WebSocket to an arbitrary host to check this (mixed
//! content / CORS), so this backend endpoint performs the probe on its behalf via
//! the zero-side-effect `?probe=1` marker protocol (see
//! [`desk_signal_facade::model::probe`]).
//!
//! Outbound dials are SSRF-guarded at connect time by [`desk_utils::ssrf`]:
//! anonymous (pre-init) callers may only reach public addresses (`Strict`);
//! authenticated (post-init) callers may also reach private / LAN addresses for a
//! self-hosted signaling server (`Relaxed`), while the cloud-metadata floor stays
//! blocked in both. The URL scheme allowlist (`ws` / `wss` / `http` / `https` plus
//! the `wss`→`ws` fallback) is enforced here — deliberately not via
//! `check_provider_url`, whose `Strict` mode is https-only and would reject the
//! `ws` / `http` this feature needs.

use std::sync::Arc;
use std::time::Duration;

use actix_session::Session;
use actix_web::{Error as AWError, HttpResponse, post, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_signal_facade::model::{
    probe::{SIGNALING_PROBE_HEADER, SIGNALING_PROBE_HEADER_VALUE},
    signal::RemoteDeskTypeEnum,
    version::VersionInfo,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse, ssrf::ProviderSsrfMode};
use log::debug;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::settings::SharedSettings;

pub const TAG: &str = "Connection";

/// Signaling path a bare `host[:port]` is expanded against.
const SIGNALING_PATH: &str = "/api/desk/signaling";
/// Per-probe timeout (connect + response).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct ConnectionVerifyParams {
    /// Which link is being verified: `"signaling"` (a bare remote signaling
    /// server) or `"manager"` (also checks the console origin is reachable).
    pub target: String,
    /// Either a bare `host[:port]` (the wizard's domain field — the backend then
    /// resolves `wss`→`ws`) or a full `ws(s)://host[:port]/path` URL (advanced /
    /// desk-connection settings — probed as-is with no fallback).
    pub input: String,
    /// API token to authenticate the probe. Omitted during pure scheme resolution
    /// (domain-field blur), supplied when checking whether the token is accepted.
    pub token: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, Default)]
pub struct ConnectionVerifyResult {
    /// Overall verdict. Signaling target: equals `auth_ok`. Manager target:
    /// `auth_ok && console_ok` (a missing console check counts as ok).
    pub ok: bool,
    /// Whether any HTTP response came back (DNS + TLS + connect all succeeded).
    /// This is what the wizard uses to pick the `wss`/`ws` scheme.
    pub reached: bool,
    /// Whether the signaling endpoint confirmed the token: probe marker header
    /// present **and** status 200. This is what the wizard's "next" gate requires.
    pub auth_ok: bool,
    /// The full `ws(s)://.../path` URL the probe settled on (for the bare-host
    /// case, the scheme that succeeded); echoed back so the wizard can persist it.
    pub resolved_url: Option<String>,
    /// The scheme that succeeded (`"wss"` or `"ws"`).
    pub scheme: Option<String>,
    /// Manager target only: whether the console origin (`https://<host>`) answered.
    pub console_ok: Option<bool>,
    /// Machine-readable outcome ([`DeskErrorCode`]); `0` on success.
    pub error_code: i32,
    /// Human-readable outcome message.
    pub message: String,
}

/// Outcome of probing one candidate endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeOutcome {
    /// Probe marker present and status 200: reachable, is a desk signaling
    /// endpoint, and the token is accepted.
    AuthOk,
    /// Probe marker present and status 401: reachable and a desk signaling
    /// endpoint, but the token was rejected or absent.
    ReachedNoAuth,
    /// An HTTP response came back but without the probe marker, so the target is
    /// not a desk signaling endpoint (some other web server answering).
    ReachedNotSignaling,
    /// The target was refused before dialing (unsupported scheme) or blocked by
    /// the SSRF guard at connect time.
    Blocked,
    /// The probe timed out.
    Timeout,
    /// The target could not be reached (DNS failure, connection refused, TLS
    /// handshake failure).
    Unreachable,
}

impl ProbeOutcome {
    /// Whether an HTTP response came back at all (TLS + connect succeeded).
    fn reached(&self) -> bool {
        matches!(
            self,
            ProbeOutcome::AuthOk | ProbeOutcome::ReachedNoAuth | ProbeOutcome::ReachedNotSignaling
        )
    }
}

/// Whether the input already carries a URL scheme (so it is a full URL, not a
/// bare `host[:port]`).
fn looks_like_full_url(input: &str) -> bool {
    let lower = input.trim().to_ascii_lowercase();
    lower.starts_with("ws://")
        || lower.starts_with("wss://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
}

/// Map a `ws(s)`/`http(s)` URL to the `http(s)` form used for the probe GET, or
/// `None` if the scheme is not in the allowlist. The signaling probe short-circuits
/// before the WebSocket upgrade, so a plain HTTP GET reaches it.
fn probe_http_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("wss://") {
        Some(format!("https://{}", &trimmed[trimmed.len() - rest.len()..]))
    } else if let Some(rest) = lower.strip_prefix("ws://") {
        Some(format!("http://{}", &trimmed[trimmed.len() - rest.len()..]))
    } else if lower.starts_with("https://") || lower.starts_with("http://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Extract the host (without port) from a full URL or bare `host[:port]`, for the
/// manager console check (`https://<host>`).
fn host_of(input: &str) -> Option<String> {
    if looks_like_full_url(input) {
        url::Url::parse(input.trim())
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
    } else {
        // Bare host[:port]: parse via a throwaway scheme so IPv6 brackets / ports
        // are handled by the URL parser.
        url::Url::parse(&format!("wss://{}", input.trim()))
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
    }
}

/// A connect-time DNS resolver that drops any candidate address blocked by the
/// active [`ProviderSsrfMode`]. Resolution runs per connection, just before the
/// socket connects, so a domain (or a rebinding one) that maps to an internal /
/// metadata address is caught authoritatively; IP literals are checked too since
/// `lookup_host` echoes them back through the same filter.
#[derive(Clone)]
struct SsrfResolver {
    mode: ProviderSsrfMode,
}

impl actix_tls::connect::Resolve for SsrfResolver {
    fn lookup<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> futures_util::future::LocalBoxFuture<
        'a,
        Result<Vec<std::net::SocketAddr>, Box<dyn std::error::Error>>,
    > {
        let mode = self.mode;
        Box::pin(async move {
            let resolved = tokio::net::lookup_host((host, port)).await?;
            let allowed: Vec<std::net::SocketAddr> = resolved
                .filter(|addr| desk_utils::ssrf::check_resolved_ip(addr.ip(), mode).is_ok())
                .collect();
            if allowed.is_empty() {
                // Coarse error: the caller must not learn which internal address
                // was probed.
                return Err(Box::<dyn std::error::Error>::from(
                    "host resolves to a blocked address",
                ));
            }
            Ok(allowed)
        })
    }
}

/// Build an SSRF-guarded, TLS-capable `awc` client for the active mode. awc does
/// not follow redirects by default, which is the behavior we want (no redirect to
/// an internal address).
fn build_probe_client(mode: ProviderSsrfMode) -> awc::Client {
    let mut root_store = rustls::RootCertStore::empty();
    // `certs` carries the successfully-loaded roots even when some platform certs
    // failed to parse; ignoring partial `errors` is fine for a probe client.
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = root_store.add(cert);
    }
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(root_store))
        .with_no_client_auth();

    let tcp = actix_tls::connect::Connector::new(actix_tls::connect::Resolver::custom(
        SsrfResolver { mode },
    ))
    .service();

    awc::Client::builder()
        .connector(
            awc::Connector::new()
                .connector(tcp)
                .timeout(PROBE_TIMEOUT)
                .rustls_0_23(Arc::new(tls_config)),
        )
        .finish()
}

/// Build a fully-valid `VersionInfo` probe query (all required fields present so
/// the signaling endpoint deserializes it and reads the token) with `probe=1`.
/// Returns `None` only if encoding fails (treated as unreachable by the caller).
fn build_probe_query(token: Option<&str>) -> Option<String> {
    let mut version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        crate::version::SERVER_BUILD_NUMBER,
        crate::version::SERVER_COMMIT_HASH.to_string(),
        RemoteDeskTypeEnum::Server,
        None,
        None,
    );
    version_info.token = token.map(|t| t.to_string());
    let encoded = serde_urlencoded::to_string(&version_info).ok()?;
    Some(format!("{encoded}&probe=1"))
}

/// Probe one `ws(s)` (or `http(s)`) URL with the given token. Never panics;
/// classifies every failure into a coarse [`ProbeOutcome`].
async fn probe_signaling(client: &awc::Client, ws_url: &str, token: Option<&str>) -> ProbeOutcome {
    let Some(http_url) = probe_http_url(ws_url) else {
        return ProbeOutcome::Blocked;
    };
    let Some(query) = build_probe_query(token) else {
        return ProbeOutcome::Unreachable;
    };
    let full = format!("{http_url}?{query}");

    match client.get(&full).timeout(PROBE_TIMEOUT).send().await {
        Ok(resp) => {
            let has_marker = resp
                .headers()
                .get(SIGNALING_PROBE_HEADER)
                .map(|v| v.as_bytes() == SIGNALING_PROBE_HEADER_VALUE.as_bytes())
                .unwrap_or(false);
            if !has_marker {
                return ProbeOutcome::ReachedNotSignaling;
            }
            match resp.status().as_u16() {
                200 => ProbeOutcome::AuthOk,
                401 => ProbeOutcome::ReachedNoAuth,
                // A marker with any other status is unexpected; treat as not a
                // usable signaling endpoint rather than as authenticated.
                _ => ProbeOutcome::ReachedNotSignaling,
            }
        }
        Err(e) => classify_send_error(&e.to_string()),
    }
}

/// Coarse classification of an `awc` send error string into a [`ProbeOutcome`].
fn classify_send_error(err: &str) -> ProbeOutcome {
    let lower = err.to_ascii_lowercase();
    if lower.contains("blocked address") {
        ProbeOutcome::Blocked
    } else if lower.contains("timed out") || lower.contains("timeout") {
        ProbeOutcome::Timeout
    } else {
        ProbeOutcome::Unreachable
    }
}

/// Whether the manager console origin answers an HTTP GET (any status counts as
/// "the frontend server is up").
async fn probe_console(client: &awc::Client, host: &str) -> bool {
    let url = format!("https://{host}");
    matches!(
        client.get(&url).timeout(PROBE_TIMEOUT).send().await,
        Ok(_resp)
    )
}

/// Turn a chosen probe outcome (plus resolved scheme/url and optional console
/// check) into the wire result.
fn build_result(
    outcome: &ProbeOutcome,
    resolved_url: Option<String>,
    scheme: Option<String>,
    console_ok: Option<bool>,
) -> ConnectionVerifyResult {
    let reached = outcome.reached();
    let auth_ok = matches!(outcome, ProbeOutcome::AuthOk);
    let (error_code, message) = match outcome {
        ProbeOutcome::AuthOk => (DeskErrorCode::SUCCESS.code(), "connection ok".to_string()),
        ProbeOutcome::ReachedNoAuth => (
            DeskErrorCode::CONNECTION_AUTH_FAILED.code(),
            "reached the signaling endpoint but the API token was rejected".to_string(),
        ),
        ProbeOutcome::ReachedNotSignaling => (
            DeskErrorCode::CONNECTION_NOT_SIGNALING.code(),
            "reached a server but it is not a desk signaling endpoint".to_string(),
        ),
        ProbeOutcome::Blocked => (
            DeskErrorCode::CONNECTION_TARGET_BLOCKED.code(),
            "the target address or scheme is not allowed".to_string(),
        ),
        ProbeOutcome::Timeout => (
            DeskErrorCode::TIMEOUT.code(),
            "the connection attempt timed out".to_string(),
        ),
        ProbeOutcome::Unreachable => (
            DeskErrorCode::CONNECTION_UNREACHABLE.code(),
            "could not reach the target (DNS, TLS, or connection failure)".to_string(),
        ),
    };
    // Overall verdict: signaling = auth_ok; manager = auth_ok && console_ok.
    let ok = auth_ok && console_ok.unwrap_or(true);
    ConnectionVerifyResult {
        ok,
        reached,
        auth_ok,
        resolved_url,
        scheme,
        console_ok,
        error_code,
        message,
    }
}

/// Resolve a bare `host[:port]` by probing `wss` first then `ws`, returning the
/// first that is reachable along with its scheme + full URL. If neither is
/// reachable, returns the `wss` outcome (so the message reflects the primary
/// attempt) with the `wss` URL.
async fn resolve_bare_host(
    client: &awc::Client,
    host: &str,
    token: Option<&str>,
) -> (ProbeOutcome, String, String) {
    let wss_url = format!("wss://{host}{SIGNALING_PATH}");
    let wss_outcome = probe_signaling(client, &wss_url, token).await;
    if wss_outcome.reached() {
        return (wss_outcome, "wss".to_string(), wss_url);
    }
    let ws_url = format!("ws://{host}{SIGNALING_PATH}");
    let ws_outcome = probe_signaling(client, &ws_url, token).await;
    if ws_outcome.reached() {
        return (ws_outcome, "ws".to_string(), ws_url);
    }
    // Neither reachable: report the primary (wss) attempt against the wss URL.
    (wss_outcome, "wss".to_string(), wss_url)
}

#[utoipa::path(
    tag = TAG,
    summary = "Verify a signaling / manager connection target",
    request_body(content = ConnectionVerifyParams),
    responses(
        (status = 200, description = "Verification result", body = RestResponse<ConnectionVerifyResult>),
    ),
)]
#[post("/api/connection/verify")]
pub async fn verify_connection(
    request_json: web::Json<ConnectionVerifyParams>,
    settings: web::Data<SharedSettings>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();

    // Self-authentication + SSRF posture. Before the system is initialized the
    // wizard runs with no account, so the endpoint is open but restricted to
    // public targets (`Strict`). Once initialized it requires a logged-in session
    // and, being an authenticated operator configuring an outbound address, may
    // reach private / LAN addresses (`Relaxed`) — the metadata floor stays blocked
    // in both.
    let initialized = {
        let s = settings.read().await;
        !s.user.login_password.is_empty()
    };
    let mode = if initialized {
        if session.get_current_user::<CurrentUser>()?.is_none() {
            return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
        }
        ProviderSsrfMode::Relaxed
    } else {
        ProviderSsrfMode::Strict
    };

    let input = params.input.trim().to_string();
    if input.is_empty() {
        return Ok(
            HttpResponse::Ok().json(RestResponse::<ConnectionVerifyResult>::failed_with_data(
                DeskErrorCode::INVALID_PARAMS,
                Some("input is empty".to_string()),
                None,
            )),
        );
    }
    let token = params.token.as_deref().filter(|t| !t.is_empty());
    let is_manager = params.target.eq_ignore_ascii_case("manager");

    let client = build_probe_client(mode);

    // Resolve scheme / URL and run the primary signaling probe.
    let (outcome, scheme, resolved_url) = if looks_like_full_url(&input) {
        let scheme = url::Url::parse(&input)
            .ok()
            .map(|u| u.scheme().to_string());
        let outcome = probe_signaling(&client, &input, token).await;
        (outcome, scheme, Some(input.clone()))
    } else {
        let (outcome, scheme, url) = resolve_bare_host(&client, &input, token).await;
        (outcome, Some(scheme), Some(url))
    };

    // Manager target: also check the console origin is reachable.
    let console_ok = if is_manager {
        match host_of(&input) {
            Some(host) => Some(probe_console(&client, &host).await),
            None => Some(false),
        }
    } else {
        None
    };

    let result = build_result(&outcome, resolved_url, scheme, console_ok);
    debug!(
        "connection verify: target={} reached={} auth_ok={} console_ok={:?} code={}",
        params.target, result.reached, result.auth_ok, result.console_ok, result.error_code
    );
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_full_urls_vs_bare_hosts() {
        assert!(looks_like_full_url("wss://a.example/x"));
        assert!(looks_like_full_url("WS://a.example"));
        assert!(looks_like_full_url("https://a.example"));
        assert!(!looks_like_full_url("a.example"));
        assert!(!looks_like_full_url("a.example:8443"));
    }

    #[test]
    fn maps_ws_schemes_to_http_for_probe() {
        assert_eq!(
            probe_http_url("wss://host:8443/api/desk/signaling").as_deref(),
            Some("https://host:8443/api/desk/signaling")
        );
        assert_eq!(
            probe_http_url("ws://host/api/desk/signaling").as_deref(),
            Some("http://host/api/desk/signaling")
        );
        assert_eq!(
            probe_http_url("https://host/x").as_deref(),
            Some("https://host/x")
        );
        // Disallowed schemes are rejected (blocked before dialing).
        assert!(probe_http_url("file:///etc/passwd").is_none());
        assert!(probe_http_url("gopher://host/").is_none());
    }

    #[test]
    fn extracts_host_from_url_and_bare_input() {
        assert_eq!(host_of("wss://a.example:8443/x").as_deref(), Some("a.example"));
        assert_eq!(host_of("a.example:8443").as_deref(), Some("a.example"));
        assert_eq!(host_of("a.example").as_deref(), Some("a.example"));
    }

    #[test]
    fn classifies_send_errors_coarsely() {
        assert_eq!(
            classify_send_error("host resolves to a blocked address"),
            ProbeOutcome::Blocked
        );
        assert_eq!(
            classify_send_error("Connection timed out"),
            ProbeOutcome::Timeout
        );
        assert_eq!(
            classify_send_error("connection refused"),
            ProbeOutcome::Unreachable
        );
    }

    #[test]
    fn auth_ok_only_on_marker_plus_200() {
        // AuthOk is the only outcome that sets auth_ok; a plain 200 without the
        // marker (ReachedNotSignaling) must NOT be treated as authenticated.
        let ok = build_result(&ProbeOutcome::AuthOk, None, Some("wss".into()), None);
        assert!(ok.auth_ok && ok.ok && ok.reached);
        let not_sig = build_result(&ProbeOutcome::ReachedNotSignaling, None, None, None);
        assert!(!not_sig.auth_ok && !not_sig.ok && not_sig.reached);
        assert_eq!(
            not_sig.error_code,
            DeskErrorCode::CONNECTION_NOT_SIGNALING.code()
        );
    }

    #[test]
    fn manager_overall_requires_console_ok() {
        // Auth ok but console down -> overall not ok.
        let r = build_result(&ProbeOutcome::AuthOk, None, Some("wss".into()), Some(false));
        assert!(r.auth_ok);
        assert!(!r.ok, "manager overall must require console_ok");
        // Auth ok and console up -> ok.
        let r = build_result(&ProbeOutcome::AuthOk, None, Some("wss".into()), Some(true));
        assert!(r.ok);
    }

    #[test]
    fn unreachable_and_blocked_are_not_reached() {
        assert!(!ProbeOutcome::Unreachable.reached());
        assert!(!ProbeOutcome::Blocked.reached());
        assert!(!ProbeOutcome::Timeout.reached());
        assert!(ProbeOutcome::ReachedNoAuth.reached());
        let r = build_result(&ProbeOutcome::ReachedNoAuth, None, Some("wss".into()), None);
        assert!(r.reached && !r.auth_ok);
        assert_eq!(r.error_code, DeskErrorCode::CONNECTION_AUTH_FAILED.code());
    }

    #[actix_web::test]
    async fn ssrf_resolver_enforces_mode_per_resolved_ip() {
        use actix_tls::connect::Resolve;
        // Loopback: blocked under Strict (anonymous), allowed under Relaxed
        // (authenticated / LAN self-host). No external DNS: literals resolve
        // locally.
        assert!(
            (SsrfResolver {
                mode: ProviderSsrfMode::Strict
            })
            .lookup("127.0.0.1", 443)
            .await
            .is_err()
        );
        assert!(
            (SsrfResolver {
                mode: ProviderSsrfMode::Relaxed
            })
            .lookup("127.0.0.1", 443)
            .await
            .is_ok()
        );
        // Cloud metadata: blocked under BOTH modes (the hard floor).
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            assert!(
                (SsrfResolver { mode })
                    .lookup("169.254.169.254", 80)
                    .await
                    .is_err()
            );
        }
    }

    fn settings_for_verify(initialized: bool) -> web::Data<SharedSettings> {
        use crate::model::settings::{Args, Settings};
        let mut settings = Settings::default();
        if initialized {
            settings.user.login_user_name = "admin".to_string();
            settings.user.login_password = "pw".to_string();
        } else {
            settings.user.login_password = String::new();
        }
        let mut temp_path = std::env::temp_dir();
        temp_path.push(format!("desk_verify_test_{}.toml", uuid::Uuid::new_v4()));
        settings.args = Args {
            config_file_path: temp_path.to_string_lossy().to_string(),
            ..Default::default()
        };
        web::Data::new(SharedSettings::from(settings))
    }

    #[actix_web::test]
    async fn verify_requires_session_once_initialized() {
        use actix_session::{SessionMiddleware, storage::CookieSessionStore};
        use actix_web::{App, cookie::Key, test};
        let app = test::init_service(
            App::new()
                .app_data(settings_for_verify(true))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(verify_connection),
        )
        .await;
        // Initialized system + no session cookie -> 401 (self-auth), before any
        // outbound probe.
        let params = ConnectionVerifyParams {
            target: "signaling".to_string(),
            input: "1.2.3.4".to_string(),
            token: None,
        };
        let req = test::TestRequest::post()
            .uri("/api/connection/verify")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn verify_uninitialized_is_open_but_blocks_metadata() {
        use actix_session::{SessionMiddleware, storage::CookieSessionStore};
        use actix_web::{App, cookie::Key, test};
        // The probe client builds a rustls config; production installs the process
        // default provider at startup, so mirror that here (idempotent).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let app = test::init_service(
            App::new()
                .app_data(settings_for_verify(false))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(verify_connection),
        )
        .await;
        // Uninitialized: the endpoint is open (no 401), but Strict mode blocks the
        // cloud-metadata address at connect time, so it is never reached.
        let params = ConnectionVerifyParams {
            target: "signaling".to_string(),
            input: "169.254.169.254".to_string(),
            token: None,
        };
        let req = test::TestRequest::post()
            .uri("/api/connection/verify")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
        let body: RestResponse<ConnectionVerifyResult> = test::read_body_json(resp).await;
        let result = body.data.expect("result");
        assert!(!result.reached, "metadata address must never be reached");
        assert!(!result.ok);
    }
}
