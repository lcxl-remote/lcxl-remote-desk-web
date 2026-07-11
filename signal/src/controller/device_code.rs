use std::collections::HashSet;

use actix_web::{HttpResponse, delete, get, post, put, web};
use desk_utils::{error::DeskErrorCode, rest::RestResponse, string::generate_device_code};
use sea_orm::*;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{entity::device_code, error::DeskSignalError, model::SharedConnectionMap};

pub const TAG: &str = "DeviceCode";

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeListParams {
    pub page: Option<u64>,
    pub page_size: Option<u64>,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeCreateParams {
    pub client_id: String,
    pub device_code: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct DeviceCodeUpdateParams {
    pub device_code: String,
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
    let page_size = std::cmp::min(100, std::cmp::max(1, query.page_size.unwrap_or(10)));

    let offset = (page - 1) * page_size;

    let total = device_code::Entity::find().count(db).await?;
    let db_items = device_code::Entity::find()
        .order_by_desc(device_code::Column::Id)
        .offset(offset)
        .limit(page_size)
        .all(db)
        .await?;

    let connection_map_guard = connection_map.read().await;
    let mut online_codes = HashSet::new();
    for (_cid, cstate) in connection_map_guard.iter() {
        if let Some(code) = &cstate.device_code {
            online_codes.insert(code.clone());
        }
    }

    let items = db_items
        .into_iter()
        .map(|model| {
            let is_online = online_codes.contains(&model.device_code);
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

    let new_code = if let Some(code) = params.device_code {
        if code.is_empty() {
            generate_device_code(6)
        } else {
            code
        }
    } else {
        generate_device_code(6)
    };

    let new_model = device_code::ActiveModel {
        client_id: Set(params.client_id),
        device_code: Set(new_code),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    };

    let result = new_model.insert(db).await?;
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

    let Some(result) = apply_device_code_update(db, id, &params.device_code).await? else {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Device code not found",
        ));
    };
    let connection_map_guard = connection_map.read().await;
    let mut is_online = false;
    for (_cid, cstate) in connection_map_guard.iter() {
        if cstate.device_code.as_ref() == Some(&params.device_code) {
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

/// Apply a device-code edit: rotate the stored code and, when the code actually
/// changes, bump `generation` so every access grant minted at the old generation is
/// refused at redeem / stamp time (the `authorize` generation check). This is the
/// single-instance signal's equivalent of the manager's dial-code regeneration
/// (rule 22): no cross-instance directed teardown, since the generation bump alone
/// supersedes the old code. A same-code edit only refreshes `updated_at`, leaving
/// the generation untouched. Returns the updated model, or `None` if no code with
/// that id exists.
pub async fn apply_device_code_update<C: ConnectionTrait>(
    db: &C,
    id: i32,
    new_code: &str,
) -> Result<Option<device_code::Model>, DbErr> {
    let Some(model) = device_code::Entity::find_by_id(id).one(db).await? else {
        return Ok(None);
    };
    let code_changed = model.device_code != new_code;
    let previous_generation = model.generation;
    let mut active: device_code::ActiveModel = model.into();
    active.device_code = Set(new_code.to_string());
    active.updated_at = Set(chrono::Utc::now());
    if code_changed {
        active.generation = Set(previous_generation + 1);
    }
    Ok(Some(active.update(db).await?))
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
            if cstate.device_code.as_ref() == Some(&m.device_code) {
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
    let mut online_codes = std::collections::HashSet::new();
    for (_cid, cstate) in connection_map_guard.iter() {
        if let Some(code) = &cstate.device_code {
            online_codes.insert(code.clone());
        }
    }

    let mut to_delete = vec![];
    for m in models {
        if !online_codes.contains(&m.device_code) {
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
    use sea_orm::{Database, DatabaseConnection, Schema};

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
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

        let updated = apply_device_code_update(&db, seeded.id, "NEW456")
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

        let updated = apply_device_code_update(&db, seeded.id, "SAME00")
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
            apply_device_code_update(&db, 999, "X")
                .await
                .unwrap()
                .is_none()
        );
    }
}
