//! Unified access-grant code redemption for the open-source portable server.
//!
//! This replaces the legacy device-code "login" that minted a full-admin session
//! for anyone holding a code. A redeemer is now a **capability-scoped**
//! code-session, not the owner: redemption mints an access grant carrying the
//! code's ceiling and stores a code-session identity (a server-minted principal)
//! in an encrypted session cookie. The signaling plane stamps the code's ceiling
//! on that session's RequestRemote frames, and the REST plane (via
//! `enforce_device_scope`) restricts it to its redeemed target's
//! capability-carrier endpoints. Settings, system info, service management and
//! every other owner-plane surface stay out of reach.
//!
//! Redemption is only meaningful on a trusted-central topology (the portable
//! `Default` mode, where the browser, embedded signal and host are one trusted
//! unit). On any other topology it is hard-rejected **before** the code is
//! resolved, so a code is never enumerated or a target leaked off-topology.

use actix_session::Session;
use actix_web::{Error as AWError, HttpRequest, HttpResponse, post, web};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::RwLock;

use desk_signal_facade::grant::{AccessGrantStore, GrantPrincipal, GrantSessionRecord};
use desk_signal_facade::model::code_session::{CODE_SESSION_KEY, CodeSessionCookie};
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::RemoteDeskTypeEnum;
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;

use crate::model::settings::SharedSettings;

/// Server-minted code-session principal length (characters). High-entropy so a
/// principal cannot be guessed by another anonymous session.
const CODE_SESSION_ID_LEN: usize = 32;

/// Anti-enumeration limit: redeem attempts per client IP per minute.
const REDEEM_ATTEMPTS_PER_MINUTE: u32 = 5;

static REDEEM_RATE_LIMIT: std::sync::LazyLock<RwLock<HashMap<String, (u32, Instant)>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

pub const TAG: &str = "Auth";

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct RedeemCodeParams {
    /// The access-grant code (device code or support code) to redeem.
    pub code: String,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RedeemCodeResult {
    /// The resolved target device connection to control.
    pub target_connection_id: String,
    /// The reusable grant-session token the control end must attach to every
    /// RequestRemote for this target, including desktop and file-manager sessions.
    pub grant_session_id: String,
    /// The redeemed code's capability ceiling, so the control end can hide the
    /// entries a dimension explicitly denies. This is a UX hint only — the host
    /// still enforces `meet(ceiling, global)` plus live approval, so a control end
    /// that ignores it gains nothing. Always present here (the open-source portable
    /// redeem is always capability-scoped); `Option` for wire parity with the
    /// manager, whose owner redeem returns `None` (unrestricted full control).
    pub access_ceiling: Option<SecuritySettings>,
}

#[utoipa::path(
    tag = TAG,
    summary = "Redeem an access-grant code into a capability-scoped session",
    request_body(content = RedeemCodeParams),
    responses(
        (status = 200, description = "Redeem result", body = RestResponse<RedeemCodeResult>),
    ),
)]
#[post("/api/desk/redeem-code")]
pub async fn redeem_code(
    req: HttpRequest,
    body: web::Json<RedeemCodeParams>,
    settings: web::Data<SharedSettings>,
    connection_map: web::Data<desk_signal::model::SharedConnectionMap>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = body.into_inner();

    // Topology gate — strictly before any code resolution (never leak a target
    // off a topology that must not redeem). Only the portable `Default` mode is a
    // trusted-central unit; every other mode hard-rejects.
    let is_trusted_central = {
        let settings = settings.read().await;
        matches!(
            settings.args.startup_mode,
            crate::model::settings::StartupMode::Default
        )
    };
    if !is_trusted_central {
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "Code redemption is not supported on this deployment".to_string(),
        )));
    }

    // Per-IP anti-enumeration rate limit (peer address, never a spoofable
    // `X-Forwarded-For`).
    if redeem_rate_limited(&req).await {
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::TOO_MANY_ATTEMPTS,
            "Too many attempts. Please try again later.".to_string(),
        )));
    }

    let code = params.code.trim().to_string();
    if code.is_empty() {
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::INVALID_PARAMS,
            "Code is empty".to_string(),
        )));
    }

    // Resolve the code to a live target device connection: a `Server` connection
    // whose registered device code matches. The target's `client_id` is the grant
    // audience (server-authoritative, taken from the receiving connection).
    let resolved = {
        let map = connection_map.read().await;
        map.iter().find_map(|(cid, cstate)| {
            let is_server =
                cstate.model.version_info.remote_desk_type == RemoteDeskTypeEnum::Server;
            let code_matches = cstate.device_code.as_deref() == Some(code.as_str());
            let client_id = cstate.model.version_info.client_id.clone();
            match (is_server && code_matches, client_id) {
                (true, Some(client_id)) if !client_id.is_empty() => Some((cid.clone(), client_id)),
                _ => None,
            }
        })
    };
    let Some((target_connection_id, target_client_id)) = resolved else {
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::DEVICE_NOT_FOUND,
            "Code not found or device is offline".to_string(),
        )));
    };

    // Do not mint a grant while the durable central mirror is locked. Without
    // this check a code holder could redeem after the generation bump and keep
    // that newly minted grant for use immediately after the host unlocks.
    match target_is_remote_access_locked(&target_client_id).await {
        Ok(false) => {}
        Ok(true) => {
            return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
                DeskErrorCode::REMOTE_ACCESS_LOCKED,
                "Remote access is locked by the host".to_string(),
            )));
        }
        Err(error) => {
            log::error!("Failed to read remote-access lock state during redeem: {error}");
            return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
                DeskErrorCode::REMOTE_ACCESS_LOCKED,
                "Remote access lock state is unavailable".to_string(),
            )));
        }
    }

    // Load the owner-configured ceiling and live generation for the target's code.
    let (ceiling, generation) = load_code_ceiling(&target_client_id).await;

    // Mint a code-session principal + a reusable grant bound to it, then remember
    // the code-session identity in the encrypted cookie. The signaling plane
    // resolves this identity and looks up the grant to stamp the ceiling; the REST
    // plane scopes the session to this target.
    let code_session_id = desk_utils::string::generate_device_code(CODE_SESSION_ID_LEN);
    let record = GrantSessionRecord {
        principal: GrantPrincipal::from_code_session(&code_session_id),
        target_device: target_client_id,
        access_ceiling: Some(ceiling.clone()),
        generation,
    };
    let store = desk_signal::access_grant::global_access_grant_store();
    let minted = match store
        .mint(
            &record,
            desk_signal_facade::grant::DEFAULT_GRANT_SESSION_TTL_SECS,
        )
        .await
    {
        Ok(m) => m,
        Err(e) => {
            log::error!("Failed to mint access grant: {e}");
            return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to establish a grant session".to_string(),
            )));
        }
    };

    let cookie = CodeSessionCookie {
        code_session_id,
        grant_session_id: minted.grant_session_id.clone(),
        target_connection_id: target_connection_id.clone(),
    };
    if let Err(e) = session.insert(CODE_SESSION_KEY, &cookie) {
        log::error!("Failed to persist code-session cookie: {e}");
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::SYSTEM_ERROR,
            "Failed to establish a code session".to_string(),
        )));
    }

    log::info!("Access-grant code redeemed for a capability-scoped session");
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(RedeemCodeResult {
            target_connection_id,
            grant_session_id: minted.grant_session_id,
            access_ceiling: Some(ceiling),
        })),
    )
}

async fn target_is_remote_access_locked(client_id: &str) -> Result<bool, sea_orm::DbErr> {
    target_is_remote_access_locked_in(desk_signal::db::get_db(), client_id).await
}

async fn target_is_remote_access_locked_in(
    db: &sea_orm::DatabaseConnection,
    client_id: &str,
) -> Result<bool, sea_orm::DbErr> {
    use sea_orm::EntityTrait as _;
    Ok(
        desk_signal::entity::host_remote_access_state::Entity::find_by_id(client_id)
            .one(db)
            .await?
            .is_some_and(|state| state.locked),
    )
}

/// Load `(ceiling, generation)` for the device code registered under
/// `client_id`. A missing row (target not registered) yields the restrictive
/// all-prompt ceiling at generation 0 — a redeemer never gains a wider ceiling
/// than configured.
async fn load_code_ceiling(client_id: &str) -> (SecuritySettings, i64) {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let db = desk_signal::db::get_db();
    match desk_signal::entity::device_code::Entity::find()
        .filter(desk_signal::entity::device_code::Column::ClientId.eq(client_id))
        .one(db)
        .await
    {
        Ok(Some(row)) => (
            SecuritySettings::parse_code_ceiling(row.capabilities.as_deref()),
            row.generation as i64,
        ),
        _ => (SecuritySettings::all_prompt(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Args, Settings, SharedSettings, StartupMode};
    use actix_session::{SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{
        App,
        cookie::Key,
        test::{self, TestRequest},
        web,
    };

    fn settings_with_mode(mode: StartupMode) -> SharedSettings {
        let mut settings = Settings::default();
        let mut temp_path = std::env::temp_dir();
        temp_path.push(format!("redeem_test_{}.toml", uuid::Uuid::new_v4()));
        settings.args = Args {
            config_file_path: Some(temp_path.clone()),
            startup_mode: mode,
            ..Default::default()
        };
        SharedSettings::from(settings)
    }

    async fn call_redeem(settings: SharedSettings, peer: &str, code: &str) -> serde_json::Value {
        let connection_map = web::Data::new(desk_signal::model::SharedConnectionMap::from(
            std::collections::BTreeMap::new(),
        ));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(connection_map)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(redeem_code),
        )
        .await;
        let req = TestRequest::post()
            .peer_addr(peer.parse().unwrap())
            .uri("/api/desk/redeem-code")
            .set_json(serde_json::json!({ "code": code }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        test::read_body_json(resp).await
    }

    /// Redemption is refused before any code resolution on a non-trusted-central
    /// topology, so a target is never leaked off-topology.
    #[actix_web::test]
    async fn topology_gate_rejects_non_default_mode() {
        let body = call_redeem(
            settings_with_mode(StartupMode::Signaling),
            "10.0.0.1:1",
            "ABC123",
        )
        .await;
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], DeskErrorCode::FEATURE_UNAVAILABLE.code());
    }

    #[actix_web::test]
    async fn empty_code_is_rejected() {
        let body = call_redeem(
            settings_with_mode(StartupMode::Default),
            "10.0.0.2:1",
            "   ",
        )
        .await;
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], DeskErrorCode::INVALID_PARAMS.code());
    }

    /// An unknown code resolves to no live device — reported uniformly, never as a
    /// distinct "wrong code vs offline" oracle.
    #[actix_web::test]
    async fn unknown_code_is_device_not_found() {
        let body = call_redeem(
            settings_with_mode(StartupMode::Default),
            "10.0.0.3:1",
            "NOPE01",
        )
        .await;
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], DeskErrorCode::DEVICE_NOT_FOUND.code());
    }

    /// The success result carries the code's ceiling under `access_ceiling`, the
    /// wire field the control end reads to hide capability-denied entries. Guards the
    /// field name / presence the frontend contract depends on (a full mint path needs
    /// a live connection + global DB, out of reach for a unit test).
    #[test]
    fn result_serializes_access_ceiling() {
        let ceiling = SecuritySettings {
            allow_terminal: Some(true),
            allow_file_transfer: Some(false),
            ..SecuritySettings::all_prompt()
        };
        let result = RedeemCodeResult {
            target_connection_id: "conn-1".to_string(),
            grant_session_id: "gs-1".to_string(),
            access_ceiling: Some(ceiling),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["access_ceiling"]["allow_terminal"], true);
        assert_eq!(json["access_ceiling"]["allow_file_transfer"], false);
        assert_eq!(
            json["access_ceiling"]["allow_clipboard_sync"],
            serde_json::Value::Null
        );
    }

    #[actix_web::test]
    async fn redeem_lock_lookup_reads_the_durable_oss_mirror() {
        use sea_orm::{ActiveModelTrait as _, ConnectionTrait as _, Database, Schema, Set};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(
            &schema.create_table_from_entity(desk_signal::entity::host_remote_access_state::Entity),
        )
        .await
        .unwrap();
        desk_signal::entity::host_remote_access_state::ActiveModel {
            client_id: Set("host-locked".into()),
            locked: Set(true),
            state_version: Set(4),
            lock_id: Set(Some("lock-a".into())),
            updated_at: Set(chrono::Utc::now()),
        }
        .insert(&db)
        .await
        .unwrap();

        assert!(
            target_is_remote_access_locked_in(&db, "host-locked")
                .await
                .unwrap()
        );
        assert!(
            !target_is_remote_access_locked_in(&db, "host-unseen")
                .await
                .unwrap()
        );
    }
}

/// Record a redeem attempt and report whether the client IP is over the limit.
/// Uses the real TCP peer address, deliberately ignoring a spoofable
/// `X-Forwarded-For` header (mirrors the login rate limiter).
async fn redeem_rate_limited(req: &HttpRequest) -> bool {
    let ip = req
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let now = Instant::now();
    let mut limit = REDEEM_RATE_LIMIT.write().await;
    // Drop entries whose window has elapsed so the map cannot grow without bound
    // on a long-lived public server (each distinct IP would otherwise leak a slot).
    limit.retain(|_, (_, last_time)| now.duration_since(*last_time).as_secs() < 60);
    match limit.get_mut(&ip) {
        Some((count, last_time)) if now.duration_since(*last_time).as_secs() < 60 => {
            if *count >= REDEEM_ATTEMPTS_PER_MINUTE {
                return true;
            }
            *count += 1;
            false
        }
        Some((count, last_time)) => {
            *count = 1;
            *last_time = now;
            false
        }
        None => {
            limit.insert(ip, (1, now));
            false
        }
    }
}
