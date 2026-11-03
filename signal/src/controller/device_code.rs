#[cfg(test)]
use crate::config::connection::DatabaseConnection;
use std::collections::HashSet;

use actix_web::{HttpResponse, delete, get, post, put, web};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use sea_orm::*;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use desk_signal_facade::model::security_settings::SecuritySettings;

use crate::{entity::device_code, error::DeskSignalError, model::SharedConnectionMap};

fn is_online_client(
    state: &desk_signal_facade::model::connection::ConnectionState,
    client_id: &str,
) -> bool {
    state.model.peer.remote_desk_type
        == desk_signal_facade::model::signal::RemoteDeskTypeEnum::Server
        && state.client_id.as_deref() == Some(client_id)
}

pub const TAG: &str = "DeviceCode";

/// Serialize a per-code capability ceiling for storage in the `capabilities`
/// column, mirroring what redeem reads back with [`SecuritySettings::parse_code_ceiling`].
/// `None` clears the column (the code falls back to the all-prompt ceiling).
fn encode_capabilities(capabilities: Option<&SecuritySettings>) -> Option<String> {
    capabilities.map(|c| serde_json::to_string(c).unwrap_or_default())
}

/// Decode the stored `capabilities` column for display. Unconfigured or malformed
/// JSON surfaces as `None` (the owner sees "not configured") rather than silently
/// substituting the all-prompt fallback that redeem applies at grant time.
fn decode_capabilities(stored: Option<&str>) -> Option<SecuritySettings> {
    stored.and_then(|s| serde_json::from_str(s).ok())
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeListParams {
    pub page: Option<u64>,
    pub page_size: Option<u64>,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeCreateParams {
    pub client_id: String,
    pub device_code: Option<String>,
    /// Per-code capability ceiling. `None` leaves the code at the all-prompt
    /// default (every dimension prompts on redeem).
    #[serde(default)]
    pub capabilities: Option<SecuritySettings>,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeUpdateParams {
    /// Omit to generate a fresh code using the current length setting.
    pub device_code: Option<String>,
    /// Replacement capability ceiling. The edit form always sends the full
    /// object, so `None` clears any prior config back to the all-prompt default.
    #[serde(default)]
    pub capabilities: Option<SecuritySettings>,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeListResult {
    pub items: Vec<DeviceCodeItem>,
    pub total: u64,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeItem {
    pub id: i32,
    pub client_id: String,
    pub device_code: String,
    pub created_at: String,
    pub updated_at: String,
    pub is_online: bool,
    /// The code's configured capability ceiling, or `None` when unconfigured.
    pub capabilities: Option<SecuritySettings>,
}

impl DeviceCodeItem {
    pub fn from_model(model: device_code::Model, is_online: bool) -> Self {
        Self {
            id: model.id,
            client_id: model.client_id,
            device_code: model.device_code,
            created_at: model.created_at.to_rfc3339(),
            updated_at: model.updated_at.to_rfc3339(),
            is_online,
            capabilities: decode_capabilities(model.capabilities.as_deref()),
        }
    }
}

#[utoipa::path(
    tag = TAG,
    summary = "List device codes",
    params(
        ("page" = Option<u64>, Query, description = "Page number"),
        ("page_size" = Option<u64>, Query, description = "Page size")
    ),
    responses(
        (status = 200, description = "List device codes successfully"),
    ),
)]
#[get("/device_codes")]
pub async fn list_device_codes(
    query: web::Query<DeviceCodeListParams>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let page = std::cmp::max(1, query.page.unwrap_or(1));
    let page_size = query.page_size.unwrap_or(10).clamp(1, 100);

    let offset = (page - 1) * page_size;

    let total = device_code::Entity::find().count(db).await?;
    let db_items = device_code::Entity::find()
        .order_by_desc(device_code::Column::Id)
        .offset(offset)
        .limit(page_size)
        .all(db)
        .await?;

    let connection_map_guard = connection_map.read().await;
    let mut online_clients = HashSet::new();
    for (_cid, cstate) in connection_map_guard.iter() {
        if cstate.model.peer.remote_desk_type
            == desk_signal_facade::model::signal::RemoteDeskTypeEnum::Server
            && let Some(client) = &cstate.client_id
        {
            online_clients.insert(client.clone());
        }
    }

    let items = db_items
        .into_iter()
        .map(|model| {
            let is_online = online_clients.contains(&model.client_id);
            DeviceCodeItem::from_model(model, is_online)
        })
        .collect();
    let result = DeviceCodeListResult { items, total };

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(result)))
}

#[utoipa::path(
    tag = TAG,
    summary = "Create device code",
    request_body(content = DeviceCodeCreateParams),
    responses(
        (status = 200, description = "Create device code successfully"),
    ),
)]
#[post("/device_codes")]
pub async fn create_device_code(
    body: web::Json<DeviceCodeCreateParams>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let params = body.into_inner();

    let explicit = params.device_code.filter(|code| !code.is_empty());
    let result = crate::device_code_store::create(
        db,
        params.client_id,
        explicit,
        encode_capabilities(params.capabilities.as_ref()),
    )
    .await?;
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(DeviceCodeItem::from_model(
            result, false,
        ))),
    )
}

#[utoipa::path(
    tag = TAG,
    summary = "Update device code",
    request_body(content = DeviceCodeUpdateParams),
    responses(
        (status = 200, description = "Update device code successfully"),
    ),
)]
#[put("/device_codes/{id}")]
pub async fn update_device_code(
    path: web::Path<i32>,
    body: web::Json<DeviceCodeUpdateParams>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let id = path.into_inner();
    let params = body.into_inner();

    let Some(result) = apply_device_code_update_generated(
        db,
        id,
        params.device_code.as_deref(),
        params.capabilities.as_ref(),
        desk_utils::string::generate_device_code,
    )
    .await?
    else {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Device code not found",
        ));
    };
    let connection_map_guard = connection_map.read().await;
    let mut is_online = false;
    for (_cid, cstate) in connection_map_guard.iter() {
        if is_online_client(cstate, &result.client_id) {
            is_online = true;
            break;
        }
    }
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(DeviceCodeItem::from_model(
            result, is_online,
        ))),
    )
}

/// Apply a device-code edit: rotate the stored code, replace the capability
/// ceiling, and, when the code actually changes, bump `generation` so every access
/// grant minted at the old generation is refused at redeem / stamp time (the
/// `authorize` generation check). This is the single-instance signal's equivalent
/// of the manager's dial-code regeneration (rule 22): no cross-instance directed
/// teardown, since the generation bump alone supersedes the old code. A same-code
/// edit only refreshes `updated_at` and the capabilities, leaving the generation
/// untouched. Returns the updated model, or `None` if no code with that id exists.
pub async fn apply_device_code_update<
    C: ConnectionTrait + crate::config::ConfigConnection + TransactionTrait,
>(
    db: &C,
    id: i32,
    new_code: &str,
    capabilities: Option<&SecuritySettings>,
) -> Result<Option<device_code::Model>, DbErr> {
    apply_device_code_update_generated(
        db,
        id,
        Some(new_code),
        capabilities,
        desk_utils::string::generate_device_code,
    )
    .await
}
async fn apply_device_code_update_generated<
    C: ConnectionTrait + crate::config::ConfigConnection + TransactionTrait,
    F: FnMut(usize) -> String + Send + 'static,
>(
    db: &C,
    id: i32,
    new_code: Option<&str>,
    capabilities: Option<&SecuritySettings>,
    mut generate: F,
) -> Result<Option<device_code::Model>, DbErr> {
    let length = if new_code.is_none() {
        Some(db.config_read().await.device_codes.length as usize)
    } else {
        None
    };
    let explicit = new_code.map(str::to_owned);
    let capabilities = encode_capabilities(capabilities);
    db.transaction::<_, Option<device_code::Model>, DbErr>(|txn| {
        Box::pin(async move {
            let Some(model) = device_code::Entity::find_by_id(id).one(txn).await? else {
                return Ok(None);
            };
            for _ in 0..if explicit.is_some() {
                1
            } else {
                desk_signal_facade::device_code::MAX_ATTEMPTS
            } {
                let code = explicit
                    .clone()
                    .unwrap_or_else(|| generate(length.expect("automatic length")));
                if explicit.is_none() && code == model.device_code {
                    continue;
                }
                let changed = code != model.device_code;
                let mut active: device_code::ActiveModel = model.clone().into();
                active.device_code = Set(code.clone());
                active.capabilities = Set(capabilities.clone());
                active.updated_at = Set(chrono::Utc::now());
                if changed {
                    active.generation = Set(model
                        .generation
                        .checked_add(1)
                        .ok_or_else(|| DbErr::Custom("Device generation exhausted".into()))?);
                }
                match active.update(txn).await {
                    Ok(row) => return Ok(Some(row)),
                    Err(error) => {
                        if explicit.is_some()
                            || !matches!(
                                error.sql_err(),
                                Some(SqlErr::UniqueConstraintViolation(_))
                            )
                        {
                            return Err(error);
                        }
                        if device_code::Entity::find()
                            .filter(device_code::Column::DeviceCode.eq(code))
                            .filter(device_code::Column::Id.ne(id))
                            .one(txn)
                            .await?
                            .is_none()
                        {
                            return Err(error);
                        }
                    }
                }
            }
            Err(DbErr::Custom(
                "Could not regenerate a unique device code after 5 attempts".into(),
            ))
        })
    })
    .await
    .map_err(|e| match e {
        TransactionError::Connection(e) | TransactionError::Transaction(e) => e,
    })
}

#[utoipa::path(
    tag = TAG,
    summary = "Delete device code",
    responses(
        (status = 200, description = "Delete device code successfully"),
    ),
)]
#[delete("/device_codes/{id}")]
pub async fn delete_device_code(
    path: web::Path<i32>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let id = path.into_inner();

    let model = device_code::Entity::find_by_id(id).one(db).await?;

    if let Some(m) = model {
        let connection_map_guard = connection_map.read().await;
        for (_cid, cstate) in connection_map_guard.iter() {
            if is_online_client(cstate, &m.client_id) {
                return Err(DeskSignalError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "Cannot delete an online device code",
                ));
            }
        }

        device_code::Entity::delete_by_id(id).exec(db).await?;
        Ok(HttpResponse::Ok().json(RestResponse::succeed()))
    } else {
        Err(DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Device code not found",
        ))
    }
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeBatchDeleteParams {
    pub ids: Vec<i32>,
}

#[utoipa::path(
    tag = TAG,
    summary = "Batch delete device codes",
    request_body(content = DeviceCodeBatchDeleteParams),
    responses(
        (status = 200, description = "Batch delete device codes successfully"),
    ),
)]
#[post("/device_codes/batch_delete")]
pub async fn batch_delete_device_codes(
    body: web::Json<DeviceCodeBatchDeleteParams>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let params = body.into_inner();

    let models = device_code::Entity::find()
        .filter(device_code::Column::Id.is_in(params.ids))
        .all(db)
        .await?;

    let connection_map_guard = connection_map.read().await;
    let mut online_clients = std::collections::HashSet::new();
    for (_cid, cstate) in connection_map_guard.iter() {
        if cstate.model.peer.remote_desk_type
            == desk_signal_facade::model::signal::RemoteDeskTypeEnum::Server
            && let Some(client) = &cstate.client_id
        {
            online_clients.insert(client.clone());
        }
    }

    let mut to_delete = vec![];
    for m in models {
        if !online_clients.contains(&m.client_id) {
            to_delete.push(m.id);
        }
    }

    if !to_delete.is_empty() {
        device_code::Entity::delete_many()
            .filter(device_code::Column::Id.is_in(to_delete))
            .exec(db)
            .await?;
    }

    Ok(HttpResponse::Ok().json(RestResponse::succeed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Schema;

    #[tokio::test]
    async fn automatic_rotation_uses_new_length_and_collision_failure_preserves_old_state() {
        let db = setup().await;
        let old = seed(&db, "old", "ABC234").await;
        seed(&db, "taken", "AAAAAAAA").await;
        let mut candidates = ["AAAAAAAA", "BBBBBBBB"].into_iter();
        let changed = apply_device_code_update_generated(&db, old.id, None, None, move |length| {
            assert_eq!(length, 8);
            candidates.next().unwrap().into()
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(changed.device_code, "BBBBBBBB");
        assert_eq!(changed.generation, 1);
        assert!(
            apply_device_code_update_generated(&db, old.id, None, None, |_| "AAAAAAAA".into())
                .await
                .is_err()
        );
        assert_eq!(
            device_code::Entity::find_by_id(old.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            changed
        );
        crate::device_code_config::update(
            &db,
            &desk_signal_facade::device_code::UpdateDeviceCodeSettings {
                expected_revision: 0,
                length: 12,
            },
        )
        .await
        .unwrap();
        let fresh = apply_device_code_update_generated(
            &db,
            old.id,
            None,
            None,
            desk_utils::string::generate_device_code,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fresh.device_code.len(), 12);
        assert_eq!(fresh.generation, 2);
    }

    #[actix_web::test]
    async fn online_identity_survives_a_stale_cached_code_and_excludes_browsers() {
        use actix_web::FromRequest;
        use desk_signal_facade::model::{
            auth_context::AuthContext,
            connection::{ConnectionModel, ConnectionState},
            os::OperationSystemEnum,
            peer::{HostExecCapabilities, PeerInfo},
            signal::RemoteDeskTypeEnum,
        };
        use std::sync::Arc;
        let req = actix_web::test::TestRequest::get()
            .insert_header(("upgrade", "websocket"))
            .insert_header(("connection", "upgrade"))
            .insert_header(("sec-websocket-version", "13"))
            .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
            .to_http_request();
        let payload = web::Payload::from_request(&req, &mut actix_web::dev::Payload::None)
            .await
            .unwrap();
        let (_response, session, _) = actix_ws::handle(&req, payload).unwrap();
        let mut state = ConnectionState {
            lifecycle: Default::default(),
            model: Arc::new(ConnectionModel {
                connection_id: "c1".into(),
                ip: None,
                version_info: None,
                peer: PeerInfo {
                    remote_desk_type: RemoteDeskTypeEnum::Server,
                    operation_system: OperationSystemEnum::Other,
                    display_name: None,
                },
                exec_capabilities: HostExecCapabilities::default(),
                ai_assistant_settings: None,
                device_id: Some("client".into()),
                owner_node_id: None,
            }),
            client_id: Some("client".into()),
            session: Arc::new(tokio::sync::RwLock::new(session)),
            terminal_connection_ids: Default::default(),
            request_callback_map: Default::default(),
            device_code: Some("OLD234".into()),
            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        };
        assert!(is_online_client(&state, "client"));
        let db = setup().await;
        let row = seed(&db, "client", "OLD234").await;
        let connections = crate::model::SharedConnectionMap::new_oss();
        connections.write().await.insert("c1".into(), state.clone());
        let before = crate::device_code_store::resolve_online(&db, &connections, "OLD234")
            .await
            .unwrap()
            .unwrap();
        let changed = apply_device_code_update(&db, row.id, "NEW23456", None)
            .await
            .unwrap()
            .unwrap();
        assert!(
            crate::device_code_store::resolve_online(&db, &connections, "OLD234")
                .await
                .unwrap()
                .is_none()
        );
        let after = crate::device_code_store::resolve_online(&db, &connections, "NEW23456")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.0, "c1");
        assert_eq!(after.1.generation, changed.generation);
        assert_eq!(before.1.generation, 0);
        assert_eq!(after.1.generation, 1);
        state.device_code = Some("STALE567".into());
        assert!(is_online_client(&state, "client"));
        assert!(!is_online_client(&state, "other-client"));
        Arc::make_mut(&mut state.model).peer.remote_desk_type = RemoteDeskTypeEnum::Browser;
        state.auth_context = AuthContext::cookie(1, RemoteDeskTypeEnum::Browser);
        assert!(!is_online_client(&state, "client"));
        db.execute_unprepared("DROP TABLE device_code")
            .await
            .unwrap();
        assert!(
            crate::device_code_store::resolve_online(&db, &connections, "NEW23456")
                .await
                .is_err()
        );
    }

    async fn setup() -> DatabaseConnection {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(device_code::Entity))
            .await
            .unwrap();
        db
    }

    async fn seed(db: &DatabaseConnection, client_id: &str, code: &str) -> device_code::Model {
        device_code::ActiveModel {
            client_id: Set(client_id.to_string()),
            device_code: Set(code.to_string()),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn changing_the_code_bumps_generation() {
        let db = setup().await;
        let seeded = seed(&db, "client-a", "OLD123").await;
        assert_eq!(seeded.generation, 0);

        let updated = apply_device_code_update(&db, seeded.id, "NEW456", None)
            .await
            .unwrap()
            .expect("code exists");
        // A rotated code supersedes the old generation so old grants are refused.
        assert_eq!(updated.device_code, "NEW456");
        assert_eq!(updated.generation, 1);
    }

    #[tokio::test]
    async fn same_code_leaves_generation_untouched() {
        let db = setup().await;
        let seeded = seed(&db, "client-a", "SAME00").await;

        let updated = apply_device_code_update(&db, seeded.id, "SAME00", None)
            .await
            .unwrap()
            .expect("code exists");
        // No code change ⇒ no regeneration ⇒ generation is untouched.
        assert_eq!(updated.generation, 0);
    }

    #[tokio::test]
    async fn unknown_id_is_none() {
        let db = setup().await;
        assert!(
            apply_device_code_update(&db, 999, "X", None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_stores_and_clears_capabilities() {
        let db = setup().await;
        let seeded = seed(&db, "client-a", "CAP000").await;
        assert!(seeded.capabilities.is_none());

        // Configuring a ceiling persists as JSON and decodes back on read.
        let ceiling = SecuritySettings {
            allow_terminal: Some(true),
            ..SecuritySettings::all_prompt()
        };
        let updated = apply_device_code_update(&db, seeded.id, "CAP000", Some(&ceiling))
            .await
            .unwrap()
            .expect("code exists");
        assert_eq!(
            decode_capabilities(updated.capabilities.as_deref()),
            Some(ceiling.clone())
        );
        // Redeem's parser round-trips the same stored value.
        assert_eq!(
            SecuritySettings::parse_code_ceiling(updated.capabilities.as_deref()),
            ceiling
        );

        // A subsequent edit that omits capabilities clears the column back to null.
        let cleared = apply_device_code_update(&db, seeded.id, "CAP000", None)
            .await
            .unwrap()
            .expect("code exists");
        assert!(cleared.capabilities.is_none());
    }

    #[tokio::test]
    async fn item_surfaces_configured_ceiling() {
        let db = setup().await;
        let seeded = seed(&db, "client-a", "ITEM00").await;
        let ceiling = SecuritySettings {
            allow_file_transfer: Some(false),
            ..SecuritySettings::all_prompt()
        };
        apply_device_code_update(&db, seeded.id, "ITEM00", Some(&ceiling))
            .await
            .unwrap();
        let refreshed = device_code::Entity::find_by_id(seeded.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let item = DeviceCodeItem::from_model(refreshed, false);
        assert_eq!(item.capabilities, Some(ceiling));
    }
}
