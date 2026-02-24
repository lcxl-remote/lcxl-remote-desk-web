use std::collections::HashSet;

use actix_web::{HttpResponse, delete, get, post, put, web};
use desk_utils::{error::DeskErrorCode, rest::RestResponse, string::generate_device_code};
use sea_orm::*;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{entity::device_code, error::DeskSignalError, model::SharedSessionMap};

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
    session_map: web::Data<crate::model::SharedSessionMap>,
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

    let session_map_guard = session_map.read().await;
    let mut online_codes = HashSet::new();
    for (_sid, sstate) in session_map_guard.iter() {
        if let Some(code) = &sstate.device_code {
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
    session_map: web::Data<SharedSessionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let id = path.into_inner();
    let params = body.into_inner();

    let model = device_code::Entity::find_by_id(id).one(db).await?;

    if let Some(model) = model {
        let mut active_model: device_code::ActiveModel = model.into();
        active_model.device_code = Set(params.device_code.clone());
        active_model.updated_at = Set(chrono::Utc::now());
        let result = active_model.update(db).await?;
        let session_map_guard = session_map.read().await;
        let mut is_online = false;
        for (_sid, sstate) in session_map_guard.iter() {
            if sstate.device_code.as_ref() == Some(&params.device_code) {
                is_online = true;
                break;
            }
        }
        Ok(
            HttpResponse::Ok().json(RestResponse::succeed_with_data(DeviceCodeItem::from_model(
                result, is_online,
            ))),
        )
    } else {
        Err(DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Device code not found",
        ))
    }
}

#[utoipa::path(
    summary = "Delete device code",
    responses(
        (status = 200, description = "Delete device code successfully"),
    ),
)]
#[delete("/device_codes/{id}")]
pub async fn delete_device_code(
    path: web::Path<i32>,
    session_map: web::Data<crate::model::SharedSessionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let id = path.into_inner();

    let model = device_code::Entity::find_by_id(id).one(db).await?;

    if let Some(m) = model {
        let session_map_guard = session_map.read().await;
        for (_sid, sstate) in session_map_guard.iter() {
            if sstate.device_code.as_ref() == Some(&m.device_code) {
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
    summary = "Batch delete device codes",
    request_body(content = DeviceCodeBatchDeleteParams),
    responses(
        (status = 200, description = "Batch delete device codes successfully"),
    ),
)]
#[post("/device_codes/batch_delete")]
pub async fn batch_delete_device_codes(
    body: web::Json<DeviceCodeBatchDeleteParams>,
    session_map: web::Data<crate::model::SharedSessionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let params = body.into_inner();

    let models = device_code::Entity::find()
        .filter(device_code::Column::Id.is_in(params.ids))
        .all(db)
        .await?;

    let session_map_guard = session_map.read().await;
    let mut online_codes = std::collections::HashSet::new();
    for (_sid, sstate) in session_map_guard.iter() {
        if let Some(code) = &sstate.device_code {
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
