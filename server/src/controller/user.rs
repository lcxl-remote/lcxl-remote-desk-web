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
use desk_signal_facade::grant::{AccessGrantStore, GrantPrincipal};
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

/// A capability dimension a code-session may exercise over REST. Only file
/// browsing is exposed today; terminal open/list is deliberately not (its handler
/// still requires an owner `CurrentUser` and injects no code-session ceiling), so
/// it stays denied until that handler is taught the code-session identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopedCapability {
    FileBrowse,
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
/// route — whose target is addressable from the query (never a request body) — is
/// reachable, and only as `Scoped` so the caller still enforces `target ==
/// redeemed target`, grant freshness, and the code's ceiling. Everything else
/// (settings, system info, service management, api tokens, turn keys, device-code
/// CRUD, virtual-display, file delete, connection listing, terminal open/list) is
/// `Denied`. Pure over its inputs so the allowlist is unit-testable.
///
/// Terminal open/list is intentionally excluded: `open_terminal_session` still
/// requires an owner `CurrentUser` and builds an anonymous, ceiling-less terminal
/// context, so admitting a code-session there would 401 at best and bypass the
/// ceiling at worst. It stays denied until that handler injects the code-session
/// ceiling.
fn classify_code_session_route(method: &Method, path: &str, query: &str) -> CodeSessionRoute {
    if method != Method::GET {
        return CodeSessionRoute::Denied;
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

/// Whether the code's ceiling, met with the host global, **explicitly allows**
/// `capability` (`meet == Some(true)`). A REST request cannot prompt the host user
/// inline, and the host does not re-apply the per-code ceiling to a REST-forwarded
/// op (it sees `from_connection_id == None` and uses its global settings), so the
/// three-state `None` ("prompt") is fail-closed here exactly like `Some(false)`:
/// only a two-sided explicit allow lets a code-session's REST capability through.
fn capability_permitted(
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
    };
    eff == Some(true)
}

/// Whether a code-session may perform a scoped REST request. It must address its
/// own redeemed target; its grant must still be live (TTL / not revoked) and bound
/// to this principal in the shared grant store — binding REST access to the same
/// freshness as signaling; and the grant's live ceiling met with the host global
/// must explicitly allow the capability.
async fn code_session_scoped_permitted(
    cs: &CodeSessionCookie,
    target: &str,
    capability: ScopedCapability,
    req: &ServiceRequest,
) -> bool {
    // Target-scope: only the redeemed device, never another.
    if target != cs.target_connection_id {
        return false;
    }
    // Grant freshness + principal binding. The authoritative ceiling is the live
    // grant record's, not a cookie-cached copy.
    let store = desk_signal::access_grant::global_access_grant_store();
    let ceiling = match store.lookup(&cs.grant_session_id).await {
        Ok(Some(record))
            if record.principal == GrantPrincipal::from_code_session(&cs.code_session_id) =>
        {
            match record.access_ceiling {
                Some(ceiling) => ceiling,
                // A code grant always carries a ceiling; a missing one is a
                // malformed record — fail closed.
                None => return false,
            }
        }
        _ => return false,
    };
    // Capability: only a two-sided explicit allow passes on REST.
    match req.app_data::<web::Data<SharedSettings>>() {
        Some(settings) => {
            let global = settings.read().await.security.clone();
            capability_permitted(&ceiling, &global, capability)
        }
        None => false,
    }
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
                code_session_scoped_permitted(&cs, &target, capability, &req).await
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
    use desk_signal_facade::grant::GrantSessionRecord;

    fn file_ceiling(file: Option<bool>) -> SecuritySettings {
        SecuritySettings {
            allow_file_browse: file,
            ..SecuritySettings::all_prompt()
        }
    }

    #[test]
    fn owner_plane_and_terminal_routes_are_denied_for_code_sessions() {
        // A representative sweep of owner-plane and unverifiable routes, plus the
        // terminal routes (not yet code-session capable).
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
            "/api/desk/terminal/dev-1",
            "/api/desk/terminals/dev-1",
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

    #[test]
    fn capability_permitted_requires_a_two_sided_explicit_allow() {
        // Only `Some(true)` on both code and global permits on REST.
        assert!(capability_permitted(
            &file_ceiling(Some(true)),
            &file_ceiling(Some(true)),
            ScopedCapability::FileBrowse,
        ));
        // Code forbids → denied.
        assert!(!capability_permitted(
            &file_ceiling(Some(false)),
            &file_ceiling(Some(true)),
            ScopedCapability::FileBrowse,
        ));
        // Global forbids → denied.
        assert!(!capability_permitted(
            &file_ceiling(Some(true)),
            &file_ceiling(Some(false)),
            ScopedCapability::FileBrowse,
        ));
        // Code unset (prompt) → fail-closed on REST (cannot prompt inline).
        assert!(!capability_permitted(
            &file_ceiling(None),
            &file_ceiling(Some(true)),
            ScopedCapability::FileBrowse,
        ));
        // Global unset (prompt) → fail-closed.
        assert!(!capability_permitted(
            &file_ceiling(Some(true)),
            &file_ceiling(None),
            ScopedCapability::FileBrowse,
        ));
    }

    use crate::model::settings::{Args, Settings, SharedSettings};
    use actix_session::{SessionMiddleware, storage::CookieSessionStore};
    use actix_web::middleware::from_fn;
    // NOTE: do not `use actix_web::test` — it also imports the `test` attribute
    // macro, which shadows the built-in `#[test]` used by the sync tests above.
    use actix_web::test as at;
    use actix_web::{App, HttpResponse, cookie::Key};

    /// Settings whose global file-browse is explicitly allowed, so the code
    /// ceiling alone governs the two-sided meet in the end-to-end test.
    fn scope_test_settings() -> SharedSettings {
        let mut settings = Settings::default();
        settings.security = file_ceiling(Some(true));
        let mut temp_path = std::env::temp_dir();
        temp_path.push(format!("scope_test_{}.toml", uuid::Uuid::new_v4()));
        settings.args = Args {
            config_file_path: temp_path.to_string_lossy().to_string(),
            ..Default::default()
        };
        SharedSettings::from(settings)
    }

    /// End-to-end: a code-session with a live grant (file browse allowed) reaches
    /// only its own target's file list, and loses all REST access once its grant is
    /// revoked — binding the REST plane to grant freshness.
    #[actix_web::test]
    async fn enforce_device_scope_gates_code_sessions_end_to_end() {
        // Mint the grant the seeded cookie will reference, in the same process-
        // global store the guard consults.
        let store = desk_signal::access_grant::global_access_grant_store();
        let record = GrantSessionRecord {
            principal: GrantPrincipal::from_code_session("sess-e2e"),
            target_device: "dev-1-client".to_string(),
            access_ceiling: Some(file_ceiling(Some(true))),
            generation: 0,
        };
        let grant_session_id = store.mint(&record, 300).await.unwrap().grant_session_id;

        let seed_gsid = grant_session_id.clone();
        let seed = move |session: Session| {
            let gsid = seed_gsid.clone();
            async move {
                let cookie = CodeSessionCookie {
                    code_session_id: "sess-e2e".to_string(),
                    grant_session_id: gsid,
                    target_connection_id: "dev-1".to_string(),
                };
                session.insert(CODE_SESSION_KEY, &cookie).unwrap();
                HttpResponse::Ok().finish()
            }
        };
        async fn ok() -> HttpResponse {
            HttpResponse::Ok().finish()
        }

        let app = at::init_service(
            App::new()
                .app_data(web::Data::new(scope_test_settings()))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
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

        // Terminal is not code-session capable → 403 even for its own target.
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/terminal/dev-1")
                    .cookie(cookie.clone())
                    .to_request()
            ),
            StatusCode::FORBIDDEN
        );

        // Another device's file list → 403 (cross-target).
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/file/list?connection_id=dev-2")
                    .cookie(cookie.clone())
                    .to_request()
            ),
            StatusCode::FORBIDDEN
        );

        // Its own target's file list, ceiling + global both allow, grant live → 200.
        assert_eq!(
            status_of!(
                at::TestRequest::get()
                    .uri("/api/desk/file/list?connection_id=dev-1")
                    .cookie(cookie.clone())
                    .to_request()
            ),
            StatusCode::OK
        );

        // Revoke the grant → the same cookie loses all scoped REST access (grant
        // freshness binding).
        store.revoke(&grant_session_id).await.unwrap();
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
}
