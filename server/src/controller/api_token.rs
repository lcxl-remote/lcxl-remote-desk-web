use actix_web::{Error as AWError, HttpResponse, post, web};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::settings::SharedSettings;

pub const TAG: &str = "ApiToken";

/// Request body for issuing a host signaling token. Mirrors the manager's
/// `POST /api/tokens` wire shape so a single mobile-client contract drives both
/// ends with no server-type probe. The open-source desk-server ignores `name`:
/// it returns the static `local_signaling_token` rather than minting a
/// per-name token in a database.
#[derive(Deserialize, ToSchema)]
pub struct CreateApiTokenParams {
    /// Token label. Accepted for wire parity with the manager; ignored here.
    pub name: String,
}

/// Issued host signaling token. Same shape as the manager response so the
/// mobile client can reuse one `ApiTokenApi` against either backend.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct CreateApiTokenResult {
    pub token: String,
}

/// Issue a signaling token for a logged-in host. On the open-source
/// desk-server this returns the co-located signaling server's
/// `local_signaling_token` (the same secret the embedded host worker already
/// uses), so a mobile host that logged in via cookie can obtain the token it
/// must present on the signaling WebSocket. A future open-source build may swap
/// the implementation for a real per-user issuance table without changing this
/// wire contract.
///
/// Registered only in modes with a co-located signaling server
/// (`Default` / `Signaling` / `ServiceDaemon`); a pure `DeskServer` has no
/// embedded signaling route and never registers this endpoint.
#[utoipa::path(
    tag = TAG,
    summary = "Issue a host signaling token",
    request_body(content = CreateApiTokenParams),
    responses(
        (status = 200, description = "Issued token", body = RestResponse<CreateApiTokenResult>),
    ),
)]
#[post("/tokens")]
pub async fn create_token(
    _request_json: web::Json<CreateApiTokenParams>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let token = {
        let settings = settings.read().await;
        settings.system.local_signaling_token.clone()
    };

    // Never log the token value: it is now a host credential.
    let Some(token) = token else {
        return Ok(HttpResponse::Ok().json(
            RestResponse::<CreateApiTokenResult>::failed_with_data(
                DeskErrorCode::FEATURE_UNAVAILABLE,
                Some("No local signaling token is available in this startup mode".to_string()),
                None,
            ),
        ));
    };

    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(CreateApiTokenResult {
            token,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Settings, SystemSettings};
    use actix_web::{App, test};

    fn shared_settings(local_token: Option<String>) -> SharedSettings {
        let mut system = SystemSettings::default();
        system.local_signaling_token = local_token;
        let mut settings = Settings::default();
        settings.system = system;
        SharedSettings::from(settings)
    }

    #[actix_web::test]
    async fn returns_local_signaling_token_when_present() {
        let shared = shared_settings(Some("secret-token".to_string()));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(shared))
                .service(create_token),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/tokens")
            .set_json(serde_json::json!({ "name": "mobile-host" }))
            .to_request();
        let resp: RestResponse<CreateApiTokenResult> =
            test::call_and_read_body_json(&app, req).await;

        assert_eq!(resp.code, DeskErrorCode::SUCCESS.code());
        assert_eq!(resp.data.unwrap().token, "secret-token");
    }

    #[actix_web::test]
    async fn returns_feature_unavailable_when_token_missing() {
        let shared = shared_settings(None);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(shared))
                .service(create_token),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/tokens")
            .set_json(serde_json::json!({ "name": "mobile-host" }))
            .to_request();
        let resp: RestResponse<CreateApiTokenResult> =
            test::call_and_read_body_json(&app, req).await;

        assert_eq!(resp.code, DeskErrorCode::FEATURE_UNAVAILABLE.code());
        assert!(resp.data.is_none());
    }
}
