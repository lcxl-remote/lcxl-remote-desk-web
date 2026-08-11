/// Service management endpoints — available in all modes but only functional
/// when running inside Tauri. After host-control-hub unification,
/// the install / uninstall command travels over the same `/ws/tauri_ipc` link
/// as every other Tauri-bound command, so the endpoint just publishes a hub
/// message and returns 202.
use std::sync::Arc;

use actix_session::Session;
use actix_web::{Error as AWError, HttpResponse, post, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_signal_facade::model::code_session::{CODE_SESSION_KEY, CodeSessionCookie};
use desk_utils::rest::RestResponse;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::{
    daemon::windows_service::default_install_dir,
    error::DeskError,
    host_control::{HostControlHub, HostControlMessage, ServiceOpKind},
    model::settings::SharedSettings,
};
use desk_utils::error::DeskErrorCode;

pub const TAG: &str = "Service";

/// Request body for `POST /api/service/install`.
#[derive(Debug, Default, Deserialize, serde::Serialize, ToSchema)]
pub struct InstallServiceRequest {
    /// Installation directory. Uses the platform default when absent.
    pub install_path: Option<String>,
    /// Whether to also stage the LcxlVirtualDisplay IDD driver during
    /// the install flow. The Tauri shell sets this from the
    /// "install IDD virtual display driver" checkbox on the install
    /// dialog; defaults to `false` for older clients that don't send
    /// the field.
    #[serde(default)]
    pub install_idd_driver: bool,
}

/// Rejects characters in `install_path` that would let an attacker
/// inject additional sidecar CLI flags through the elevated
/// `ShellExecuteW` invocation. The Tauri quote helper offers a second
/// layer of defence (handling backslash + double-quote correctly) but
/// any caller-supplied input should be rejected up front when it
/// contains `"`, `\r`, `\n`, `\0`, or any other ASCII control char —
/// none of these legitimately occur in a Windows directory name.
fn validate_install_path(path: &str) -> Result<(), DeskError> {
    if path.is_empty() {
        return DeskError::custom_error(DeskErrorCode::INVALID_PARAMS, "install_path is empty");
    }
    for ch in path.chars() {
        // Backslash is legal in Windows paths; `"` and any control
        // character are not, and either would let the caller break
        // out of the elevated sidecar's CLI argv layout.
        if ch == '"' || ch.is_control() {
            return DeskError::custom_error(
                DeskErrorCode::INVALID_PARAMS,
                &format!(
                    "install_path contains forbidden character U+{:04X}",
                    ch as u32
                ),
            );
        }
    }
    Ok(())
}

/// Request the host (Tauri) to install the OS system service.
///
/// Stateless: the handler publishes a `ServiceOp` command on the host control
/// hub and returns 202 Accepted immediately. The caller should poll
/// `GET /api/server_info` to check `service_installed`.
#[utoipa::path(
    tag = TAG,
    summary = "Install OS system service",
    request_body = InstallServiceRequest,
    responses(
        (status = 202, description = "Install request accepted"),
        (status = 503, description = "No host control hub or no Tauri shell connected"),
    ),
)]
#[post("/api/service/install")]
pub async fn install_service(
    hub: web::Data<Option<Arc<HostControlHub>>>,
    settings: web::Data<SharedSettings>,
    session: Session,
    req: Option<web::Json<InstallServiceRequest>>,
) -> Result<HttpResponse, AWError> {
    if let Some(response) = authorize_service_management(&settings, &session).await? {
        return Ok(response);
    }
    let body = req.map(|r| r.into_inner()).unwrap_or_default();
    let install_path = body.install_path.unwrap_or_else(default_install_dir);
    validate_install_path(&install_path)?;

    Ok(dispatch_service_op(
        hub.as_ref().as_ref(),
        HostControlMessage::ServiceOp {
            op: ServiceOpKind::Install,
            install_path: Some(install_path),
            install_idd_driver: body.install_idd_driver,
        },
        "Install request accepted",
    )?)
}

/// Request the host (Tauri) to uninstall the OS system service.
#[utoipa::path(
    tag = TAG,
    summary = "Uninstall OS system service",
    responses(
        (status = 202, description = "Uninstall request accepted"),
        (status = 503, description = "No host control hub or no Tauri shell connected"),
    ),
)]
#[post("/api/service/uninstall")]
pub async fn uninstall_service(
    hub: web::Data<Option<Arc<HostControlHub>>>,
    settings: web::Data<SharedSettings>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    if let Some(response) = authorize_service_management(&settings, &session).await? {
        return Ok(response);
    }
    Ok(dispatch_service_op(
        hub.as_ref().as_ref(),
        HostControlMessage::ServiceOp {
            op: ServiceOpKind::Uninstall,
            install_path: None,
            install_idd_driver: false,
        },
        "Uninstall request accepted",
    )?)
}

async fn authorize_service_management(
    settings: &SharedSettings,
    session: &Session,
) -> Result<Option<HttpResponse>, AWError> {
    if settings.read().await.user.login_password.is_empty() {
        return Ok(Some(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::PRECONDITION_FAILED,
            "Initialize the system before managing the service".to_string(),
        ))));
    }
    if session.get_current_user::<CurrentUser>()?.is_some() {
        return Ok(None);
    }
    if session
        .get::<CodeSessionCookie>(CODE_SESSION_KEY)?
        .is_some()
    {
        return Ok(Some(HttpResponse::Forbidden().json(
            RestResponse::<()>::failed(
                DeskErrorCode::PERMISSION_ERROR,
                "Code sessions cannot manage the system service".to_string(),
            ),
        )));
    }
    Ok(Some(HttpResponse::Unauthorized().json(
        RestResponse::<()>::failed(
            DeskErrorCode::PERMISSION_ERROR,
            "Owner session required".to_string(),
        ),
    )))
}

fn dispatch_service_op(
    hub: Option<&Arc<HostControlHub>>,
    msg: HostControlMessage,
    accepted_message: &str,
) -> Result<HttpResponse, DeskError> {
    let Some(hub) = hub else {
        return Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "No host control hub configured".into(),
            )),
        );
    };

    if !hub.has_tauri_ui() {
        return Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "No Tauri shell connected".into(),
            )),
        );
    }

    match hub.send_command(msg) {
        Ok(_) => Ok(
            HttpResponse::Accepted().json(RestResponse::<()>::succeed_with_message(
                accepted_message.into(),
            )),
        ),
        Err(e) => Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                format!("Service op dispatch failed: {e}"),
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{SessionExt as _, SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{App, cookie::Key, dev::Service as _, test};

    fn settings_data(initialized: bool) -> web::Data<SharedSettings> {
        let mut settings = crate::model::settings::Settings::default();
        settings.user.login_password = if initialized {
            "initialized".to_string()
        } else {
            String::new()
        };
        web::Data::new(SharedSettings::from(settings))
    }

    async fn seed_code_session(session: Session) -> HttpResponse {
        session
            .insert(
                CODE_SESSION_KEY,
                CodeSessionCookie {
                    code_session_id: "code".to_string(),
                    grant_session_id: "grant".to_string(),
                    target_connection_id: "target".to_string(),
                },
            )
            .unwrap();
        HttpResponse::Ok().finish()
    }

    fn build_app(
        hub: Option<Arc<HostControlHub>>,
    ) -> App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        App::new()
            .app_data(web::Data::new(hub))
            .app_data(settings_data(true))
            .service(install_service)
            .service(uninstall_service)
            .wrap_fn(|req, service| {
                req.get_session()
                    .set_current_user(&CurrentUser::new_admin("owner"))
                    .expect("seed owner session");
                service.call(req)
            })
            .wrap(SessionMiddleware::new(
                CookieSessionStore::default(),
                Key::generate(),
            ))
    }

    #[actix_web::test]
    async fn service_management_is_preconditioned_then_owner_only() {
        let preinit = test::init_service(
            App::new()
                .app_data(web::Data::new(None::<Arc<HostControlHub>>))
                .app_data(settings_data(false))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(install_service),
        )
        .await;
        let response = test::call_service(
            &preinit,
            test::TestRequest::post()
                .uri("/api/service/install")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), actix_web::http::StatusCode::OK);
        let body: RestResponse<()> = test::read_body_json(response).await;
        assert_eq!(body.code, DeskErrorCode::PRECONDITION_FAILED.code());

        let initialized = test::init_service(
            App::new()
                .app_data(web::Data::new(None::<Arc<HostControlHub>>))
                .app_data(settings_data(true))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(install_service),
        )
        .await;
        let response = test::call_service(
            &initialized,
            test::TestRequest::post()
                .uri("/api/service/install")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn code_session_cannot_manage_service() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(None::<Arc<HostControlHub>>))
                .app_data(settings_data(true))
                .route("/seed-code", web::post().to(seed_code_session))
                .service(uninstall_service)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                )),
        )
        .await;
        let seed = test::call_service(
            &app,
            test::TestRequest::post().uri("/seed-code").to_request(),
        )
        .await;
        let cookie = seed.response().cookies().next().unwrap().into_owned();
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/service/uninstall")
                .cookie(cookie)
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), actix_web::http::StatusCode::FORBIDDEN);
    }

    /// Without a hub configured, both install and uninstall return 503 with a
    /// useful body.
    #[actix_web::test]
    async fn install_without_hub_returns_503() {
        let app = test::init_service(build_app(None)).await;
        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .set_json(InstallServiceRequest::default())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 503);
    }

    /// With a hub but no Tauri subscriber, dispatch still rejects with 503 so
    /// the caller is not led to believe the install was queued.
    #[actix_web::test]
    async fn install_without_tauri_subscriber_returns_503() {
        let hub = Arc::new(HostControlHub::new_local());
        let app = test::init_service(build_app(Some(hub))).await;
        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .set_json(InstallServiceRequest::default())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 503);
    }

    /// Once a Tauri subscriber is registered the dispatch succeeds (202) and
    /// the broadcast carries a `ServiceOp` message with the IDD flag the
    /// caller supplied.
    #[actix_web::test]
    async fn install_with_tauri_subscriber_returns_202_and_threads_idd_flag() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let app = test::init_service(build_app(Some(Arc::clone(&hub)))).await;

        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .set_json(InstallServiceRequest {
                install_path: Some("C:/foo".to_string()),
                install_idd_driver: true,
            })
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202);

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        match msg {
            HostControlMessage::ServiceOp {
                op,
                install_path,
                install_idd_driver,
            } => {
                assert!(matches!(op, ServiceOpKind::Install));
                assert_eq!(install_path.as_deref(), Some("C:/foo"));
                assert!(install_idd_driver, "must thread caller-supplied flag");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Missing body field defaults to `install_idd_driver: false` —
    /// the install dialog must explicitly opt in.
    #[actix_web::test]
    async fn install_without_idd_field_defaults_false() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let app = test::init_service(build_app(Some(Arc::clone(&hub)))).await;

        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"install_path":"C:/foo"}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202);

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        match msg {
            HostControlMessage::ServiceOp {
                install_idd_driver, ..
            } => assert!(!install_idd_driver),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// install_path containing `"` would let the caller smuggle a
    /// second CLI flag through the ShellExecuteW invocation; we reject
    /// it at the REST boundary with HTTP 200 + body INVALID_PARAMS (=5)
    /// per the project convention that HTTP status stays 200 and the
    /// business code travels in the response body.
    #[actix_web::test]
    async fn install_with_double_quote_in_path_rejected_with_invalid_params() {
        let hub = Arc::new(HostControlHub::new_local());
        hub.mark_tauri_connected();
        let app = test::init_service(build_app(Some(hub))).await;
        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"install_path":"C:\\x\" --uninstall-service"}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"].as_i64(), Some(5));
    }

    /// install_path containing a newline would split the elevated CLI
    /// across argv; reject the same way as `"`.
    #[actix_web::test]
    async fn install_with_newline_in_path_rejected_with_invalid_params() {
        let hub = Arc::new(HostControlHub::new_local());
        hub.mark_tauri_connected();
        let app = test::init_service(build_app(Some(hub))).await;
        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .insert_header(("content-type", "application/json"))
            .set_payload("{\"install_path\":\"C:\\\\x\\n--evil\"}")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"].as_i64(), Some(5));
    }

    #[actix_web::test]
    async fn uninstall_with_tauri_subscriber_returns_202() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let app = test::init_service(build_app(Some(Arc::clone(&hub)))).await;

        let req = test::TestRequest::post()
            .uri("/api/service/uninstall")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202);

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        assert!(matches!(
            msg,
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Uninstall,
                install_path: None,
                install_idd_driver: false,
            }
        ));
    }

    // Plain sync tests are wrapped in a child module so the outer
    // `use actix_web::{App, test};` cannot shadow Rust's built-in
    // `#[test]` attribute.
    mod validate_install_path_tests {
        use super::super::validate_install_path;

        #[test]
        fn accepts_typical_windows_paths() {
            validate_install_path("C:\\Program Files\\LCXL Remote Desktop").unwrap();
            validate_install_path("D:\\foo\\bar").unwrap();
            validate_install_path("/opt/lcxl-remote-desk").unwrap();
        }

        #[test]
        fn rejects_quote_control_chars_and_empty() {
            assert!(validate_install_path("").is_err());
            assert!(validate_install_path("C:\\x\"--evil").is_err());
            assert!(validate_install_path("C:\\x\n--evil").is_err());
            assert!(validate_install_path("C:\\x\r--evil").is_err());
            assert!(validate_install_path("C:\\x\0--evil").is_err());
            assert!(validate_install_path("C:\\x\t--evil").is_err());
        }
    }
}
