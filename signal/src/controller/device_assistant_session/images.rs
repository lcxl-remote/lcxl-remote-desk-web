use super::*;
use desk_agent_protocol::visual_evidence::VisualEvidenceFrame;
#[utoipa::path(tag = TAG, params(
    ("session" = String, Query), ("before" = Option<String>, Query)),
    responses((status = 200, body = RestResponse<Vec<VisualEvidenceFrame>>)))]
#[get("/my/device-assistant-session/images")]
pub async fn list_assistant_images(
    query: web::Query<AssistantImageQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    match crate::agent_image_store::list(&db, &query.session, &actor, query.before.as_deref()).await
    {
        Ok(frames) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(frames))),
        Err(_) => Ok(not_accessible()),
    }
}
#[utoipa::path(tag = TAG, params(
    ("session" = String, Query), ("attachment" = String, Query)),
    responses((status = 200, description = "Stored image bytes", content_type = "image/png", body = AssistantImageBytes), (status = 404)))]
#[get("/my/device-assistant-session/image")]
pub async fn get_assistant_image(
    query: web::Query<AssistantImageQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some(id) = query.attachment.as_deref() else {
        return Ok(HttpResponse::NotFound().finish());
    };
    match crate::agent_image_store::read(&db, &query.session, &actor, Some(id), None).await {
        Ok((attachment, pixels)) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "private, no-store"))
            .insert_header(("X-Content-Type-Options", "nosniff"))
            .content_type(
                attachment
                    .frame
                    .media_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
            )
            .body(pixels)),
        Err(_) => Ok(HttpResponse::NotFound().finish()),
    }
}
#[utoipa::path(tag = TAG, params(
    ("session" = String, Query), ("attachment" = String, Query)),
    responses((status = 200, body = RestResponse<bool>)))]
#[post("/my/device-assistant-session/image/delete")]
pub async fn delete_assistant_image(
    query: web::Query<AssistantImageQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some(id) = query.attachment.as_deref() else {
        return Ok(not_accessible());
    };
    match crate::agent_image_store::delete(&db, &query.session, &actor, id).await {
        Ok(()) => Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(true))),
        Err(_) => Ok(not_accessible()),
    }
}
