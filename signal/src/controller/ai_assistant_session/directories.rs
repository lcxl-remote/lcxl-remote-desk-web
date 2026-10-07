//! Original-session directory decisions; parent input changes cannot retarget a child.
use super::*;

#[utoipa::path(tag = TAG, summary = "Select, approve, deny or revoke an original session directory",
    request_body = AiAssistantDirectoryControlBody, responses((status = 200, body = RestResponse<bool>)))]
#[post("/my/ai-assistant-session/directory-control")]
pub async fn control_ai_assistant_directory(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<AiAssistantDirectoryControlBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db();
    let store = SignalAgentSessionStore::new(db.clone());
    let body = body.into_inner();
    let Some((run, device)) = recovery::resolve(
        &store,
        &connection_map,
        &actor,
        &body.connection,
        Some(&body.session),
        None,
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let snapshot = store
        .read_assistant_snapshot_for_subject(&run, &actor, &device)
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "Directory request not found or not accessible",
            )
        })?
        .ok_or_else(|| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "Directory request not found or not accessible",
            )
        })?
        .session;
    let selector = if snapshot.subagents.task.is_some() {
        run.clone()
    } else {
        snapshot.client_conversation_id.ok_or_else(|| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Directory request has no owner context",
            )
        })?
    };
    let update = desk_agent_protocol::ai_assistant::AiAssistantObjectContextUpdate {
        conversation_id: selector.clone(),
        client_request_id: body.client_request_id,
        operation: body.operation.into(),
    };
    update.validate().map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "Invalid directory control",
        )
    })?;
    let subject = desk_diagnose_core::file_scope::FileScopeSubject {
        actor_id: actor,
        device_id: device,
        conversation_id: run,
    };
    let event_store = SignalAgentSessionStore::new(db.clone()).with_client_metadata(
        Some(selector),
        desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
    );
    let mutation = if let desk_agent_protocol::ai_assistant::AiAssistantObjectContextOperation::SelectDirectory { path, .. } = &update.operation {
        use desk_diagnose_core::file_scope::transaction;
        if let Some(receipt) = event_store.read_file_scope_receipt(&subject, &update.conversation_id, &update.client_request_id)
            .await.map_err(|_| DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, "Directory request changed"))? {
            transaction::match_owner_selection(&receipt, &update, &subject).map_err(|_| DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED, "Directory selection conflicts with its original request"))?;
            return Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(receipt.changed)));
        }
        let resolved = crate::remote_tool_edge::directory::resolve_candidate(&connection_map, &body.connection,
            &subject.actor_id, &subject.device_id, path).await.map_err(|_| DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED, "Directory target unavailable or candidate changed"))?;
        transaction::owner_selection(&update, subject, resolved).map_err(|_| DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED, "Invalid directory selection"))?
    } else {
        desk_diagnose_core::file_scope::transaction::from_owner_decision(&update, subject)
            .ok_or_else(|| DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, "Invalid directory control"))?
    };
    let receipt = event_store
        .update_file_scope(&mutation, chrono::Utc::now())
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Directory request or source changed; refresh and retry",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(receipt.changed)))
}
