//! Mounted only under the authenticated single-account owner REST scope.

use crate::{
    assistant_data_config::{self, WriteError},
    error::DeskSignalError,
};
use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::ai_assistant::memory::{
    AssistantDataControlReceipt, MemoryClearRequest, MemoryDeleteRequest, MemoryListRequest,
    MemoryListResult,
};
use desk_agent_protocol::ai_assistant::policy::{PolicyAssessment, PolicyView, PolicyWriteData};
use desk_agent_protocol::ai_assistant::source::{
    AssistantSourceReadRequest, AssistantSourceResolution,
};
use desk_agent_protocol::ai_assistant::user_instructions::{
    AssistantDataPolicyUpdate, AssistantDataSettingsUpdate, AssistantDataSettingsView,
    UserInstructions, UserInstructionsUpdate,
};
use desk_diagnose_core::ai_assistant::policy as policy_rules;
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::Serialize;

pub const TAG: &str = "AssistantData";
fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "Assistant data storage is unavailable",
        )
    })
}
fn reply<T: Serialize + utoipa::ToSchema>(value: T) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(value))
}
fn failure(error: WriteError) -> HttpResponse {
    let (code, revision) = match error {
        WriteError::Conflict { current_revision } => {
            (DeskErrorCode::REVISION_CONFLICT, Some(current_revision))
        }
        WriteError::Invalid => (DeskErrorCode::INVALID_PARAMS, None),
        WriteError::PolicyInvalid(assessment) => return policy_assessment(assessment),
        WriteError::Backend(_) => (DeskErrorCode::SYSTEM_ERROR, None),
    };
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<serde_json::Value>::failed_with_data(
            code,
            Some("Assistant data could not be saved; reload before retrying".into()),
            revision
                .map(|current_revision| serde_json::json!({"current_revision": current_revision})),
        ))
}

fn store_failure(error: crate::assistant_data_store::AssistantStoreError) -> HttpResponse {
    use crate::assistant_data_store::AssistantStoreError;
    let (code, revision) = match error {
        AssistantStoreError::Inaccessible | AssistantStoreError::SourceUnavailable => {
            (DeskErrorCode::PERMISSION_ERROR, None)
        }
        AssistantStoreError::Capacity => (DeskErrorCode::PRECONDITION_FAILED, None),
        AssistantStoreError::Invalid(_) => (DeskErrorCode::INVALID_PARAMS, None),
        AssistantStoreError::Stale { current_revision } => {
            (DeskErrorCode::REVISION_CONFLICT, Some(current_revision))
        }
        AssistantStoreError::Backend(_) => (DeskErrorCode::SYSTEM_ERROR, None),
    };
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<serde_json::Value>::failed_with_data(
            code,
            Some("Assistant data could not be accessed; reload before retrying".into()),
            revision
                .map(|current_revision| serde_json::json!({"current_revision": current_revision})),
        ))
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<AssistantDataSettingsView>)))]
#[get("/my/ai-assistant-settings")]
pub async fn get_assistant_data_settings() -> Result<HttpResponse, DeskSignalError> {
    let db = db()?;
    Ok(reply(AssistantDataSettingsView {
        settings: assistant_data_config::settings(db).await,
        policy: assistant_data_config::policy(db).await,
    }))
}
#[utoipa::path(tag = TAG, request_body = AssistantDataSettingsUpdate, responses((status = 200, body = RestResponse<AssistantDataSettingsView>)))]
#[post("/my/ai-assistant-settings")]
pub async fn update_assistant_data_settings(
    body: web::Json<AssistantDataSettingsUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = db()?;
    Ok(
        match assistant_data_config::update_settings(db, &body).await {
            Ok(settings) => reply(AssistantDataSettingsView {
                settings,
                policy: assistant_data_config::policy(db).await,
            }),
            Err(error) => failure(error),
        },
    )
}
#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<UserInstructions>)))]
#[get("/my/ai-assistant-user-instructions")]
pub async fn get_assistant_user_instructions() -> Result<HttpResponse, DeskSignalError> {
    Ok(reply(assistant_data_config::instructions(db()?).await))
}
#[utoipa::path(tag = TAG, request_body = UserInstructionsUpdate, responses((status = 200, body = RestResponse<UserInstructions>)))]
#[post("/my/ai-assistant-user-instructions")]
pub async fn update_assistant_user_instructions(
    body: web::Json<UserInstructionsUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match assistant_data_config::update_instructions(db()?, &body).await {
            Ok(value) => reply(value),
            Err(error) => failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, params(
    ("limit" = Option<u32>, Query), ("scope" = Option<String>, Query), ("include_superseded" = Option<bool>, Query),
    ("cursor" = Option<String>, Query), ("memory_state_generation" = Option<u64>, Query), ("control_generation" = Option<u64>, Query)
), responses((status = 200, body = RestResponse<MemoryListResult>)))]
#[get("/my/ai-assistant-memories")]
pub async fn get_assistant_memories(
    query: web::Query<MemoryListRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::assistant_data_store::memories(
            db()?,
            &query,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, request_body = MemoryDeleteRequest, responses((status = 200, body = RestResponse<AssistantDataControlReceipt>)))]
#[post("/my/ai-assistant-memory/delete")]
pub async fn delete_assistant_memory(
    body: web::Json<MemoryDeleteRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::assistant_data_store::forget_memory(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, request_body = MemoryClearRequest, responses((status = 200, body = RestResponse<AssistantDataControlReceipt>)))]
#[post("/my/ai-assistant-memory/clear")]
pub async fn clear_assistant_memories(
    body: web::Json<MemoryClearRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::assistant_data_store::clear_memories(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, params(("source_id" = String, Query)), responses((status = 200, body = RestResponse<AssistantSourceResolution>)))]
#[get("/my/ai-assistant-source")]
pub async fn get_assistant_source(
    query: web::Query<AssistantSourceReadRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::assistant_data_store::owner_source(
            db()?,
            &query.source_id,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<PolicyView>)))]
#[get("/admin/system/ai-assistant-data")]
pub async fn get_assistant_data_policy() -> Result<HttpResponse, DeskSignalError> {
    let policy = assistant_data_config::policy(db()?).await;
    let issues = policy_rules::warnings(&policy);
    Ok(reply(PolicyView { policy, issues }))
}
#[utoipa::path(tag = TAG, request_body = AssistantDataPolicyUpdate, responses((status = 200, body = RestResponse<PolicyWriteData>)))]
#[post("/admin/system/ai-assistant-data")]
pub async fn update_assistant_data_policy(
    body: web::Json<AssistantDataPolicyUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match assistant_data_config::update_policy(db()?, &body).await {
            Ok(value) => reply(value),
            Err(error) => failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, params(
    ("source_id" = String, Query), ("revision" = u64, Query),
    ("view" = Option<String>, Query), ("cursor" = Option<String>, Query)
), responses((status = 200, body = RestResponse<desk_agent_protocol::ai_assistant::history_window::AssistantHistoryWindow>)))]
#[get("/my/ai-assistant-history/window")]
pub async fn get_assistant_history_window(
    query: web::Query<
        desk_agent_protocol::ai_assistant::history_window::AssistantHistoryWindowRequest,
    >,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::assistant_data_store::owner_history_window(
            db()?,
            &query,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, request_body = desk_agent_protocol::ai_assistant::owner_history::AssistantHistorySearchRequest,
    responses((status = 200, body = RestResponse<desk_agent_protocol::ai_assistant::owner_history::AssistantHistorySearchResult>)))]
#[post("/my/ai-assistant-history/search")]
pub async fn search_assistant_history(
    body: web::Json<
        desk_agent_protocol::ai_assistant::owner_history::AssistantHistorySearchRequest,
    >,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::assistant_data_store::owner_history_search(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, params(("limit" = Option<u32>, Query), ("cursor" = Option<String>, Query)),
    responses((status = 200, body = RestResponse<desk_agent_protocol::ai_assistant::owner_history::AssistantHistoryConversations>)))]
#[get("/my/ai-assistant-history/conversations")]
pub async fn list_retained_assistant_conversations(
    body: web::Query<
        desk_agent_protocol::ai_assistant::owner_history::AssistantHistoryConversationsRequest,
    >,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::assistant_data_store::owner_history_conversations(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<desk_agent_protocol::ai_assistant::processing::AssistantProcessingStatus>)))]
#[get("/my/ai-assistant-processing")]
pub async fn get_assistant_processing() -> Result<HttpResponse, DeskSignalError> {
    Ok(match crate::assistant_data_store::processing(db()?).await {
        Ok(value) => reply(value),
        Err(error) => store_failure(error),
    })
}

fn policy_assessment(assessment: PolicyAssessment) -> HttpResponse {
    if assessment.has_errors() {
        HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::failed_with_data(
                DeskErrorCode::INVALID_PARAMS,
                Some("Assistant policy contains invalid values".into()),
                Some(assessment),
            ))
    } else {
        reply(assessment)
    }
}

#[cfg(test)]
mod tests;
