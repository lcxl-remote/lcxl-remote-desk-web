//! Local OS owner recovery, including backups from a replaced upstream credential.
//! These routes are deliberately unavailable through manager/remote proxies.
use super::host_readiness::validate_local_mutation;
use crate::{error::DeskError, model::settings::SharedSettings};
use actix_web::{HttpRequest, HttpResponse, http::header, post, web};
pub use desk_agent_protocol::file_recovery::{
    FileRecoveryCleanupDto, FileRecoveryPageDto, FileRecoveryPolicyDto, FileRecoveryRecordDto,
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
fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(1) as u64
}
async fn local_operation<T: Send + 'static>(
    settings: &SharedSettings,
    operation: impl FnOnce(
        &mut desk_file_recovery::LockedVault,
        &mut desk_file_recovery::quota::LockedDeviceQuota,
    ) -> std::io::Result<T>
    + Send
    + 'static,
) -> Result<T, DeskError> {
    let root = settings.read().await.paths().data_root().to_path_buf();
    Ok(tokio::task::spawn_blocking(move || {
        let vault = desk_file_recovery::Vault::open(&root)?;
        let mut locked = vault.try_lock()?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "File backup storage is busy; retry after the active operation finishes",
            )
        })?;
        let mut quota = desk_file_recovery::quota::DeviceQuota::open(&root)?.lock()?;
        locked.observe_system_clock()?;
        let policy = quota.policy();
        if locked.policy() != &policy {
            locked.set_policy(policy)?;
        }
        #[cfg(unix)]
        locked.maintain_epoch_indexes(&unsafe { libc::geteuid() }.to_string(), &mut quota, 64)?;
        operation(&mut locked, &mut quota)
    })
    .await?
    .map_err(|error| {
        DeskError::FileRecoveryFailure(crate::file_recovery_service::storage_failure(error))
    })?)
}
fn policy(vault: &desk_file_recovery::LockedVault) -> FileRecoveryPolicyDto {
    FileRecoveryPolicyDto {
        retention_days: vault.policy().retention_days,
        max_bytes: vault.policy().max_bytes,
    }
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
    let page = local_operation(&settings, move |vault, quota| {
        let mut records = vault.local_records(body.after.as_deref())?;
        let more = records.len() > 100;
        records.truncate(100);
        let next_cursor = more.then(|| records.last().unwrap().id.clone());
        Ok(FileRecoveryPageDto {
            execution_epoch: vault.execution_epoch(),
            cleanup_warning: vault
                .cleanup_clock_paused()
                .then_some(desk_agent_protocol::file_recovery::FileRecoveryFailure::ClockChanged),
            clock_confirmation_time_unix_ms: vault.cleanup_clock_paused().then(now_ms),
            oldest_pending_at_unix_ms: vault.oldest_pending_created_at(None),
            oldest_pending_record: vault.oldest_pending_record(None, None).map(|record| {
                crate::file_recovery_service::project_record(vault, record, now_ms())
            }),
            policy: policy(vault),
            used_bytes: quota.used_bytes(),
            reserved_bytes: quota.reserved_bytes(),
            next_cursor,
            records: records
                .into_iter()
                .map(|r| crate::file_recovery_service::project_record(vault, r, now_ms()))
                .collect(),
        })
    })
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
    let report = local_operation(&settings, |vault, quota| {
        let unknown = vault.recover_interrupted(now_ms())?;
        for record in vault.pending_quota_settlement().into_iter().take(32) {
            let mut key = desk_file_recovery::quota::QuotaKey::new(
                &record.scope,
                &record.conversation,
                &record.operation,
                &record.generation,
            )?;
            key.epoch = record.storage_epoch;
            quota.settle(&key, record.bytes)?;
            vault.acknowledge_quota_settlement(&record.scope, &record.id)?;
        }
        for record in vault.pending_quota_cleanup().into_iter().take(32) {
            let mut key = desk_file_recovery::quota::QuotaKey::new(
                &record.scope,
                &record.conversation,
                &record.operation,
                &record.generation,
            )?;
            key.epoch = record.storage_epoch;
            quota.release_with_retained_index(
                &key,
                desk_file_recovery::LockedVault::retained_index_bytes(&record)?,
            )?;
            vault.acknowledge_quota_cleanup(&record.scope, &record.id)?;
        }
        for namespace in vault.pending_quota_namespaces().into_iter().take(32) {
            #[cfg(unix)]
            quota.release_namespace_at_epoch(
                &namespace,
                &unsafe { libc::geteuid() }.to_string(),
                vault.namespace_epoch(&namespace)?,
            )?;
            #[cfg(not(unix))]
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "local recovery is unavailable",
            ));
            vault.acknowledge_quota_namespace(&namespace)?;
        }
        #[cfg(unix)]
        vault.maintain_epoch_indexes(&unsafe { libc::geteuid() }.to_string(), quota, 1)?;
        Ok(FileRecoveryCleanupDto {
            pending_files: vault.pending_cleanup_count(),
            unknown_outcomes: unknown.len() as u64,
        })
    })
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
    let report = local_operation(&settings, move |vault, quota| {
        if !body.confirmed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Backup discard requires confirmation",
            ));
        }
        vault.discard_local(&body.conversation_id, &body.recovery_id, now_ms())?;
        for record in vault
            .pending_quota_cleanup()
            .into_iter()
            .filter(|record| record.id == body.recovery_id)
        {
            let mut key = desk_file_recovery::quota::QuotaKey::new(
                &record.scope,
                &record.conversation,
                &record.operation,
                &record.generation,
            )?;
            key.epoch = record.storage_epoch;
            quota.release_with_retained_index(
                &key,
                desk_file_recovery::LockedVault::retained_index_bytes(&record)?,
            )?;
            vault.acknowledge_quota_cleanup(&record.scope, &record.id)?;
        }
        Ok(FileRecoveryCleanupDto {
            pending_files: vault.pending_cleanup_count(),
            unknown_outcomes: vault.unknown_outcome_count(None),
        })
    })
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
    let report = local_operation(&settings, move |vault, _| {
        if !body.confirmed || body.displayed_time_unix_ms == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Clock confirmation is required",
            ));
        }
        vault.acknowledge_clock(body.displayed_time_unix_ms)?;
        Ok(FileRecoveryCleanupDto {
            pending_files: vault.pending_cleanup_count(),
            unknown_outcomes: vault.unknown_outcome_count(None),
        })
    })
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
    let bytes = local_operation(&settings, move |vault, _quota| {
        vault.export_local_package(&body.recovery_id, now_ms())
    })
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

#[cfg(all(test, target_os = "macos"))]
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
