use actix_web::{HttpResponse, post, web};
use log::info;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{error::DeskError, model::settings::SharedSettings};

pub const TAG: &str = "System";

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct InitParams {
    pub username: String,
    pub password: String,
    pub telemetry_consent: bool,
}

#[utoipa::path(
    tag = TAG,
    summary = "Initialize system",
    request_body(content = InitParams),
    responses(
        (status = 200, description = "System initialized successfully"),
        (status = 403, description = "System already initialized"),
    ),
)]
#[post("/api/init")]
pub async fn init_system(
    request_json: web::Json<InitParams>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    let mut settings = settings.write().await;

    // Check if system is already initialized string is not empty
    if !settings.user.login_password.is_empty() {
        return Err(DeskError::new_custom_error(
            desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
            "System is already initialized",
        ));
    }

    let params = request_json.into_inner();
    settings.user.login_user_name = params.username;
    settings.user.login_password = params.password;
    settings.system.telemetry_consent = Some(params.telemetry_consent);

    settings.save()?;
    info!("System initialized successfully");
    Ok(HttpResponse::Ok().json(desk_utils::rest::RestResponse::succeed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Args, Settings};
    use actix_web::{App, test, web};
    use std::env;

    async fn create_test_settings() -> SharedSettings {
        let mut settings = Settings::default();
        // Ensure it's not "initialized"
        settings.user.login_password = "".to_string();

        let mut temp_path = env::temp_dir();
        temp_path.push(format!("desk_init_test_{}.toml", uuid::Uuid::new_v4()));
        settings.args = Args {
            config_file_path: temp_path.to_string_lossy().to_string(),
            ..Default::default()
        };

        SharedSettings::from(settings)
    }

    #[actix_web::test]
    async fn test_init_system_success() {
        let settings = create_test_settings().await;
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .service(init_system),
        )
        .await;

        let params = InitParams {
            username: "admin".to_string(),
            password: "new_password".to_string(),
            telemetry_consent: true,
        };

        let req = test::TestRequest::post()
            .uri("/api/init")
            .set_json(&params)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: desk_utils::rest::RestResponse<()> = test::read_body_json(resp).await;
        assert!(body.success);
    }

    #[actix_web::test]
    async fn test_init_system_already_initialized() {
        let settings = create_test_settings().await;
        {
            let mut s = settings.write().await;
            s.user.login_password = "already_set".to_string();
        }

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .service(init_system),
        )
        .await;

        let params = InitParams {
            username: "admin".to_string(),
            password: "new_password".to_string(),
            telemetry_consent: false,
        };

        let req = test::TestRequest::post()
            .uri("/api/init")
            .set_json(&params)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

        let body: desk_utils::rest::RestResponse<()> = test::read_body_json(resp).await;
        assert!(!body.success);
    }
}
