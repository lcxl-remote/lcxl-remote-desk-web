use super::*;
use crate::agent_attachment_store as store;
use desk_signal_facade::controller::ai_assistant_attachment::*;

#[utoipa::path(tag = TAG, params(
    ("session" = String, Query), ("before" = Option<String>, Query)),
    responses((status = 200, body = RestResponse<AttachmentListDto>)))]
#[get("/my/ai-assistant-session/attachments")]
pub async fn list_assistant_attachments(
    query: web::Query<AttachmentQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let rows = match store::list(&db, &query.session, &actor, query.before.as_deref()).await {
        Ok(rows) => rows,
        Err(_) => return Ok(not_accessible()),
    };
    let used_bytes = match store::usage(&db, &query.session, &actor).await {
        Ok(used) => used,
        Err(_) => return Ok(not_accessible()),
    };
    let cursor = (rows.len() == 32).then(|| rows.last().unwrap().attachment_id.clone());
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(AttachmentListDto {
            attachments: rows.into_iter().map(Into::into).collect(),
            cursor,
            used_bytes,
            capacity_bytes: desk_diagnose_core::conversation_attachment::MAX_SESSION_BYTES,
        })))
}

#[utoipa::path(tag = TAG, params(
    ("session" = String, Query), ("attachment" = String, Query)),
    responses((status = 200, description = "Attachment bytes or a JSON business error", content_type = "application/octet-stream", body = AttachmentBytes)))]
#[get("/my/ai-assistant-session/attachment")]
pub async fn get_assistant_attachment(
    query: web::Query<AttachmentQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some(id) = query.attachment.as_deref() else {
        return Ok(not_accessible());
    };
    match store::read(&db, &query.session, &actor, id, true).await {
        Ok(part) => {
            let extension = match part.metadata.kind {
                desk_diagnose_core::conversation_attachment::ContentKind::Json => "json",
                desk_diagnose_core::conversation_attachment::ContentKind::Text => "txt",
                desk_diagnose_core::conversation_attachment::ContentKind::Image => "image",
            };
            Ok(HttpResponse::Ok()
                .insert_header(("Cache-Control", "private, no-store"))
                .insert_header(("X-Content-Type-Options", "nosniff"))
                .insert_header(("Content-Security-Policy", "default-src 'none'; sandbox"))
                .insert_header((
                    "Content-Disposition",
                    format!("attachment; filename=attachment.{extension}"),
                ))
                .insert_header(("X-Assistant-Attachment", "content"))
                .content_type("application/octet-stream")
                .body(part.content))
        }
        Err(_) => Ok(not_accessible()),
    }
}

#[utoipa::path(tag = TAG, request_body = DeleteAttachments,
    responses((status = 200, body = RestResponse<bool>)))]
#[post("/my/ai-assistant-session/attachments/delete")]
pub async fn delete_assistant_attachments(
    body: web::Json<DeleteAttachments>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    match store::delete(&db, &body.session, &actor, &body.attachment_ids).await {
        Ok(()) => Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(true))),
        Err(_) => Ok(not_accessible()),
    }
}

#[utoipa::path(tag = TAG, request_body = ReadAttachment,
    responses((status = 200, body = RestResponse<AttachmentPageDto>)))]
#[post("/my/ai-assistant-session/attachment/read")]
pub async fn read_assistant_attachment(
    body: web::Json<ReadAttachment>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let part = match store::read(&db, &body.session, &actor, &body.attachment_id, false).await {
        Ok(part) => part,
        Err(_) => return Ok(not_accessible()),
    };
    if body.queries.is_some() && (body.start_line.is_some() || body.end_line.is_some()) {
        return Ok(HttpResponse::Ok().json(RestResponse::failed(
            DeskErrorCode::INVALID_PARAMS,
            "Search and line ranges cannot be combined".into(),
        )));
    }
    let page = match desk_diagnose_core::conversation_attachment::read::read_page(
        &part.metadata,
        &part.content,
        &body.request(),
    ) {
        Ok(page) => page,
        Err(error) => {
            return Ok(HttpResponse::Ok().json(RestResponse::failed(
                DeskErrorCode::INVALID_PARAMS,
                error.message,
            )));
        }
    };
    // Invalid queries never count as successful content access.
    if store::read(&db, &body.session, &actor, &body.attachment_id, true)
        .await
        .is_err()
    {
        return Ok(not_accessible());
    }
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(AttachmentPageDto::from(
            page,
        ))))
}
