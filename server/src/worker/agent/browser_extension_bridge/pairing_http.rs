//! Exercise proof issuance and redemption through the actual REST owner guard.
use super::*;
use actix_session::{Session, SessionMiddleware, storage::CookieSessionStore};
use actix_web::{cookie::Key, http::StatusCode, middleware::from_fn, test as at};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};

async fn owner(session: Session) -> HttpResponse {
    session
        .set_current_user(&CurrentUser::new_admin("owner"))
        .unwrap();
    HttpResponse::Ok().finish()
}

#[actix_web::test]
async fn local_proof_requires_owner_and_local_origin_and_is_redeemed_once() {
    use crate::model::settings::{Settings, SharedSettings};
    use std::os::unix::fs::DirBuilderExt;
    let root = private_test_directory();
    let mut settings = Settings::for_test_config(&root.path().join("config.json"));
    let device = settings.system.get_or_generate_client_id();
    let data_root = settings.paths().data_root().to_path_buf();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data_root)
        .unwrap();
    let proof = pairing::issue_local_proof(&data_root).unwrap();
    let app = at::init_service(
        App::new()
            .app_data(web::Data::new(SharedSettings::from(settings)))
            .wrap(SessionMiddleware::new(
                CookieSessionStore::default(),
                Key::generate(),
            ))
            .route("/seed-owner", web::post().to(owner))
            .service(
                web::scope("/api/desk")
                    .wrap(from_fn(crate::controller::user::enforce_device_scope))
                    .service(
                        crate::controller::browser_extension::create_browser_extension_pairing,
                    ),
            ),
    )
    .await;
    let login = at::call_service(
        &app,
        at::TestRequest::post().uri("/seed-owner").to_request(),
    )
    .await;
    let cookie = login.response().cookies().next().unwrap().into_owned();
    let request = |peer: &str, origin: &str, token: &str| {
        at::TestRequest::post()
            .uri("/api/desk/browser-extension/pairing")
            .peer_addr(peer.parse().unwrap())
            .insert_header(("host", "127.0.0.1:8080"))
            .insert_header(("origin", origin))
            .set_json(serde_json::json!({"local_proof": token}))
    };
    let anonymous = at::try_call_service(
        &app,
        request("127.0.0.1:1234", "http://127.0.0.1:8080", &proof).to_request(),
    )
    .await;
    assert_eq!(
        anonymous.err().unwrap().as_response_error().status_code(),
        StatusCode::UNAUTHORIZED
    );
    // Neither absent token nor absent endpoint may spend the local proof.
    for token_ready in [false, true] {
        if token_ready {
            load_or_create_pairing_token(&data_root).unwrap();
        }
        let unavailable = at::call_service(
            &app,
            request("127.0.0.1:1234", "http://127.0.0.1:8080", &proof)
                .cookie(cookie.clone())
                .to_request(),
        )
        .await;
        assert_eq!(unavailable.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    let bound = endpoint::bind(&data_root, &device).unwrap();
    let secret = read_pairing_token(&data_root).unwrap();
    for (peer, origin, token) in [
        ("192.0.2.1:1234", "http://127.0.0.1:8080", proof.as_str()),
        ("127.0.0.1:1234", "http://example.invalid", proof.as_str()),
        ("127.0.0.1:1234", "http://127.0.0.1:8080", "invalid-proof"),
    ] {
        let response = at::call_service(
            &app,
            request(peer, origin, token)
                .cookie(cookie.clone())
                .to_request(),
        )
        .await;
        let body: serde_json::Value = if response.status().is_success() {
            at::read_body_json(response).await
        } else {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let bytes = at::read_body(response).await;
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
            );
            continue;
        };
        assert_eq!(body["success"], false);
        assert_eq!(
            body["code"],
            desk_utils::error::DeskErrorCode::PERMISSION_ERROR.code()
        );
        assert!(!body.to_string().contains(&secret));
    }
    let response = at::call_service(
        &app,
        request("127.0.0.1:1234", "http://127.0.0.1:8080", &proof)
            .cookie(cookie.clone())
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let body: serde_json::Value = at::read_body_json(response).await;
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["pairing_code"], secret);
    assert_eq!(
        body["data"]["bridge_url"],
        pairing_endpoint(&data_root, &device).unwrap()
    );
    let replay = at::call_service(
        &app,
        request("127.0.0.1:1234", "http://127.0.0.1:8080", &proof)
            .cookie(cookie)
            .to_request(),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    drop(bound);
}
