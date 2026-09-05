//! Browser-facing access to an OSS Device Assistant conversation.
//!
//! The browser supplies the same target connection and conversation intent used
//! by the signaling turn. The server resolves the target's authenticated client
//! id and re-derives the subject-namespaced conversation key. Snapshot reads and
//! background stops can also locate an offline target using an explicit original
//! session id; the subsequent store operation still checks the original actor and
//! device. This recovery selector is not authority to execute a new action.

use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::device_assistant::DeviceAssistantAsk;
use desk_diagnose_core::conversation_key::derive_conversation_key;
use desk_diagnose_core::{
    capability_availability::project_capability_availability,
    device_assistant::{device_assistant_provider_registry, provider_readiness_reports},
};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

use crate::agent_session_store::{PermissionGrantIssuanceContext, SignalAgentSessionStore};
use crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
use crate::error::DeskSignalError;

pub const TAG: &str = "DeviceAssistantSession";
pub(crate) mod recovery;
pub use desk_signal_facade::controller::device_assistant_session::*;

fn not_accessible() -> HttpResponse {
    HttpResponse::Ok().json(RestResponse::<()>::failed(
        DeskErrorCode::PERMISSION_ERROR,
        "Device Assistant session not found or not accessible".to_string(),
    ))
}

#[utoipa::path(
    tag = TAG,
    summary = "Read a Device Assistant conversation snapshot (browser view)",
    params(
        ("connection" = String, Query, description = "Target connection id"),
        ("conversation" = Option<String>, Query, description = "Client conversation intent"),
        ("session" = Option<String>, Query, description = "Opaque session id from history list"),
        ("message_before" = Option<String>, Query, description = "Exclusive older-message cursor"),
        ("message_limit" = Option<usize>, Query, description = "Message page size, 1 through 100"),
    ),
    responses((status = 200, description = "Conversation snapshot, or a uniform \
        not-found/not-accessible response", body = RestResponse<DeviceAssistantSessionSnapshotDto>)),
)]
#[get("/my/device-assistant-session")]
pub async fn get_device_assistant_session(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<DeviceAssistantSessionQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let store = SignalAgentSessionStore::new(crate::db::get_db().clone());
    let Some((session_id, target_audience)) = recovery::resolve(
        &store,
        &connection_map,
        &actor_id,
        &query.connection,
        query.session.as_deref(),
        query.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let snapshot = store
        .read_assistant_snapshot_for_subject(&session_id, &actor_id, &target_audience)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    match snapshot {
        Some(snapshot) => {
            let background_tasks = snapshot.background_tasks;
            let capability_grants = snapshot.capability_grants;
            let snapshot = snapshot.session;
            let evidence_summary =
                build_evidence_summary(&snapshot.messages, &snapshot.context_attachments);
            let visual_evidence = desk_diagnose_core::visual_evidence::durable_projection(
                &snapshot.visual_evidence,
                u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            );
            let message_page = project_snapshot_message_page(
                snapshot.messages,
                query.message_before.as_deref(),
                query.message_limit,
            )
            .map_err(|message| {
                DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, message)
            })?;
            Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
                DeviceAssistantSessionSnapshotDto {
                    terminal_error: snapshot.terminal_error,
                    session_id,
                    context_usage: snapshot.context_usage.map(Into::into),
                    seq: snapshot.seq,
                    active: snapshot.active,
                    request_id: snapshot.request_id,
                    active_execution_generation: snapshot.active_execution_generation,
                    unresolved_outcome: snapshot.unresolved_action.map(|action| {
                        UnknownOutcomeDto {
                            work_id: action.work_id,
                            action_request_id: action.action_request_id,
                            execution_id: action.execution_id,
                            work_kind: action.kind.as_str().to_string(),
                        }
                    }),
                    latest_input_seq: snapshot.latest_input_seq,
                    input_revision: snapshot.input_revision,
                    handled_input_seq: snapshot.handled_input_seq,
                    task_status_projection: snapshot.task_status_projection.map(Into::into),
                    permission_requests: snapshot
                        .permission_requests
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    background_tasks: background_tasks.into_iter().map(Into::into).collect(),
                    capability_grants: capability_grants.into_iter().map(Into::into).collect(),
                    evidence_summary,
                    visual_evidence,
                    messages: message_page.messages,
                    message_page: message_page.page,
                    context_notices: snapshot
                        .context_notices
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    context_attachments: snapshot
                        .context_attachments
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                },
            )))
        }
        None => Ok(not_accessible()),
    }
}

#[utoipa::path(
    tag = TAG,
    summary = "Manually dispose one exact unknown Device Assistant action",
    request_body = UnknownOutcomeDispositionBody,
    responses((status = 200, description = "Owner disposition recorded; no retry or grant restoration occurs", body = RestResponse<UnknownOutcomeDispositionResponse>)),
)]
#[post("/my/device-assistant-session/outcome-unknown/dispose")]
pub async fn dispose_device_assistant_unknown_outcome(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<UnknownOutcomeDispositionBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&body.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let session_id = match (
        body.session.as_deref().filter(|value| !value.is_empty()),
        body.conversation.as_deref(),
    ) {
        (Some(session_id), _) => session_id.to_string(),
        (None, Some(conversation)) => {
            derive_conversation_key(&actor_id, &target_audience, Some(conversation), "")
        }
        (None, None) => return Ok(not_accessible()),
    };
    let session_store = SignalAgentSessionStore::new(crate::db::get_db().clone());
    let Some(snapshot) = session_store
        .read_snapshot_for_subject(&session_id, &actor_id, &target_audience)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?
    else {
        return Ok(not_accessible());
    };
    if !snapshot.unresolved_action.as_ref().is_some_and(|action| {
        action.work_id == body.work_id && action.execution_id == body.execution_id
    }) {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "the exact unknown action is no longer pending disposition",
        ));
    }

    let now_dt = chrono::Utc::now();
    let now_unix_ms = u64::try_from(now_dt.timestamp_millis()).map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "system clock predates Unix epoch",
        )
    })?;
    use crate::capability_grant_store::CapabilityManualDispositionResult;
    match crate::capability_grant_store::SignalCapabilityGrantStore::new(
        crate::db::get_db().clone(),
    )
    .manually_dispose_unknown_for_subject(
        body.work_id,
        &body.execution_id,
        &session_id,
        &actor_id,
        &target_audience,
        now_unix_ms,
    )
    .await
    .map_err(|error| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("record unknown-action disposition: {error}"),
        )
    })? {
        CapabilityManualDispositionResult::Applied
        | CapabilityManualDispositionResult::AlreadyResolved => {}
        CapabilityManualDispositionResult::SubjectMismatch
        | CapabilityManualDispositionResult::StateMismatch => {
            return Err(DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "the exact unknown action cannot be manually disposed",
            ));
        }
    }

    let now = now_dt.to_rfc3339();
    let disposition = session_store
        .manually_dispose_unknown_for_subject(
            &session_id,
            &actor_id,
            &target_audience,
            body.work_id,
            &body.execution_id,
            &now,
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    if disposition == crate::agent_session_store::EventAppend::Busy {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::ACTION_NEED_RETRY,
            "the conversation is still active; retry the disposition",
        ));
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        UnknownOutcomeDispositionResponse { disposed: true },
    )))
}

#[utoipa::path(
    tag = TAG,
    summary = "Revoke one owner-scoped Device Assistant capability grant",
    request_body = CapabilityGrantRevokeBody,
    responses((status = 200, description = "Revoked grant metadata; no work is dispatched", body = RestResponse<CapabilityGrantDto>)),
)]
#[post("/my/device-assistant-session/capability-grant/revoke")]
pub async fn revoke_device_assistant_capability_grant(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<CapabilityGrantRevokeBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&body.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let session_id = match (
        body.session.as_deref().filter(|value| !value.is_empty()),
        body.conversation.as_deref(),
    ) {
        (Some(session_id), _) => session_id.to_string(),
        (None, Some(conversation)) => {
            derive_conversation_key(&actor_id, &target_audience, Some(conversation), "")
        }
        (None, None) => return Ok(not_accessible()),
    };
    let session = SignalAgentSessionStore::new(crate::db::get_db().clone())
        .read_snapshot_for_subject(&session_id, &actor_id, &target_audience)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    if session.is_none() {
        return Ok(not_accessible());
    }
    let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "system clock predates Unix epoch",
        )
    })?;
    let grant =
        crate::capability_grant_store::SignalCapabilityGrantStore::new(crate::db::get_db().clone())
            .revoke(
                &body.grant_id,
                &actor_id,
                &target_audience,
                now_unix_ms,
                &body.reason,
            )
            .await
            .map_err(|error| {
                DeskSignalError::new_custom_error(
                    DeskErrorCode::PRECONDITION_FAILED,
                    &error.to_string(),
                )
            })?;
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(CapabilityGrantDto::from(
            grant,
        ))),
    )
}

#[utoipa::path(
    tag = TAG,
    summary = "Decide one pending Device Assistant permission request",
    request_body = PermissionDecisionBody,
    responses((status = 200, description = "Durable decision projection; no tool is dispatched", body = RestResponse<PermissionDecisionResponse>)),
)]
#[post("/my/device-assistant-session/permission-decision")]
pub async fn decide_device_assistant_permission(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<PermissionDecisionBody>,
) -> Result<HttpResponse, DeskSignalError> {
    if !crate::device_assistant_gate::global_device_assistant_gate().is_enabled() {
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "Device Assistant is disabled on this device".to_string(),
        )));
    }
    decide_permission_on(crate::db::get_db(), connection_map, body).await
}

pub(crate) async fn decide_permission_on(
    db: &sea_orm::DatabaseConnection,
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<PermissionDecisionBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&body.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let store = SignalAgentSessionStore::new(db.clone());
    let session_id = match (
        body.session.as_deref().filter(|value| !value.is_empty()),
        body.conversation.as_deref(),
    ) {
        (Some(session_id), _) => session_id.to_string(),
        (None, Some(conversation)) => {
            derive_conversation_key(&actor_id, &target_audience, Some(conversation), "")
        }
        (None, None) => return Ok(not_accessible()),
    };
    let decisions: Vec<desk_diagnose_core::dynamic_run::PermissionDecisionItem> =
        body.items.clone().into_iter().map(Into::into).collect();
    if let Some(state) = store
        .replay_permission_decision(
            &session_id,
            &actor_id,
            &target_audience,
            &body.request_id,
            &decisions,
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &error.message)
        })?
    {
        return Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
            PermissionDecisionResponse {
                state: state.into(),
            },
        )));
    }
    let now_dt = chrono::Utc::now();
    let now_unix_ms = u64::try_from(now_dt.timestamp_millis()).map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "system clock predates Unix epoch",
        )
    })?;
    let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
        .get_fresh(&body.connection, now_dt)
        .ok_or_else(|| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "target capability readiness is unavailable; refresh and retry",
            )
        })?;
    let readiness_revision = readiness.readiness.revision;
    let implicit_fresh_object_refs = readiness
        .readiness
        .context_references
        .iter()
        .filter(|reference| {
            matches!(
                reference.object_ref.object_kind,
                desk_agent_protocol::computer_use::ObjectKind::BrowserSurface
                    | desk_agent_protocol::computer_use::ObjectKind::Application
                    | desk_agent_protocol::computer_use::ObjectKind::Range
                    | desk_agent_protocol::computer_use::ObjectKind::Document
                    | desk_agent_protocol::computer_use::ObjectKind::Slide
            )
        })
        .map(|reference| reference.object_ref.clone())
        .collect::<Vec<_>>();
    let reports = provider_readiness_reports(&readiness.readiness).map_err(|error| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            &format!("invalid target capability readiness: {error}"),
        )
    })?;
    let search_config = crate::web_search_config::read(db).await?;
    let registry =
        device_assistant_provider_registry().with_web_search_binding(search_config.binding());
    let registry = match crate::command_policy::current(
        db,
        connection_map.as_ref(),
        &body.connection,
        &actor_id,
    )
    .await
    {
        Ok(policy) => registry.with_command_policy(policy),
        Err(_) => registry,
    };
    let inventory = project_capability_availability(
        &registry,
        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
        now_unix_ms,
        crate::device_assistant_orchestrator::oss_central_capability_readiness(
            search_config.configured(),
        ),
        reports,
    )
    .map_err(|error| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            &format!("invalid target capability inventory: {error}"),
        )
    })?;
    let now = now_dt.to_rfc3339();
    let decision = store
        .decide_permission_request(
            &session_id,
            &actor_id,
            &target_audience,
            &body.request_id,
            decisions,
            PermissionGrantIssuanceContext {
                surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision,
                now_unix_ms,
                implicit_fresh_object_refs: &implicit_fresh_object_refs,
            },
            &now,
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &error.message)
        })?;
    if decision.newly_recorded
        && let Some(snapshot) = store
            .read_snapshot_for_subject(&session_id, &actor_id, &target_audience)
            .await
            .map_err(|error| {
                DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
            })?
        && let Some(question) =
            desk_diagnose_core::permission_resume::latest_user_requirement(&snapshot.messages)
                .map(|message| message.text.clone())
    {
        let resume_request_id = format!("permission-resume-{}", body.request_id);
        let resume_ask = DeviceAssistantAsk {
            question,
            client_message_id: resume_request_id.clone(),
            conversation_id: snapshot.client_conversation_id,
            locale: None,
            // The orchestrator reloads the original input selection. Current
            // active context is not authority to expand a permission resume.
            selected_capability_ids: Vec::new(),
            selected_attachment_ids: Vec::new(),
        };
        let resume_connections = connection_map.clone();
        let resume_db = db.clone();
        let resume_target_connection = body.connection.clone();
        let resume_target_audience = target_audience.clone();
        let resume_session_id = session_id.clone();
        let resume_permission_request_id = body.request_id.clone();
        actix_web::rt::spawn(async move {
            crate::device_assistant_orchestrator::resume_after_permission_decision(
                resume_connections,
                resume_db,
                resume_request_id,
                resume_target_connection,
                SINGLE_ACCOUNT_USER_ID,
                resume_target_audience,
                resume_session_id,
                resume_permission_request_id,
                resume_ask,
            )
            .await;
        });
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        PermissionDecisionResponse {
            state: decision.state.into(),
        },
    )))
}

#[utoipa::path(
    tag = TAG,
    summary = "Request cancellation of an authorized Device Assistant background task",
    request_body = BackgroundCancelBody,
    responses((status = 200, description = "Durable cancellation intent; Provider delivery is asynchronous", body = RestResponse<BackgroundTaskDto>)),
)]
#[post("/my/device-assistant-session/background-task/cancel")]
pub async fn cancel_device_assistant_background_task(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<BackgroundCancelBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some((session_id, target_audience)) = recovery::resolve(
        &SignalAgentSessionStore::new(crate::db::get_db().clone()),
        &connection_map,
        &actor_id,
        &body.connection,
        body.session.as_deref(),
        body.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let original =
        crate::capability_grant_store::SignalCapabilityGrantStore::new(crate::db::get_db().clone())
            .request_computer_background_cancel(
                &body.task_id,
                &session_id,
                &actor_id,
                &target_audience,
                &body.request_id,
                &body.reason,
            )
            .await
            .map_err(|error| {
                DeskSignalError::new_custom_error(
                    DeskErrorCode::PRECONDITION_FAILED,
                    &error.to_string(),
                )
            })?;
    if let Some(record) = original {
        return Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
            BackgroundTaskDto::from(record),
        )));
    }
    let store = crate::agent_background_task_store::SignalBackgroundTaskStore::new(
        crate::db::get_db().clone(),
    );
    store
        .request_cancel_for_subject(
            &body.task_id,
            &session_id,
            &actor_id,
            &target_audience,
            &body.request_id,
            &body.reason,
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                &error.to_string(),
            )
        })?;
    let record = store
        .load(&body.task_id)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
        .ok_or_else(|| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "background task was not found or not accessible",
            )
        })?;
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(BackgroundTaskDto::from(
            record,
        ))),
    )
}

#[utoipa::path(
    tag = TAG,
    summary = "List recent Device Assistant conversations for a device",
    params(
        ("connection" = String, Query, description = "Target connection id"),
        ("limit" = Option<u64>, Query, description = "Maximum rows, default 30 and capped at 100"),
    ),
    responses((status = 200, description = "Authorized recent Device Assistant sessions",
        body = RestResponse<DeviceAssistantSessionListDto>)),
)]
#[get("/my/device-assistant-sessions")]
pub async fn list_device_assistant_sessions(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<DeviceAssistantSessionListQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&query.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let limit = query.limit.unwrap_or(30).clamp(1, 100);
    let summaries = SignalAgentSessionStore::new(crate::db::get_db().clone())
        .list_device_assistant_sessions(&actor_id, &target_audience, limit)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    let sessions = summaries
        .into_iter()
        .map(|summary| DeviceAssistantSessionSummaryDto {
            session_id: summary.session_id,
            conversation_id: summary.client_conversation_id,
            first_question: summary.first_question,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            active: summary.active,
            message_count: summary.message_count,
        })
        .collect();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        DeviceAssistantSessionListDto { sessions },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::browser_control::{BrowserOrigin, BrowserOriginKind};
    use desk_agent_protocol::communication::{
        COMMUNICATION_SCHEMA_VERSION, CommunicationChannel, CommunicationDraftHandoff,
        CommunicationPrepareVerification, CommunicationSendAuthority, CommunicationSurfaceKind,
        CommunicationSurfaceRef, CommunicationSurfaceScope,
    };
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    };
    use desk_diagnose_core::chat::{ChatMessage, ChatRole, ToolCallRef};
    use desk_diagnose_core::context_attachment::{
        AttachmentBounds, AttachmentObjectRef, AttachmentState, CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        ContextAttachment, ContextAttachmentKind,
    };
    use desk_diagnose_core::session::AgentSessionSurface;

    #[test]
    fn snapshot_message_uses_the_manager_compatible_camel_case_shape() {
        let mut message = ChatMessage::text("m1", ChatRole::Assistant, "checking");
        message.tool_calls.push(ToolCallRef {
            id: "c1".into(),
            name: "read_system_info".into(),
            arguments_json: "{}".into(),
        });
        message.image_data_url = Some("data:image/jpeg;base64,AQID".into());
        let value = serde_json::to_value(SnapshotMessageDto::from(message)).unwrap();
        assert_eq!(value["id"], "m1");
        assert_eq!(value["toolCalls"][0]["argumentsJson"], "{}");
        assert!(value.get("tool_calls").is_none());
        assert!(value.get("imageDataUrl").is_none());
        assert!(!value.to_string().contains("AQID"));

        let completion = ChatMessage::untrusted_output("done-1", "call-1", "task-1", "exit_code=0");
        let value = serde_json::to_value(SnapshotMessageDto::from(completion)).unwrap();
        assert_eq!(value["backgroundTaskId"], "task-1");
        assert!(value.get("background_task_id").is_none());
    }

    #[test]
    fn evidence_summary_is_bounded_metadata_only_and_checks_graph_integrity() {
        fn envelope(id: &str, sources: Vec<String>, digest: char) -> DataEnvelope {
            DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: id.into(),
                content: ContentRef::ImmutableBlob {
                    blob_id: format!("{id}-blob"),
                    sha256: digest.to_string().repeat(64),
                    size_bytes: 7,
                    media_type: "application/json".into(),
                },
                provenance: DataProvenance {
                    source_provider_id: "test.provider".into(),
                    source_tool_name: "test_tool".into(),
                    source_object_id: None,
                    source_envelope_ids: sources,
                },
                digest_sha256: digest.to_string().repeat(64),
                sensitivity: Sensitivity::Sensitive,
                allowed_destinations: Vec::new(),
                retention: RetentionBoundary {
                    expires_at_unix_ms: None,
                    delete_with_run: false,
                },
            }
        }

        let mut user = ChatMessage::text(
            "user-1",
            ChatRole::User,
            "SECRET BODY C:\\private\\report.docx",
        );
        user.data_envelope = Some(envelope("user-envelope", Vec::new(), 'a'));
        let mut result = ChatMessage::tool_result(
            "result-1",
            "call-1",
            "SECRET TOOL OUTPUT C:\\private\\report.docx",
        );
        result.data_envelope = Some(envelope(
            "result-envelope",
            vec!["user-envelope".into()],
            'b',
        ));

        let summary = build_evidence_summary(&[user.clone(), result.clone()], &[]);
        assert!(summary.graph_complete);
        assert!(!summary.truncated);
        assert_eq!(summary.nodes.len(), 2);
        assert!(summary.missing_source_envelope_ids.is_empty());
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("SECRET"));
        assert!(!json.contains("private"));

        let handoff = CommunicationDraftHandoff {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff_id: "gmail-handoff-1".into(),
            run_id: "run-1".into(),
            surface: CommunicationSurfaceRef {
                channel: CommunicationChannel::Email,
                kind: CommunicationSurfaceKind::ChromeExtension,
                scope: CommunicationSurfaceScope::WebOrigin {
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: "mail.google.com".into(),
                        port: 443,
                    },
                },
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                adapter_id: "lcxl-browser-extension".into(),
                adapter_version: "0.1.0".into(),
                profile_id: "profile-1".into(),
                account_id: "gmail-web-current-profile".into(),
                revision: 1,
            },
            compose_id: "compose-1".into(),
            prepared_payload_sha256: "d".repeat(64),
            verification: CommunicationPrepareVerification::SemanticExact,
            readback_payload_sha256: Some("d".repeat(64)),
            send_authority: CommunicationSendAuthority::ManualOnly,
            send_payload_snapshot: None,
            handed_off_at_unix_ms: 42,
        };
        handoff.validate().unwrap();
        let mut handoff_message = ChatMessage::tool_result(
            "handoff-result",
            "gmail-call",
            serde_json::to_string(&handoff).unwrap(),
        );
        handoff_message.data_envelope = Some(envelope(
            "handoff-envelope",
            vec!["result-envelope".into()],
            'd',
        ));
        let with_handoff = build_evidence_summary(&[user, result, handoff_message], &[]);
        assert!(with_handoff.graph_complete);
        assert_eq!(with_handoff.handoff_receipts.len(), 1);
        assert_eq!(
            with_handoff.handoff_receipts[0].surface_kind,
            "chrome_extension"
        );
        assert_eq!(
            with_handoff.handoff_receipts[0].source_envelope_id,
            "handoff-envelope"
        );

        let orphan = ChatMessage {
            data_envelope: Some(envelope(
                "orphan-envelope",
                vec!["missing-envelope".into()],
                'c',
            )),
            ..ChatMessage::text("orphan", ChatRole::Assistant, "hidden")
        };
        let incomplete = build_evidence_summary(&[orphan], &[]);
        assert!(!incomplete.graph_complete);
        assert_eq!(
            incomplete.missing_source_envelope_ids,
            vec!["missing-envelope"]
        );
    }

    #[test]
    fn context_notice_uses_the_manager_compatible_compacted_shape() {
        let notice = desk_diagnose_core::model_context::ContextNotice::compacted("turn-7", 3, 19);
        let value = serde_json::to_value(ContextNoticeDto::from(notice)).unwrap();
        assert_eq!(value["kind"], "compacted");
        assert_eq!(value["checkpointGeneration"], 3);
        assert_eq!(value["coveredMessageCount"], 19);
    }

    #[test]
    fn attachment_snapshot_never_exposes_opaque_ref_or_egress_metadata() {
        let attachment = ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: "attachment-1".into(),
            client_request_id: "request-1".into(),
            actor_id: "owner".into(),
            device_id: "device".into(),
            surface: AgentSessionSurface::DeviceAssistant,
            kind: ContextAttachmentKind::InteractiveSession,
            object_ref: AttachmentObjectRef {
                opaque_token: "never-send-this-token".into(),
                object_incarnation: "worker-1".into(),
                source_provider_id: "desktop.ui".into(),
                source_capability_id: "desktop.ui.inspect".into(),
            },
            bounds: AttachmentBounds {
                max_bytes: 1024,
                max_objects: 16,
            },
            display_summary: "current interactive session".into(),
            created_at_unix_ms: 100,
            expires_at_unix_ms: 200,
            envelope: DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: "private-envelope".into(),
                content: ContentRef::EphemeralObservation {
                    observation_id: "private-observation".into(),
                    size_bytes: 1,
                    expires_at_unix_ms: 200,
                },
                provenance: DataProvenance {
                    source_provider_id: "desktop.ui".into(),
                    source_tool_name: "inspect_desktop_ui".into(),
                    source_object_id: Some("private-object".into()),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: "a".repeat(64),
                sensitivity: Sensitivity::Sensitive,
                allowed_destinations: vec![DestinationIdentity::Model {
                    connection_id: "private-connection".into(),
                    connection_revision: 1,
                    model_id: "private-model".into(),
                    profile_revision: 1,
                }],
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(200),
                    delete_with_run: false,
                },
            },
            state: AttachmentState::Active,
        };
        let active = ContextAttachmentDto::from_at(attachment.clone(), 150);
        assert_eq!(active.state, "active");
        let expired = ContextAttachmentDto::from_at(attachment.clone(), 200);
        assert_eq!(expired.state, "stale");
        assert_eq!(expired.stale_reason.as_deref(), Some("expired"));

        let encoded = serde_json::to_string(&ContextAttachmentDto::from(attachment)).unwrap();
        assert!(encoded.contains("desktop.ui.inspect"));
        for secret in [
            "never-send-this-token",
            "private-envelope",
            "private-observation",
            "private-connection",
            "private-model",
            &"a".repeat(64),
        ] {
            assert!(!encoded.contains(secret));
        }
    }
}
