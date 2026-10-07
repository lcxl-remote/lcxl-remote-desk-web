//! Durable main stop; child scope is resolved inside the single writer transaction.
use super::*;

#[utoipa::path(tag = TAG, summary = "Stop main generation and optionally all unfinished subagents",
    request_body = AiAssistantStopBody, responses((status = 200, body = RestResponse<AiAssistantStopResult>)))]
#[post("/my/ai-assistant-session/stop")]
pub async fn stop_ai_assistant_session(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<AiAssistantStopBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some((root, device)) = recovery::resolve(
        &SignalAgentSessionStore::new(db.clone()),
        &connection_map,
        &actor,
        &body.connection,
        body.session.as_deref(),
        body.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let outcome = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .stop_for_owner(&root, &actor, &device, &body.control)
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Conversation or subagent state changed; refresh the stop confirmation",
            )
        })?;
    for request in &outcome.cancel_request_ids {
        crate::ai_assistant_orchestrator::cancellation::cancel(SINGLE_ACCOUNT_USER_ID, request);
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(outcome.result)))
}
