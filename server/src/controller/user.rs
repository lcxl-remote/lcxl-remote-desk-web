use actix_session::Session;
use actix_web::{
    Error as AWError, FromRequest, HttpRequest, HttpResponse,
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    get,
    http::Method,
    middleware::Next,
    web,
};
use desk_server_user::{
    model::{CurrentUser, NoLogintUser, UserRespone},
    service::UserSessionAccessor,
};
use desk_signal_facade::model::code_session::{CODE_SESSION_KEY, CodeSessionCookie};
use desk_signal_facade::model::security_settings::SecuritySettings;
use log::{info, warn};

use crate::model::security_approval::effective_permission;
use crate::model::settings::SharedSettings;

pub const TAG: &str = "User";

#[utoipa::path(
    tag = TAG,
    summary = "Get current user",
    responses(
        (status = 200, description = "Current user info", body = UserRespone<CurrentUser>),
        (status  = 401, description = "Unauthorized", body = UserRespone<NoLogintUser>),
    ),
)]
#[get("/api/currentUser")]
pub async fn get_current_user(req: HttpRequest, session: Session) -> Result<HttpResponse, AWError> {
    info!("Connection Info: {:?}", req.connection_info());
    if let Some(client_ip_str) = req.connection_info().realip_remote_addr() {
        info!("Client IP: {}", client_ip_str);
    } else {
        warn!("No client IP found in request");
    }

    if let Some(current_user) = session.get_current_user()? {
        let user_response = UserRespone::<CurrentUser> {
            data: current_user,
            error_code: 0,
            error_message: String::from(""),
            success: true,
        };

        info!("Current user: {:?}", user_response.data);
        return Ok(HttpResponse::Ok().json(user_response));
    }
    warn!("User is not logged in.");
    let no_login_user = NoLogintUser { login: false };
    let user_response = UserRespone::<NoLogintUser> {
        data: no_login_user,
        error_code: 401,
        error_message: String::from("User is not logged in."),
        success: true,
    };
    Ok(HttpResponse::Unauthorized().json(user_response))
}

/// A capability dimension a code-session may exercise over REST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopedCapability {
    FileBrowse,
    Terminal,
}

/// Whether a code-session may reach a REST route, and if so which target
/// connection and capability it addresses (so the caller can enforce target-scope
/// and the capability ceiling).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CodeSessionRoute {
    /// Not reachable by a code-session — an owner-plane route, a non-GET method,
    /// or a body-addressed request whose target cannot be verified before the body
    /// is read. Default-deny.
    Denied,
    /// A capability-carrier route addressing `target` under `capability`.
    Scoped {
        target: String,
        capability: ScopedCapability,
    },
}

/// Classify a request for a code-session against the fixed allowlist of
/// capability-carrier routes. This is the default-deny gate: only the file-list
/// and terminal open/list routes — whose target is addressable from the path or
/// query (never a request body) — are reachable, and only as `Scoped` so the
/// caller still enforces `target == redeemed target` and the code's ceiling.
/// Everything else (settings, system info, service management, api tokens, turn
/// keys, device-code CRUD, virtual-display, file delete, connection listing, …)
/// is `Denied`. Pure over its inputs so the allowlist is unit-testable.
fn classify_code_session_route(method: &Method, path: &str, query: &str) -> CodeSessionRoute {
    if method != Method::GET {
        return CodeSessionRoute::Denied;
    }
    // Terminal open (`/terminal/{cid}`) and list (`/terminals/{cid}`): the target
    // is the single trailing path segment. Check the longer prefix first.
    for prefix in ["/api/desk/terminals/", "/api/desk/terminal/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if rest.is_empty() || rest.contains('/') {
                return CodeSessionRoute::Denied;
            }
            return CodeSessionRoute::Scoped {
                target: rest.to_string(),
                capability: ScopedCapability::Terminal,
            };
        }
    }
    // File list (`/file/list?connection_id=…`): the target is a query param.
    if path == "/api/desk/file/list" {
        if let Some(target) = query_param(query, "connection_id")
            && !target.is_empty()
        {
            return CodeSessionRoute::Scoped {
                target,
                capability: ScopedCapability::FileBrowse,
            };
        }
    }
    CodeSessionRoute::Denied
}

/// Extract a single query-string parameter by key (first occurrence).
fn query_param(query: &str, key: &str) -> Option<String> {
    serde_urlencoded::from_str::<Vec<(String, String)>>(query)
        .ok()?
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// Whether the code's ceiling, met with the host global, hard-denies `capability`
/// (`meet == Some(false)`). `None` / `Some(true)` pass through to the host, which
/// prompts or auto-accepts per its own settings.
fn capability_hard_denied(
    ceiling: &SecuritySettings,
    global: &SecuritySettings,
    capability: ScopedCapability,
) -> bool {
    let eff = match capability {
        ScopedCapability::FileBrowse => {
            effective_permission(Some(ceiling), global.allow_file_browse, |c| {
                c.allow_file_browse
            })
        }
        ScopedCapability::Terminal => {
            effective_permission(Some(ceiling), global.allow_terminal, |c| c.allow_terminal)
        }
    };
    eff == Some(false)
}

/// Default-deny device-scope guard for the `/api` surface. An owner
/// (`CurrentUser`) keeps full access; a capability-scoped code-session is
/// restricted to its redeemed target's capability-carrier routes (met against the
/// code's ceiling); anything else is anonymous and rejected. This is the REST-side
/// enforcement point that keeps a redeemed code off every owner-plane endpoint and
/// off other devices.
pub async fn enforce_device_scope(
    mut req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    let session = {
        let (http_request, payload) = req.parts_mut();
        Session::from_request(http_request, payload).await
    }?;

    // Owner (single account) — full access, current behavior.
    if session.get_current_user::<CurrentUser>()?.is_some() {
        return next.call(req).await;
    }

    // Capability-scoped code-session — default-deny except its target's
    // capability-carrier routes.
    if let Some(cs) = session.get::<CodeSessionCookie>(CODE_SESSION_KEY)? {
        let route = classify_code_session_route(req.method(), req.path(), req.query_string());
        let permitted = match route {
            CodeSessionRoute::Denied => false,
            CodeSessionRoute::Scoped { target, capability } => {
                // Target-scope: a code-session may only address its redeemed
                // device, never another.
                if target != cs.target_connection_id {
                    false
                } else {
                    // Capability ceiling: a `Some(false)` dimension is hard-denied
                    // here (the host does not re-apply the per-code ceiling to a
                    // REST-forwarded op). Read the host global once; fail closed if
                    // it is somehow absent.
                    match req.app_data::<web::Data<SharedSettings>>() {
                        Some(settings) => {
                            let global = settings.read().await.security.clone();
                            !capability_hard_denied(&cs.access_ceiling, &global, capability)
                        }
                        None => false,
                    }
                }
            }
        };
        if permitted {
            return next.call(req).await;
        }
        warn!("Code session denied access to a non-scoped or over-ceiling resource.");
        return Err(actix_web::error::ErrorForbidden(
            "Code session is not permitted to access this resource.",
        ));
    }

    warn!("Anonymous user tried to access protected resource.");
    Err(actix_web::error::ErrorUnauthorized(
        "User is not logged in.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ceiling_with(file: Option<bool>, terminal: Option<bool>) -> SecuritySettings {
        SecuritySettings {
            allow_file_browse: file,
            allow_terminal: terminal,
            ..SecuritySettings::all_prompt()
        }
    }

    #[test]
    fn owner_plane_routes_are_denied_for_code_sessions() {
        // A representative sweep of owner-plane and unverifiable routes.
        for path in [
            "/api/desk/settings",
            "/api/desk/security_settings",
            "/api/desk/sysinfo",
            "/api/desk/regenerate_turn_secret",
            "/api/create_token",
            "/api/install_virtual_display_driver",
            "/api/desk/device_codes",
            "/api/desk/connections",
            "/api/turn/info",
            "/api/model/provider",
        ] {
            assert_eq!(
                classify_code_session_route(&Method::GET, path, ""),
                CodeSessionRoute::Denied,
                "expected {path} to be denied"
            );
        }
    }

    #[test]
    fn file_list_is_scoped_to_the_query_target() {
        assert_eq!(
            classify_code_session_route(&Method::GET, "/api/desk/file/list", "connection_id=dev-1"),
            CodeSessionRoute::Scoped {
                target: "dev-1".to_string(),
                capability: ScopedCapability::FileBrowse,
            }
        );
        // No target → denied (cannot verify scope).
        assert_eq!(
            classify_code_session_route(&Method::GET, "/api/desk/file/list", ""),
            CodeSessionRoute::Denied
        );
    }

    #[test]
    fn terminal_open_and_list_are_scoped_to_the_path_target() {
        assert_eq!(
            classify_code_session_route(&Method::GET, "/api/desk/terminal/dev-1", ""),
            CodeSessionRoute::Scoped {
                target: "dev-1".to_string(),
                capability: ScopedCapability::Terminal,
            }
        );
        assert_eq!(
            classify_code_session_route(&Method::GET, "/api/desk/terminals/dev-1", ""),
            CodeSessionRoute::Scoped {
                target: "dev-1".to_string(),
                capability: ScopedCapability::Terminal,
            }
        );
        // Empty or nested trailing segment → denied.
        assert_eq!(
            classify_code_session_route(&Method::GET, "/api/desk/terminal/", ""),
            CodeSessionRoute::Denied
        );
        assert_eq!(
            classify_code_session_route(&Method::GET, "/api/desk/terminal/a/b", ""),
            CodeSessionRoute::Denied
        );
    }

    #[test]
    fn non_get_methods_are_denied() {
        // File delete is a body-addressed mutation — never reachable by a
        // code-session (its target cannot be verified before the body is read).
        assert_eq!(
            classify_code_session_route(&Method::DELETE, "/api/desk/file", ""),
            CodeSessionRoute::Denied
        );
        assert_eq!(
            classify_code_session_route(
                &Method::POST,
                "/api/desk/file/list",
                "connection_id=dev-1"
            ),
            CodeSessionRoute::Denied
        );
    }

    use crate::model::settings::{Args, Settings, SharedSettings};
    use actix_session::{SessionMiddleware, storage::CookieSessionStore};
    use actix_web::middleware::from_fn;
    // NOTE: do not `use actix_web::test` — it also imports the `test` attribute
    // macro, which shadows the built-in `#[test]` used by the sync tests above.
    use actix_web::test as at;
    use actix_web::{App, HttpResponse, cookie::Key};

    fn scope_test_settings() -> SharedSettings {
        // Global leaves every dimension at prompt (`None`), so the ceiling alone
        // governs the hard-deny decision in these tests.
        let mut settings = Settings::default();
        settings.security = SecuritySettings::all_prompt();
        let mut temp_path = std::env::temp_dir();
        temp_path.push(format!("scope_test_{}.toml", uuid::Uuid::new_v4()));
        settings.args = Args {
            config_file_path: temp_path.to_string_lossy().to_string(),
            ..Default::default()
        };
        SharedSettings::from(settings)
    }

    /// Seed a code-session cookie (target `dev-1`, file browse forbidden) and
    /// return its `Cookie` header value, then assert the guard's decision on a set
    /// of routes replayed with that cookie.
    #[actix_web::test]
    async fn enforce_device_scope_gates_code_sessions_end_to_end() {
        async fn seed(session: Session) -> HttpResponse {
            let cookie = CodeSessionCookie {
                code_session_id: "sess-1".to_string(),
                target_connection_id: "dev-1".to_string(),
                // file browse explicitly forbidden; terminal left at prompt.
                access_ceiling: ceiling_with(Some(false), None),
            };
            session.insert(CODE_SESSION_KEY, &cookie).unwrap();
            HttpResponse::Ok().finish()
        }
        async fn ok() -> HttpResponse {
            HttpResponse::Ok().finish()
        }

        let key = Key::generate();
        let app = at::init_service(
            App::new()
                .app_data(web::Data::new(scope_test_settings()))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    key.clone(),
                ))
                .route("/seed", web::post().to(seed))
                .service(
                    actix_web::web::scope("/api")
                        .wrap(from_fn(enforce_device_scope))
                        .route("/desk/settings", web::get().to(ok))
                        .route("/desk/file/list", web::get().to(ok))
                        .route("/desk/terminal/{cid}", web::get().to(ok)),
                ),
        )
        .await;

        // The guard rejects by returning an error (401/403); `try_call_service`
        // surfaces that as `Err`, from which the status is read.
        macro_rules! status_of {
            ($req:expr) => {
                match at::try_call_service(&app, $req).await {
                    Ok(resp) => resp.status(),
                    Err(e) => e.error_response().status(),
                }
            };
        }
        use actix_web::http::StatusCode;

        // Anonymous → 401 on any guarded route.
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/settings")
                    .to_request()
            ),
            StatusCode::UNAUTHORIZED
        );

        // Establish the code-session and capture its cookie.
        let seed_resp =
            at::call_service(&app, at::TestRequest::post().uri("/seed").to_request()).await;
        let cookie = seed_resp
            .response()
            .cookies()
            .next()
            .expect("session cookie set")
            .into_owned();

        // Owner-plane route → 403 (default-deny).
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/settings")
                    .cookie(cookie.clone())
                    .to_request()
            ),
            StatusCode::FORBIDDEN
        );

        // Its own target's terminal (ceiling unset → prompt, passes the gate) → 200.
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/terminal/dev-1")
                    .cookie(cookie.clone())
                    .to_request()
            ),
            StatusCode::OK
        );

        // Another device's terminal → 403 (cross-target).
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/terminal/dev-2")
                    .cookie(cookie.clone())
                    .to_request()
            ),
            StatusCode::FORBIDDEN
        );

        // Its own target's file list, but file browse is forbidden by the ceiling
        // → 403.
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/file/list?connection_id=dev-1")
                    .cookie(cookie.clone())
                    .to_request()
            ),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn capability_hard_denied_only_on_meet_false() {
        // Code explicitly forbids the dimension → denied regardless of global.
        assert!(capability_hard_denied(
            &ceiling_with(Some(false), None),
            &ceiling_with(Some(true), None),
            ScopedCapability::FileBrowse,
        ));
        // Global forbids it (code leaves it unset) → meet is false → denied.
        assert!(capability_hard_denied(
            &ceiling_with(None, None),
            &ceiling_with(Some(false), None),
            ScopedCapability::FileBrowse,
        ));
        // Both allow → not hard-denied.
        assert!(!capability_hard_denied(
            &ceiling_with(Some(true), None),
            &ceiling_with(Some(true), None),
            ScopedCapability::FileBrowse,
        ));
        // Unset on both → prompt (passes through, not hard-denied).
        assert!(!capability_hard_denied(
            &ceiling_with(None, None),
            &ceiling_with(None, None),
            ScopedCapability::FileBrowse,
        ));
        // Terminal dimension is selected independently of the file dimension.
        assert!(capability_hard_denied(
            &ceiling_with(Some(true), Some(false)),
            &ceiling_with(Some(true), Some(true)),
            ScopedCapability::Terminal,
        ));
    }
}
