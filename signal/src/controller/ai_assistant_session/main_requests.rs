use super::*;
use desk_assistant_data_store::{AssistantStoreError, main_request};
use desk_diagnose_core::subagent::valid_id;

#[utoipa::path(
    tag = TAG,
    summary = "Read assistant queue entries or an individual durable receipt",
    params(
        ("connection" = Option<String>, Query, description = "Device connection for an original conversation intent"),
        ("conversation" = Option<String>, Query, description = "Client conversation intent"),
        ("session" = Option<String>, Query, description = "Opaque assistant session id"),
        ("request" = Option<String>, Query, description = "Durable request id"),
    ),
    responses((status = 200, body = RestResponse<AiAssistantMainRequestsDto>)),
)]
#[get("/my/ai-assistant-session/requests")]
pub async fn list_ai_assistant_main_requests(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<AiAssistantMainRequestQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    if query.request.as_deref().is_some_and(|id| !valid_id(id)) {
        return Ok(not_accessible());
    }
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    use AiAssistantSnapshotSelection::{DeviceIntent, PersonalIntent, Saved};
    let session_id = match query.session_selector() {
        Some(Saved { session }) => session.to_owned(),
        Some(PersonalIntent { conversation }) => derive_assistant_conversation_key(
            &actor,
            AssistantConversationScope::Personal,
            conversation,
        ),
        Some(DeviceIntent {
            connection,
            conversation,
        }) => {
            let Some((id, _)) = recovery::resolve(
                &SignalAgentSessionStore::new(db.clone()),
                &connection_map,
                &actor,
                connection,
                None,
                Some(conversation),
            )
            .await?
            else {
                return Ok(not_accessible());
            };
            id
        }
        _ => return Ok(not_accessible()),
    };
    response(
        main_request::receipts(db, &actor, &session_id, query.request.as_deref())
            .await
            .map(|items| AiAssistantMainRequestsDto { session_id, items }),
    )
}

#[utoipa::path(
    tag = TAG,
    summary = "Cancel a queued assistant request",
    request_body = AiAssistantMainRequestCancelBody,
    responses((status = 200, body = RestResponse<MainRequestReceipt>)),
)]
#[post("/my/ai-assistant-session/requests/cancel")]
pub async fn cancel_ai_assistant_main_request(
    body: web::Json<AiAssistantMainRequestCancelBody>,
) -> Result<HttpResponse, DeskSignalError> {
    if !valid_id(&body.session) || !valid_id(&body.request_id) {
        return Ok(not_accessible());
    }
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let txn = crate::db::begin_write(
        crate::db::get_db(),
        crate::entity::agent_main_request::Entity,
    )
    .await?;
    let result = main_request::cancel_owner_request(
        &txn,
        &actor,
        &body.session,
        &body.request_id,
        chrono::Utc::now().timestamp_millis(),
    )
    .await;
    if result.is_ok() {
        txn.commit().await?;
    } else {
        txn.rollback().await?;
    }
    response(result)
}

#[utoipa::path(
    tag = TAG,
    summary = "Resume assistant queue processing after a main stop",
    request_body = AiAssistantMainRequestResumeBody,
    responses((status = 200, body = RestResponse<AiAssistantMainRequestResumeDto>)),
)]
#[post("/my/ai-assistant-session/requests/resume")]
pub async fn resume_ai_assistant_main_requests(
    body: web::Json<AiAssistantMainRequestResumeBody>,
) -> Result<HttpResponse, DeskSignalError> {
    if !valid_id(&body.session) {
        return Ok(not_accessible());
    }
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db().clone();
    let Some(device) = SignalAgentSessionStore::new(db.clone())
        .recovery_device(&body.session, &actor)
        .await
        .ok()
        .flatten()
    else {
        return Ok(not_accessible());
    };
    let control_revision = crate::agent_subagent_store::SubAgentStore::new(db)
        .resume_main_queue_for_owner(
            &body.session,
            &actor,
            &device,
            body.expected_control_revision,
        )
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Conversation changed; refresh before resuming",
            )
        })?;
    response(Ok(AiAssistantMainRequestResumeDto { control_revision }))
}

fn response<T: serde::Serialize + utoipa::ToSchema>(
    result: Result<T, AssistantStoreError>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match result {
        Ok(value) => HttpResponse::Ok().json(RestResponse::succeed_with_data(value)),
        Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
        Err(AssistantStoreError::Stale { .. }) => {
            HttpResponse::Ok().json(RestResponse::<()>::failed(
                DeskErrorCode::PRECONDITION_FAILED,
                "The assistant request has already left the queue".into(),
            ))
        }
        Err(_) => not_accessible(),
    })
}
