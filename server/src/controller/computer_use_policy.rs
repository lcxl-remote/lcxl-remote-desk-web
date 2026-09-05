//! Owner-authenticated, same-origin loopback access only. No manager proxy.

use actix_web::{HttpRequest, HttpResponse, http::header, post, web};
use desk_utils::rest::RestResponse;

use super::host_readiness::validate_local_mutation;
use crate::{
    error::DeskError,
    model::{
        settings::{
            ComputerUseApplicationPolicy, ComputerUseApplicationPolicyUpdate, SharedSettings,
        },
        settings_coordinator::SettingsCoordinator,
    },
};

#[utoipa::path(
    tag = "ComputerUsePolicy",
    summary = "Read the local Computer Use application restriction",
    responses((status = 200, body = RestResponse<ComputerUseApplicationPolicy>)),
)]
#[post("/settings/computer-use-applications/query")]
pub async fn query_computer_use_application_policy(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    // POST intentionally requires the same browser Origin boundary as writes.
    validate_local_mutation(&req)?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(
            settings.read().await.computer_use.application_policy(),
        )))
}

#[utoipa::path(
    tag = "ComputerUsePolicy",
    summary = "Update the local Computer Use application restriction",
    request_body = ComputerUseApplicationPolicyUpdate,
    responses((status = 200, body = RestResponse<ComputerUseApplicationPolicy>)),
)]
#[post("/settings/computer-use-applications")]
pub async fn update_computer_use_application_policy(
    req: HttpRequest,
    request: web::Json<ComputerUseApplicationPolicyUpdate>,
    coordinator: web::Data<SettingsCoordinator>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let update = request.into_inner();
    // Once accepted, finish persistence and worker convergence even if the
    // browser disconnects. Never leave a durable update half-published.
    let applied = actix_web::rt::spawn(async move {
        let mut applied = None;
        coordinator
            .commit(|settings| {
                settings.computer_use.update_application_policy(update)?;
                applied = Some(settings.computer_use.application_policy());
                Ok(())
            })
            .await?;
        Ok::<_, DeskError>(applied.expect("a successful commit applied the policy"))
    })
    .await??;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(applied)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use actix_session::{Session, SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{App, cookie::Key, middleware::from_fn, test};
    use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
    use std::sync::Arc;

    async fn login(session: Session) -> HttpResponse {
        session
            .set_current_user(&CurrentUser::new_admin("owner"))
            .unwrap();
        HttpResponse::Ok().finish()
    }

    #[actix_web::test]
    async fn application_policy_requires_owner_loopback_and_origin_and_uses_cas() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::for_test_config(&dir.path().join("config"));
        let security = settings.security.clone();
        let shared = Arc::new(SharedSettings::from(settings));
        let coordinator = SettingsCoordinator::new(shared.clone(), security);
        let app = test::init_service(
            App::new()
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .app_data(web::Data::from(shared.clone()))
                .app_data(web::Data::new(coordinator))
                .route("/login", web::post().to(login))
                .service(
                    web::scope("/api/desk")
                        .wrap(from_fn(crate::controller::user::enforce_device_scope))
                        .service(query_computer_use_application_policy)
                        .service(update_computer_use_application_policy),
                ),
        )
        .await;
        let response =
            test::call_service(&app, test::TestRequest::post().uri("/login").to_request()).await;
        let cookie = response.response().cookies().next().unwrap().into_owned();
        let path = "/api/desk/settings/computer-use-applications";
        let update = ComputerUseApplicationPolicyUpdate {
            expected_revision: 0,
            allowed_application_paths: vec![],
        };
        let anonymous = test::try_call_service(
            &app,
            test::TestRequest::post()
                .uri(path)
                .set_json(&update)
                .to_request(),
        )
        .await
        .expect_err("anonymous request must be rejected by the owner scope");
        assert_eq!(
            anonymous.error_response().status(),
            actix_web::http::StatusCode::UNAUTHORIZED
        );
        for (peer, origin) in [
            ("192.0.2.1:1234", "http://localhost"),
            ("127.0.0.1:1234", "http://evil.example"),
            ("127.0.0.1:1234", "null"),
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(path)
                    .cookie(cookie.clone())
                    .peer_addr(peer.parse().unwrap())
                    .insert_header((header::HOST, "localhost"))
                    .insert_header((header::ORIGIN, origin))
                    .set_json(&update)
                    .to_request(),
            )
            .await;
            let body: serde_json::Value = test::read_body_json(response).await;
            assert_ne!(
                body["code"],
                serde_json::json!(desk_utils::error::DeskErrorCode::SUCCESS)
            );
            assert_eq!(shared.read().await.computer_use.revision, 0);
        }
        for expected_revision in [0, 0] {
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(path)
                    .cookie(cookie.clone())
                    .peer_addr("127.0.0.1:1234".parse().unwrap())
                    .insert_header((header::HOST, "localhost"))
                    .insert_header((header::ORIGIN, "http://localhost"))
                    .set_json(ComputerUseApplicationPolicyUpdate {
                        expected_revision,
                        allowed_application_paths: vec![],
                    })
                    .to_request(),
            )
            .await;
            let body: serde_json::Value = test::read_body_json(response).await;
            if body["data"].is_object() {
                assert_eq!(body["data"]["revision"], 1);
                assert!(body["data"].get("enabled").is_none());
            } else {
                assert_eq!(
                    body["code"],
                    serde_json::json!(desk_utils::error::DeskErrorCode::REVISION_CONFLICT)
                );
            }
        }
        assert_eq!(shared.read().await.computer_use.revision, 1);
    }
}
