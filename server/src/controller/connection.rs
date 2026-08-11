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
//! Outbound dials are transport-guarded at connect time via
//! [`crate::transport_guard`]: the cloud-metadata floor is always blocked, private
//! / LAN targets stay reachable (this endpoint always dials the host's own
//! self-hosted signaling server / manager, legitimately on a private address), and
//! a *public* target dialed over a plaintext scheme (`ws` / `http`) is refused when
//! the host's `require_secure_signaling` switch is on. One client is built per
//! scheme so the plaintext-vs-TLS decision is made on the single authoritative
//! resolution. The URL scheme allowlist (`ws` / `wss` / `http` / `https` plus the
//! `wss`→`ws` fallback) is enforced here.

use std::sync::Arc;
use std::time::Duration;

use actix_session::Session;
use actix_web::{Error as AWError, HttpRequest, HttpResponse, post, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_signal_facade::model::{
    probe::{SIGNALING_PROBE_HEADER, SIGNALING_PROBE_HEADER_VALUE},
    signal::RemoteDeskTypeEnum,
    version::VersionInfo,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use log::debug;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    model::settings::SharedSettings,
    service::{
        bootstrap::BootstrapToken,
        client_ip::ClientIpExtractor,
        rate_limit::{AuthRateLimiter, BootstrapAttempt, QuotaDecision},
    },
};

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
    /// Deployment bootstrap token used only while the standalone server is not
    /// initialized and was started with `LRD_BOOTSTRAP_TOKEN`.
    #[serde(default)]
    pub bootstrap_token: Option<String>,
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
    /// Manager target only: whether the console origin answered (over `https`
    /// or, as a fallback, plaintext `http`).
    pub console_ok: Option<bool>,
    /// Whether the resolved connection is TLS-encrypted end to end: the signaling
    /// scheme is `wss` and (for a manager target) the console answered over
    /// `https`. `false` means the target answered only over plaintext (`ws` /
    /// `http`) — a self-hosted server without TLS still works, but the frontend
    /// surfaces this as a security warning rather than a hard failure.
    pub secure: bool,
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
    /// The target resolved to a public address dialed over a plaintext scheme
    /// while `require_secure_signaling` is on. Refused before any TCP connect.
    InsecureTransport,
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
        Some(format!(
            "https://{}",
            &trimmed[trimmed.len() - rest.len()..]
        ))
    } else if let Some(rest) = lower.strip_prefix("ws://") {
        Some(format!("http://{}", &trimmed[trimmed.len() - rest.len()..]))
    } else if lower.starts_with("https://") || lower.starts_with("http://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Extract the host (without port) from a full URL or bare `host[:port]`, for the
/// manager console check (`https://<host>`, falling back to `http://<host>`).
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

/// Build an SSRF-guarded, TLS-capable `awc` client for one probe scheme. awc does
/// not follow redirects by default, which is the behavior we want (no redirect to
/// an internal address).
///
/// The connect-time guard runs on the single authoritative resolution: the
/// metadata floor is always blocked, private / LAN targets stay reachable (this is
/// the self-hosted case, so `allow_private` is always true), and a *public* target
/// dialed over a plaintext scheme is refused when `require_secure_signaling` is on.
/// `scheme_is_tls` is fixed per client so the plaintext-vs-TLS decision needs no
/// second lookup — hence one client per scheme rather than one shared client.
fn build_probe_client(scheme_is_tls: bool, require_secure_signaling: bool) -> awc::Client {
    let mut root_store = rustls::RootCertStore::empty();
    // `certs` carries the successfully-loaded roots even when some platform certs
    // failed to parse; ignoring partial `errors` is fine for a probe client.
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = root_store.add(cert);
    }
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(root_store))
        .with_no_client_auth();

    let guard = crate::transport_guard::TransportGuardResolver::system(
        crate::transport_guard::TransportPolicy {
            allow_private: true,
            scheme_is_tls,
            enforce_public_tls: require_secure_signaling,
        },
    );
    let tcp =
        actix_tls::connect::Connector::new(actix_tls::connect::Resolver::custom(guard)).service();

    awc::Client::builder()
        .connector(
            awc::Connector::new()
                .connector(tcp)
                .timeout(PROBE_TIMEOUT)
                .rustls_0_23(Arc::new(tls_config)),
        )
        .finish()
}

/// Pick the right per-scheme probe client for a `ws(s)`/`http(s)` URL.
fn client_for_url<'a, T>(url: &str, secure: &'a T, plain: &'a T) -> &'a T {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("wss://") || lower.starts_with("https://") {
        secure
    } else {
        plain
    }
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
async fn probe_signaling(
    client: &awc::Client,
    ws_url: &str,
    token: Option<&str>,
    require_secure_signaling: bool,
) -> ProbeOutcome {
    // Guard IP-literal targets before dialing (the resolver-based guard never sees
    // them). A domain is deferred to the connect-time resolver / error mapping.
    if let Some(refused) = precheck_literal_target(ws_url, require_secure_signaling) {
        return refused;
    }
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
    if lower.contains("requires tls") {
        // The transport guard refused a public plaintext candidate. Checked before
        // the generic "blocked address" so it maps to the dedicated outcome.
        ProbeOutcome::InsecureTransport
    } else if lower.contains("blocked address") {
        ProbeOutcome::Blocked
    } else if lower.contains("timed out") || lower.contains("timeout") {
        ProbeOutcome::Timeout
    } else {
        ProbeOutcome::Unreachable
    }
}

/// Outcome of probing the manager console origin: whether it answered and, if so,
/// over which scheme (any HTTP status counts as "the frontend server is up").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsoleProbe {
    /// Answered over `https` (encrypted).
    Https,
    /// Answered only over plaintext `http` (the `https` attempt failed first).
    Http,
    /// Did not answer on either scheme.
    Unreachable,
}

impl ConsoleProbe {
    /// Whether the console answered at all (either scheme).
    fn reached(self) -> bool {
        !matches!(self, ConsoleProbe::Unreachable)
    }
}

/// Probe the manager console origin, preferring `https` and falling back to
/// plaintext `http` so a self-hosted manager without TLS is still reachable. The
/// `http` fallback uses the plaintext-scheme client, so a *public* console is
/// refused over `http` when secure-signaling is enforced (a private / LAN console
/// still answers over `http`).
async fn probe_console(
    secure: &awc::Client,
    plain: &awc::Client,
    host: &str,
    require_secure_signaling: bool,
) -> ConsoleProbe {
    // Each scheme attempt guards its IP-literal target before dialing (a refused
    // literal simply counts as "did not answer over that scheme").
    let https = format!("https://{host}");
    if precheck_literal_target(&https, require_secure_signaling).is_none()
        && secure
            .get(&https)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .is_ok()
    {
        return ConsoleProbe::Https;
    }
    let http = format!("http://{host}");
    if precheck_literal_target(&http, require_secure_signaling).is_none()
        && plain.get(&http).timeout(PROBE_TIMEOUT).send().await.is_ok()
    {
        return ConsoleProbe::Http;
    }
    ConsoleProbe::Unreachable
}

/// Whether a resolved signaling scheme is TLS-encrypted.
fn scheme_is_secure(scheme: Option<&str>) -> bool {
    matches!(scheme, Some("wss") | Some("https"))
}

/// Static pre-dial transport judgment for an IP-literal probe target. The
/// actix-tls resolver short-circuits an IP literal before the custom transport
/// guard runs, so a literal must be judged here, before the dial. Returns
/// `Some(outcome)` when the literal is refused (caller skips the dial); `None`
/// when it is allowed, the host is a domain (judged at connect time by the guard
/// resolver), or the URL does not parse (left to the normal dial + error mapping).
/// `allow_private` is always true here (host↔manager wiring reaches LAN targets).
fn precheck_literal_target(url: &str, require_secure_signaling: bool) -> Option<ProbeOutcome> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?;
    let scheme_is_tls = scheme_is_secure(Some(parsed.scheme()));
    match desk_utils::ssrf::check_transport_for_host(
        host,
        true,
        scheme_is_tls,
        require_secure_signaling,
    ) {
        Ok(()) => None,
        Err(desk_utils::ssrf::SsrfError::InsecureTransport) => {
            Some(ProbeOutcome::InsecureTransport)
        }
        Err(_) => Some(ProbeOutcome::Blocked),
    }
}

/// Turn a chosen probe outcome (plus resolved scheme/url and optional console
/// check) into the wire result.
fn build_result(
    outcome: &ProbeOutcome,
    resolved_url: Option<String>,
    scheme: Option<String>,
    console_ok: Option<bool>,
    secure: bool,
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
        ProbeOutcome::InsecureTransport => (
            DeskErrorCode::CONNECTION_INSECURE_TRANSPORT.code(),
            "the target is public but the URL is plaintext; use wss:// / https:// \
             or disable secure-signaling enforcement"
                .to_string(),
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
        secure,
        error_code,
        message,
    }
}

/// Resolve a bare `host[:port]` by probing `wss` first then `ws`, returning the
/// first that is reachable along with its scheme + full URL. If neither is
/// reachable, returns the `wss` outcome (so the message reflects the primary
/// attempt) with the `wss` URL.
async fn resolve_bare_host(
    secure: &awc::Client,
    plain: &awc::Client,
    host: &str,
    token: Option<&str>,
    require_secure_signaling: bool,
) -> (ProbeOutcome, String, String) {
    let wss_url = format!("wss://{host}{SIGNALING_PATH}");
    let wss_outcome = probe_signaling(secure, &wss_url, token, require_secure_signaling).await;
    if wss_outcome.reached() {
        return (wss_outcome, "wss".to_string(), wss_url);
    }
    let ws_url = format!("ws://{host}{SIGNALING_PATH}");
    let ws_outcome = probe_signaling(plain, &ws_url, token, require_secure_signaling).await;
    if ws_outcome.reached() {
        return (ws_outcome, "ws".to_string(), ws_url);
    }
    // Neither reachable. If the plaintext attempt was refused specifically because
    // the target is public and secure-signaling is enforced, surface that (it is
    // more actionable than the wss "unreachable"): the user should switch to wss://
    // or deliberately disable enforcement. Otherwise report the primary (wss)
    // attempt against the wss URL.
    if ws_outcome == ProbeOutcome::InsecureTransport {
        return (ws_outcome, "ws".to_string(), ws_url);
    }
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
    req: HttpRequest,
    request_json: web::Json<ConnectionVerifyParams>,
    settings: web::Data<SharedSettings>,
    bootstrap: web::Data<BootstrapToken>,
    client_ip: web::Data<ClientIpExtractor>,
    rate_limiter: web::Data<Arc<AuthRateLimiter>>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();

    // Self-authentication + SSRF posture. This endpoint dials the host's own
    // signaling server / manager, which is legitimately reached over a private /
    // LAN address in self-hosted and private-cloud deployments (e.g. a manager on
    // `192.168.x.x`). So it uses `Relaxed` — private / LAN targets are allowed
    // while the cloud-metadata hard floor (169.254.169.254 etc.) stays blocked —
    // regardless of init state. This differs from the model-provider guard, whose
    // `Strict` public posture does not fit host↔manager wiring. Access control is
    // orthogonal: before the system is initialized the onboarding wizard runs with
    // no account so the endpoint is open; once initialized it requires a logged-in
    // session.
    let (initialized, require_secure_signaling) = {
        let s = settings.read().await;
        (
            !s.user.login_password.is_empty(),
            s.system.require_secure_signaling,
        )
    };
    if initialized && session.get_current_user::<CurrentUser>()?.is_none() {
        return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
    }

    let input = params.input.trim().to_string();
    if input.is_empty() {
        return Ok(HttpResponse::Ok().json(
            RestResponse::<ConnectionVerifyResult>::failed_with_data(
                DeskErrorCode::INVALID_PARAMS,
                Some("input is empty".to_string()),
                None,
            ),
        ));
    }
    if !initialized {
        let network_key = client_ip.network_key(&req);
        match bootstrap.evaluate(
            rate_limiter.get_ref().as_ref(),
            network_key,
            params.bootstrap_token.as_deref(),
        ) {
            BootstrapAttempt::Allowed => {}
            BootstrapAttempt::Invalid => {
                return Ok(HttpResponse::Ok().json(
                    RestResponse::<ConnectionVerifyResult>::failed_with_data(
                        DeskErrorCode::PERMISSION_ERROR,
                        Some("Invalid bootstrap token".to_string()),
                        None,
                    ),
                ));
            }
            BootstrapAttempt::Limited => {
                return Ok(HttpResponse::Ok().json(
                    RestResponse::<ConnectionVerifyResult>::failed_with_data(
                        DeskErrorCode::TOO_MANY_ATTEMPTS,
                        Some("Too many attempts. Please try again later.".to_string()),
                        None,
                    ),
                ));
            }
        }
        if rate_limiter.consume_probe(network_key) == QuotaDecision::Limited {
            return Ok(HttpResponse::Ok().json(
                RestResponse::<ConnectionVerifyResult>::failed_with_data(
                    DeskErrorCode::TOO_MANY_ATTEMPTS,
                    Some("Too many connection probes. Please try again later.".to_string()),
                    None,
                ),
            ));
        }
    }
    let token = params.token.as_deref().filter(|t| !t.is_empty());
    let is_manager = params.target.eq_ignore_ascii_case("manager");

    // One client per scheme: the transport guard bakes `scheme_is_tls` in so a
    // public plaintext candidate is refused before any TCP connect (no second
    // lookup that could rebind). Both share the same secure-signaling enforcement.
    let client_secure = build_probe_client(true, require_secure_signaling);
    let client_plain = build_probe_client(false, require_secure_signaling);

    // Resolve scheme / URL and run the primary signaling probe.
    let (outcome, scheme, resolved_url) = if looks_like_full_url(&input) {
        let scheme = url::Url::parse(&input).ok().map(|u| u.scheme().to_string());
        let client = client_for_url(&input, &client_secure, &client_plain);
        let outcome = probe_signaling(client, &input, token, require_secure_signaling).await;
        (outcome, scheme, Some(input.clone()))
    } else {
        let (outcome, scheme, url) = resolve_bare_host(
            &client_secure,
            &client_plain,
            &input,
            token,
            require_secure_signaling,
        )
        .await;
        (outcome, Some(scheme), Some(url))
    };

    // Manager target: also check the console origin is reachable, recording
    // whether it answered over TLS.
    let console = if is_manager {
        match host_of(&input) {
            Some(host) => Some(
                probe_console(
                    &client_secure,
                    &client_plain,
                    &host,
                    require_secure_signaling,
                )
                .await,
            ),
            None => Some(ConsoleProbe::Unreachable),
        }
    } else {
        None
    };
    let console_ok = console.map(ConsoleProbe::reached);

    // End-to-end encryption verdict: the signaling scheme must be TLS and, for a
    // manager, the console must also have answered over `https`. A reachable but
    // plaintext target is still allowed (not a hard failure) so a self-hosted
    // manager without TLS works; the frontend surfaces the downgrade as a warning.
    let signaling_secure = scheme_is_secure(scheme.as_deref());
    let secure = match console {
        Some(c) => signaling_secure && matches!(c, ConsoleProbe::Https),
        None => signaling_secure,
    };

    let result = build_result(&outcome, resolved_url, scheme, console_ok, secure);
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
        assert_eq!(
            host_of("wss://a.example:8443/x").as_deref(),
            Some("a.example")
        );
        assert_eq!(host_of("a.example:8443").as_deref(), Some("a.example"));
        assert_eq!(host_of("a.example").as_deref(), Some("a.example"));
    }

    #[test]
    fn precheck_literal_target_guards_ip_literals_before_dial() {
        // Public IP literal over plaintext with enforcement on → refused before dial
        // (this is the actix-tls short-circuit path the resolver-based guard misses).
        assert_eq!(
            precheck_literal_target("ws://203.0.113.5/api/desk/signaling", true),
            Some(ProbeOutcome::InsecureTransport)
        );
        assert_eq!(
            precheck_literal_target("http://203.0.113.5:8443", true),
            Some(ProbeOutcome::InsecureTransport)
        );
        // Same literal over TLS, or with enforcement off → allowed (deferred to dial).
        assert_eq!(
            precheck_literal_target("wss://203.0.113.5/api/desk/signaling", true),
            None
        );
        assert_eq!(
            precheck_literal_target("ws://203.0.113.5/api/desk/signaling", false),
            None
        );
        // Metadata floor literal → always blocked, ignoring the switch and scheme.
        for enforce in [true, false] {
            assert_eq!(
                precheck_literal_target("wss://169.254.169.254/api", enforce),
                Some(ProbeOutcome::Blocked)
            );
        }
        // Private literal → allowed over plaintext (host↔manager reaches the LAN).
        assert_eq!(
            precheck_literal_target("ws://10.0.0.5/api/desk/signaling", true),
            None
        );
        // Domain host → deferred to the connect-time resolver (None here).
        assert_eq!(
            precheck_literal_target("ws://sig.example/api/desk/signaling", true),
            None
        );
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
        let ok = build_result(&ProbeOutcome::AuthOk, None, Some("wss".into()), None, true);
        assert!(ok.auth_ok && ok.ok && ok.reached);
        let not_sig = build_result(&ProbeOutcome::ReachedNotSignaling, None, None, None, false);
        assert!(!not_sig.auth_ok && !not_sig.ok && not_sig.reached);
        assert_eq!(
            not_sig.error_code,
            DeskErrorCode::CONNECTION_NOT_SIGNALING.code()
        );
    }

    #[test]
    fn manager_overall_requires_console_ok() {
        // Auth ok but console down -> overall not ok.
        let r = build_result(
            &ProbeOutcome::AuthOk,
            None,
            Some("wss".into()),
            Some(false),
            false,
        );
        assert!(r.auth_ok);
        assert!(!r.ok, "manager overall must require console_ok");
        // Auth ok and console up -> ok.
        let r = build_result(
            &ProbeOutcome::AuthOk,
            None,
            Some("wss".into()),
            Some(true),
            true,
        );
        assert!(r.ok);
    }

    #[test]
    fn unreachable_and_blocked_are_not_reached() {
        assert!(!ProbeOutcome::Unreachable.reached());
        assert!(!ProbeOutcome::Blocked.reached());
        assert!(!ProbeOutcome::Timeout.reached());
        assert!(ProbeOutcome::ReachedNoAuth.reached());
        let r = build_result(
            &ProbeOutcome::ReachedNoAuth,
            None,
            Some("wss".into()),
            None,
            true,
        );
        assert!(r.reached && !r.auth_ok);
        assert_eq!(r.error_code, DeskErrorCode::CONNECTION_AUTH_FAILED.code());
    }

    #[test]
    fn scheme_security_is_tls_only() {
        assert!(scheme_is_secure(Some("wss")));
        assert!(scheme_is_secure(Some("https")));
        assert!(!scheme_is_secure(Some("ws")));
        assert!(!scheme_is_secure(Some("http")));
        assert!(!scheme_is_secure(None));
    }

    #[test]
    fn console_probe_reached_covers_both_schemes() {
        assert!(ConsoleProbe::Https.reached());
        assert!(ConsoleProbe::Http.reached());
        assert!(!ConsoleProbe::Unreachable.reached());
    }

    #[test]
    fn plaintext_manager_is_reachable_but_not_secure() {
        // A manager answering only over ws + http: overall ok (not blocked) but
        // flagged insecure so the frontend can warn.
        let r = build_result(
            &ProbeOutcome::AuthOk,
            None,
            Some("ws".into()),
            Some(true),
            false,
        );
        assert!(r.ok, "plaintext manager must not be hard-blocked");
        assert!(!r.secure, "plaintext manager must be flagged insecure");
        // A fully TLS manager (wss + https): ok and secure.
        let r = build_result(
            &ProbeOutcome::AuthOk,
            None,
            Some("wss".into()),
            Some(true),
            true,
        );
        assert!(r.ok && r.secure);
    }

    #[test]
    fn insecure_transport_error_maps_to_dedicated_code() {
        // The transport guard's "requires tls" marker classifies to
        // InsecureTransport (checked before the generic "blocked address").
        assert_eq!(
            classify_send_error(
                "target requires TLS: plaintext transport to a public address is not allowed"
            ),
            ProbeOutcome::InsecureTransport
        );
        // ...and it carries its own error code (not the opaque TARGET_BLOCKED), so
        // the wizard can prompt "use wss:// or disable enforcement".
        let r = build_result(
            &ProbeOutcome::InsecureTransport,
            Some("ws://public.example/x".into()),
            Some("ws".into()),
            None,
            false,
        );
        assert!(!r.reached && !r.ok && !r.secure);
        assert_eq!(
            r.error_code,
            DeskErrorCode::CONNECTION_INSECURE_TRANSPORT.code()
        );
    }

    #[test]
    fn client_for_url_picks_scheme() {
        // The selector is transport-agnostic; sentinels keep this unit test from
        // initializing TLS merely to compare references.
        let secure = "secure";
        let plain = "plain";
        let s_ptr = std::ptr::from_ref(client_for_url("wss://h/x", &secure, &plain));
        assert_eq!(s_ptr, std::ptr::from_ref(&secure));
        let s_ptr = std::ptr::from_ref(client_for_url("https://h", &secure, &plain));
        assert_eq!(s_ptr, std::ptr::from_ref(&secure));
        let p_ptr = std::ptr::from_ref(client_for_url("ws://h/x", &secure, &plain));
        assert_eq!(p_ptr, std::ptr::from_ref(&plain));
        let p_ptr = std::ptr::from_ref(client_for_url("http://h", &secure, &plain));
        assert_eq!(p_ptr, std::ptr::from_ref(&plain));
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
            config_file_path: Some(temp_path.clone()),
            ..Default::default()
        };
        web::Data::new(SharedSettings::from(settings))
    }

    fn verify_auth_data() -> (
        web::Data<BootstrapToken>,
        web::Data<ClientIpExtractor>,
        web::Data<Arc<AuthRateLimiter>>,
    ) {
        (
            web::Data::new(BootstrapToken::disabled()),
            web::Data::new(ClientIpExtractor::default()),
            web::Data::new(Arc::new(AuthRateLimiter::new(64))),
        )
    }

    #[actix_web::test]
    async fn verify_requires_session_once_initialized() {
        use actix_session::{SessionMiddleware, storage::CookieSessionStore};
        use actix_web::{App, cookie::Key, test};
        let (bootstrap, client_ip, limiter) = verify_auth_data();
        let app = test::init_service(
            App::new()
                .app_data(settings_for_verify(true))
                .app_data(bootstrap)
                .app_data(client_ip)
                .app_data(limiter)
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
            bootstrap_token: None,
        };
        let req = test::TestRequest::post()
            .peer_addr("192.0.2.1:1234".parse().unwrap())
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
        let (bootstrap, client_ip, limiter) = verify_auth_data();
        let app = test::init_service(
            App::new()
                .app_data(settings_for_verify(false))
                .app_data(bootstrap)
                .app_data(client_ip)
                .app_data(limiter)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(verify_connection),
        )
        .await;
        // Uninitialized: the endpoint is open (no 401). Even under Relaxed the
        // cloud-metadata hard floor still blocks this address at connect time, so
        // it is never reached.
        let params = ConnectionVerifyParams {
            target: "signaling".to_string(),
            input: "169.254.169.254".to_string(),
            token: None,
            bootstrap_token: None,
        };
        let req = test::TestRequest::post()
            .peer_addr("192.0.2.2:1234".parse().unwrap())
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

    #[actix_web::test]
    async fn wrong_bootstrap_token_does_not_consume_probe_quota() {
        use actix_session::{SessionMiddleware, storage::CookieSessionStore};
        use actix_web::{App, cookie::Key, test};
        let limiter = Arc::new(AuthRateLimiter::new(64));
        let extractor = ClientIpExtractor::default();
        let peer = "192.0.2.44:1234".parse().unwrap();
        let app = test::init_service(
            App::new()
                .app_data(settings_for_verify(false))
                .app_data(web::Data::new(BootstrapToken::required("correct-token")))
                .app_data(web::Data::new(extractor.clone()))
                .app_data(web::Data::new(Arc::clone(&limiter)))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(verify_connection),
        )
        .await;
        let params = ConnectionVerifyParams {
            target: "signaling".to_string(),
            input: "169.254.169.254".to_string(),
            token: None,
            bootstrap_token: Some("wrong-token".to_string()),
        };
        let req = test::TestRequest::post()
            .peer_addr(peer)
            .uri("/api/connection/verify")
            .set_json(&params)
            .to_request();
        let response = test::call_service(&app, req).await;
        let body: RestResponse<ConnectionVerifyResult> = test::read_body_json(response).await;
        assert_eq!(body.code, DeskErrorCode::PERMISSION_ERROR.code());
        let request = test::TestRequest::default()
            .peer_addr(peer)
            .to_http_request();
        assert_eq!(limiter.probe_count(&extractor.network_key(&request)), 0);
    }

    #[actix_web::test]
    async fn verify_uninitialized_allows_private_target() {
        use actix_session::{SessionMiddleware, storage::CookieSessionStore};
        use actix_web::{App, cookie::Key, test};
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (bootstrap, client_ip, limiter) = verify_auth_data();
        let app = test::init_service(
            App::new()
                .app_data(settings_for_verify(false))
                .app_data(bootstrap)
                .app_data(client_ip)
                .app_data(limiter)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(verify_connection),
        )
        .await;
        // Uninitialized onboarding wizard pointing at a private / LAN manager (the
        // self-hosted case). A full `ws://` URL on loopback avoids any TLS
        // handshake and hits a port with no listener, so the probe fails with
        // "unreachable" — crucially NOT "blocked": the private address is allowed
        // past the SSRF guard (Relaxed), which is the regression this guards.
        let params = ConnectionVerifyParams {
            target: "manager".to_string(),
            input: "ws://127.0.0.1:9/api/desk/signaling".to_string(),
            token: None,
            bootstrap_token: None,
        };
        let req = test::TestRequest::post()
            .peer_addr("192.0.2.3:1234".parse().unwrap())
            .uri("/api/connection/verify")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
        let body: RestResponse<ConnectionVerifyResult> = test::read_body_json(resp).await;
        let result = body.data.expect("result");
        assert_ne!(
            result.error_code,
            DeskErrorCode::CONNECTION_TARGET_BLOCKED.code(),
            "a private LAN target must not be blocked by the SSRF guard in the wizard"
        );
    }
}
