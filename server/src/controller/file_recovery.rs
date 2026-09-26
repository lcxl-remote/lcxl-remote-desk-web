//! Local OS owner recovery, including backups from a replaced upstream credential.
//! These routes are deliberately unavailable through manager/remote proxies.
use super::host_readiness::validate_local_mutation;
use crate::{error::DeskError, file_recovery_service::local, model::settings::SharedSettings};
use actix_web::{HttpRequest, HttpResponse, http::header, post, web};
pub use desk_agent_protocol::file_recovery::{
    FileRecoveryCleanupDto, FileRecoveryPageDto, FileRecoveryPolicyDto, FileRecoveryRecordDto,
};
use desk_ipc_protocol::local_file_recovery::{
    LocalFileRecoveryCommand as Command, LocalFileRecoveryOutcome as Outcome,
};
use desk_utils::rest::RestResponse;
use serde::Deserialize;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryPageBody {
    pub after: Option<String>,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryExportBody {
    pub recovery_id: String,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryDiscardBody {
    pub recovery_id: String,
    pub conversation_id: String,
    pub confirmed: bool,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryClockBody {
    pub displayed_time_unix_ms: u64,
    pub confirmed: bool,
}
async fn local_operation<T: Send + 'static>(
    _req: &HttpRequest,
    settings: &SharedSettings,
    command: Command,
    project: fn(Outcome) -> Result<T, &'static str>,
) -> Result<T, DeskError> {
    #[cfg(windows)]
    let local_user = crate::windows_local_user::authenticate(_req).map_err(|error| {
        DeskError::FileRecoveryFailure(crate::file_recovery_service::storage_failure(error))
    })?;
    #[cfg(windows)]
    if let Some(manager) = _req
        .app_data::<web::Data<crate::model::settings_coordinator::SettingsCoordinator>>()
        .and_then(|coordinator| coordinator.worker_manager())
        .filter(|manager| manager.uses_session_targeting())
    {
        let outcome = manager
            .request_local_file_recovery(&local_user, command)
            .await
            .map_err(DeskError::FileRecoveryFailure)?;
        return project(outcome).map_err(|_| {
            DeskError::FileRecoveryFailure(
                desk_agent_protocol::file_recovery::FileRecoveryFailure::StorageUnavailable,
            )
        });
    }
    let root = settings.read().await.paths().data_root().to_path_buf();
    Ok(tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        if !local_user.is_alive()
            || crate::file_recovery_service::platform_user::current()? != local_user.sid
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Local backup access requires the same OS user",
            ));
        }
        local::execute(
            &root,
            || desk_file_recovery::quota::DeviceQuota::open(&root)?.lock(),
            command,
        )
        .and_then(|outcome| {
            project(outcome)
                .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))
        })
    })
    .await?
    .map_err(|error| {
        DeskError::FileRecoveryFailure(crate::file_recovery_service::storage_failure(error))
    })?)
}
#[utoipa::path(tag = "FileRecovery", summary = "List local OS user file backups",
    request_body = FileRecoveryPageBody,
    responses((status = 200, body = RestResponse<FileRecoveryPageDto>)))]
#[post("/file-recovery/query")]
pub async fn query_local_file_recovery(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
    body: web::Json<FileRecoveryPageBody>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let body = body.into_inner();
    let page = local_operation(
        &req,
        &settings,
        Command::Query { after: body.after },
        Outcome::into_page,
    )
    .await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(page)))
}
#[utoipa::path(tag = "FileRecovery", summary = "Update local file backup retention and capacity",
    request_body = FileRecoveryPolicyDto,
    responses((status = 200, body = RestResponse<FileRecoveryPolicyDto>)))]
#[post("/file-recovery/policy")]
pub async fn update_local_file_recovery_policy(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
    body: web::Json<FileRecoveryPolicyDto>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let body = body.into_inner();
    let root = settings.read().await.paths().data_root().to_path_buf();
    let value = tokio::task::spawn_blocking(move || -> std::io::Result<FileRecoveryPolicyDto> {
        let mut quota = desk_file_recovery::quota::DeviceQuota::open(&root)?.lock()?;
        quota.set_policy(desk_file_recovery::Policy {
            retention_days: body.retention_days,
            max_bytes: body.max_bytes,
        })?;
        let policy = quota.policy();
        Ok(FileRecoveryPolicyDto {
            retention_days: policy.retention_days,
            max_bytes: policy.max_bytes,
        })
    })
    .await?
    .map_err(|error| {
        DeskError::FileRecoveryFailure(crate::file_recovery_service::storage_failure(error))
    })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(value)))
}
#[utoipa::path(tag = "FileRecovery", summary = "Retry local file backup cleanup",
    responses((status = 200, body = RestResponse<FileRecoveryCleanupDto>)))]
#[post("/file-recovery/cleanup")]
pub async fn retry_local_file_recovery_cleanup(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let report = local_operation(
        &req,
        &settings,
        Command::RetryCleanup,
        Outcome::into_cleanup,
    )
    .await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(report)))
}
#[utoipa::path(tag = "FileRecovery", summary = "Discard a confirmed local OS user backup",
    request_body = FileRecoveryDiscardBody,
    responses((status = 200, body = RestResponse<FileRecoveryCleanupDto>)))]
#[post("/file-recovery/discard")]
pub async fn discard_local_file_recovery(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
    body: web::Json<FileRecoveryDiscardBody>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let body = body.into_inner();
    let report = local_operation(
        &req,
        &settings,
        Command::Discard {
            recovery_id: body.recovery_id,
            conversation_id: body.conversation_id,
            confirmed: body.confirmed,
        },
        Outcome::into_cleanup,
    )
    .await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(report)))
}

#[utoipa::path(tag = "FileRecovery", summary = "Confirm corrected device time for backup cleanup",
    request_body = FileRecoveryClockBody,
    responses((status = 200, body = RestResponse<FileRecoveryCleanupDto>)))]
#[post("/file-recovery/clock")]
pub async fn confirm_local_file_recovery_clock(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
    body: web::Json<FileRecoveryClockBody>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let body = body.into_inner();
    let report = local_operation(
        &req,
        &settings,
        Command::ConfirmClock {
            displayed_time_unix_ms: body.displayed_time_unix_ms,
            confirmed: body.confirmed,
        },
        Outcome::into_cleanup,
    )
    .await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(report)))
}

#[utoipa::path(tag = "FileRecovery", summary = "Export a local OS user file recovery package",
    request_body = FileRecoveryExportBody,
    responses((status = 200, description = "ZIP with before.txt and metadata.json", content_type = "application/zip", body = Vec<u8>)))]
#[post("/file-recovery/export")]
pub async fn export_local_file_recovery(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
    body: web::Json<FileRecoveryExportBody>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let body = body.into_inner();
    let bytes = local_operation(
        &req,
        &settings,
        Command::Export {
            recovery_id: body.recovery_id,
        },
        Outcome::into_export,
    )
    .await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .insert_header((header::CONTENT_TYPE, "application/zip"))
        .insert_header((
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"file-recovery.zip\"",
        ))
        .insert_header((
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        ))
        .body(bytes))
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use actix_session::{Session, SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{App, cookie::Key, middleware::from_fn, test};
    use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
    #[actix_web::test]
    async fn local_file_recovery_errors_are_typed_and_do_not_expose_internal_paths() {
        use actix_web::ResponseError;
        for (kind, expected) in [
            (std::io::ErrorKind::WouldBlock, "busy"),
            (std::io::ErrorKind::NotFound, "material_unavailable"),
            (std::io::ErrorKind::InvalidInput, "invalid_request"),
            (std::io::ErrorKind::PermissionDenied, "storage_unavailable"),
        ] {
            let error =
                DeskError::FileRecoveryFailure(crate::file_recovery_service::storage_failure(
                    std::io::Error::new(kind, "private filesystem path and trace"),
                ));
            let response = error.error_response();
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-store"
            );
            let bytes = actix_web::body::to_bytes(response.into_body())
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["success"], false);
            assert_eq!(body["data"], expected);
            assert!(!String::from_utf8_lossy(&bytes).contains("private filesystem"));
        }
    }
    async fn login(session: Session) -> HttpResponse {
        session
            .set_current_user(&CurrentUser::new_admin("owner"))
            .unwrap();
        HttpResponse::Ok().finish()
    }
    #[actix_web::test]
    async fn local_recovery_requires_owner_and_same_origin_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let shared = web::Data::new(SharedSettings::from(Settings::for_test_config(
            &dir.path().join("config"),
        )));
        let app = test::init_service(
            App::new()
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .app_data(shared.clone())
                .route("/login", web::post().to(login))
                .service(
                    web::scope("/api/desk")
                        .wrap(from_fn(crate::controller::user::enforce_device_scope))
                        .service(query_local_file_recovery)
                        .service(update_local_file_recovery_policy)
                        .service(retry_local_file_recovery_cleanup)
                        .service(discard_local_file_recovery)
                        .service(confirm_local_file_recovery_clock)
                        .service(export_local_file_recovery),
                ),
        )
        .await;
        let logged_in =
            test::call_service(&app, test::TestRequest::post().uri("/login").to_request()).await;
        let cookie = logged_in.response().cookies().next().unwrap().into_owned();
        for (path, body) in [
            ("query", serde_json::json!({})),
            (
                "policy",
                serde_json::json!({"retention_days": 7, "max_bytes": 104857600}),
            ),
            ("cleanup", serde_json::json!({})),
            (
                "clock",
                serde_json::json!({"displayed_time_unix_ms": 1000, "confirmed": true}),
            ),
            (
                "discard",
                serde_json::json!({"recovery_id": "a".repeat(64), "conversation_id": "conversation", "confirmed": true}),
            ),
            ("export", serde_json::json!({"recovery_id": "a".repeat(64)})),
        ] {
            let uri = format!("/api/desk/file-recovery/{path}");
            let denied = test::try_call_service(
                &app,
                test::TestRequest::post()
                    .uri(&uri)
                    .peer_addr("127.0.0.1:1234".parse().unwrap())
                    .insert_header((header::HOST, "localhost"))
                    .insert_header((header::ORIGIN, "http://localhost"))
                    .set_json(&body)
                    .to_request(),
            )
            .await
            .expect_err("owner login is required");
            assert_eq!(
                denied.error_response().status(),
                actix_web::http::StatusCode::UNAUTHORIZED
            );
            for (peer, origin) in [
                ("192.0.2.1:1234", "http://localhost"),
                ("127.0.0.1:1234", "http://other.example"),
            ] {
                let response = test::call_service(
                    &app,
                    test::TestRequest::post()
                        .uri(&uri)
                        .cookie(cookie.clone())
                        .peer_addr(peer.parse().unwrap())
                        .insert_header((header::HOST, "localhost"))
                        .insert_header((header::ORIGIN, origin))
                        .set_json(&body)
                        .to_request(),
                )
                .await;
                let response: serde_json::Value = test::read_body_json(response).await;
                assert_ne!(
                    response["code"],
                    serde_json::json!(desk_utils::error::DeskErrorCode::SUCCESS)
                );
            }
        }
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/desk/file-recovery/query")
                .cookie(cookie.clone())
                .peer_addr("127.0.0.1:1234".parse().unwrap())
                .insert_header((header::HOST, "localhost"))
                .insert_header((header::ORIGIN, "http://localhost"))
                .set_json(serde_json::json!({}))
                .to_request(),
        )
        .await;
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let response: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(
            response["code"],
            serde_json::json!(desk_utils::error::DeskErrorCode::SUCCESS)
        );
        assert_eq!(response["data"]["policy"]["retention_days"], 7);
        assert_eq!(response["data"]["records"], serde_json::json!([]));
        let saved = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/desk/file-recovery/policy")
                .cookie(cookie.clone())
                .peer_addr("127.0.0.1:1234".parse().unwrap())
                .insert_header((header::HOST, "localhost"))
                .insert_header((header::ORIGIN, "http://localhost"))
                .set_json(serde_json::json!({"retention_days": 12, "max_bytes": 1048576}))
                .to_request(),
        )
        .await;
        let saved: serde_json::Value = test::read_body_json(saved).await;
        assert_eq!(saved["data"]["retention_days"], 12);
        let root = shared.read().await.paths().data_root().to_path_buf();
        let mut quota = desk_file_recovery::quota::DeviceQuota::open(&root)
            .unwrap()
            .lock()
            .unwrap();
        assert_eq!(quota.policy().retention_days, 12);
        let scope = desk_file_recovery::Scope {
            authority: "other-authority".into(),
            device: "device".into(),
            os_user: "other-user".into(),
            owner: "owner".into(),
        };
        let key = desk_file_recovery::quota::QuotaKey::new(
            &scope,
            "other-conversation",
            "operation",
            "generation",
        )
        .unwrap();
        quota.reserve(key, 1200, 100, 1).unwrap();
        let used = quota.used_bytes();
        drop(quota);
        let queried = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/desk/file-recovery/query")
                .cookie(cookie)
                .peer_addr("127.0.0.1:1234".parse().unwrap())
                .insert_header((header::HOST, "localhost"))
                .insert_header((header::ORIGIN, "http://localhost"))
                .set_json(serde_json::json!({}))
                .to_request(),
        )
        .await;
        let queried: serde_json::Value = test::read_body_json(queried).await;
        assert_eq!(queried["data"]["policy"]["retention_days"], 12);
        assert_eq!(queried["data"]["used_bytes"], used);
        assert_eq!(queried["data"]["records"], serde_json::json!([]));
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "file_recovery/export_tests.rs"]
mod export_tests;
