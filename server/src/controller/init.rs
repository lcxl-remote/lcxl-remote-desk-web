use actix_web::{HttpResponse, post, web};
use log::info;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{error::DeskError, model::settings::SharedSettings};

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct InitParams {
    pub username: String,
    pub password: String,
}

#[utoipa::path(
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

    settings.save()?;
    info!("System initialized successfully");
    Ok(HttpResponse::Ok().finish())
}
