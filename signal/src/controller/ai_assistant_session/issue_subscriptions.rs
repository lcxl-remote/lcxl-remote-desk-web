//! Owner settings for one workbench's external issue sources.

use super::*;
use crate::{config::connection::DatabaseConnection, entity::agent_session, issue_connections};
use actix_web::put;
use desk_agent_protocol::ai_assistant::issue_subscription::{
    IssueSubscriptionAccounts, IssueSubscriptionEnable, IssueSubscriptionSave,
    IssueSubscriptionView,
};
use desk_assistant_data_store::{
    AssistantStoreError,
    issue_authority::settings::{self, WriteResult},
    main_request::decode_workbench_coordinator,
};
use desk_diagnose_core::{
    issue_authority::IssueSubscription,
    native_ids::{self, Kind},
};
use desk_signal_facade::mcp_client::settings::IssueConnectionConfig;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

#[derive(serde::Deserialize)]
pub struct IssueSubscriptionsQuery {
    workbench: String,
}

#[derive(serde::Deserialize)]
pub struct IssueSubscriptionAccountsQuery {
    workbench: String,
    connection: String,
    login: Option<String>,
}

#[utoipa::path(tag = TAG, params(("workbench" = String, Query), ("connection" = String, Query), ("login" = Option<String>, Query)), responses((status = 200, body = RestResponse<IssueSubscriptionAccounts>)))]
#[get("/my/ai-assistant-session/issue-subscriptions/accounts")]
pub async fn get_ai_assistant_issue_subscription_accounts(
    session: Session,
    query: web::Query<IssueSubscriptionAccountsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    if !super::device_roster::owner(&session)? {
        return Ok(not_accessible());
    }
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db();
    let result = async {
        check_workbench(db, &actor, &query.workbench).await?;
        let connection = read_connection(db, &actor, &query.connection).await?;
        connection
            .subscription_accounts(query.login.as_deref())
            .await
            .map_err(|_| AssistantStoreError::SourceUnavailable)
    }
    .await;
    match result {
        Ok(accounts) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(accounts))),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(tag = TAG, params(("workbench" = String, Query)), responses((status = 200, body = RestResponse<Vec<IssueSubscriptionView>>)))]
#[get("/my/ai-assistant-session/issue-subscriptions")]
pub async fn list_ai_assistant_issue_subscriptions(
    session: Session,
    query: web::Query<IssueSubscriptionsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    if session
        .get_current_user::<CurrentUser>()
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
        .is_none()
    {
        return Ok(not_accessible());
    }
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db().clone();
    let result = async {
        check_workbench(&db, &actor, &query.workbench).await?;
        settings::list_on(&db, &actor, &query.workbench).await
    }
    .await;
    match result {
        Ok(items) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(items))),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(tag = TAG, request_body = IssueSubscriptionSave, responses((status = 200, body = RestResponse<IssueSubscriptionView>)))]
#[put("/my/ai-assistant-session/issue-subscriptions")]
pub async fn save_ai_assistant_issue_subscription(
    session: Session,
    body: web::Json<IssueSubscriptionSave>,
) -> Result<HttpResponse, DeskSignalError> {
    if session
        .get_current_user::<CurrentUser>()
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
        .is_none()
    {
        return Ok(not_accessible());
    }
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db().clone();
    write_response(save(&db, &actor, body.into_inner()).await)
}

#[utoipa::path(tag = TAG, params(("subscription_id" = String, Path)), request_body = IssueSubscriptionEnable, responses((status = 200, body = RestResponse<IssueSubscriptionView>)))]
#[put("/my/ai-assistant-session/issue-subscriptions/{subscription_id}/enabled")]
pub async fn set_ai_assistant_issue_subscription_enabled(
    session: Session,
    subscription_id: web::Path<String>,
    body: web::Json<IssueSubscriptionEnable>,
) -> Result<HttpResponse, DeskSignalError> {
    if session
        .get_current_user::<CurrentUser>()
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
        .is_none()
    {
        return Ok(not_accessible());
    }
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db().clone();
    write_response(set_enabled(&db, &actor, &subscription_id, body.into_inner()).await)
}

pub(crate) async fn save(
    db: &DatabaseConnection,
    actor: &str,
    body: IssueSubscriptionSave,
) -> Result<WriteResult, AssistantStoreError> {
    settings::validate_save(&body)?;
    check_workbench(db, actor, &body.workbench_id).await?;
    let id = if let Some(id) = &body.subscription_id {
        settings::current_for_edit(db, actor, id, body.expected_revision).await?;
        id.clone()
    } else {
        native_ids::stable(Kind::IssueSubscription, &uuid::Uuid::new_v4().to_string())
    };
    let connection = read_connection(db, actor, &body.connection_ref).await?;
    let (scope, filter) = connection
        .resolve_subscription(&body.owner, &body.repository, body.issue_number)
        .await
        .map_err(|_| AssistantStoreError::SourceUnavailable)?;
    if body
        .expected_outbound_account_id
        .is_some_and(|expected| expected != scope.account_id)
    {
        return Err(AssistantStoreError::SourceUnavailable);
    }
    let candidate = settings::candidate(actor.into(), id, body, scope, filter);
    commit(db, candidate, Some(&connection)).await
}

pub(crate) async fn set_enabled(
    db: &DatabaseConnection,
    actor: &str,
    id: &str,
    body: IssueSubscriptionEnable,
) -> Result<WriteResult, AssistantStoreError> {
    let mut candidate = settings::current_for_edit(db, actor, id, body.expected_revision).await?;
    let connection = if body.enabled {
        check_workbench(db, actor, &candidate.conversation_id).await?;
        let repository = &candidate.repository;
        let connection = read_connection(db, actor, &repository.connection_ref).await?;
        let (scope, filter) = connection
            .resolve_subscription(
                &repository.owner,
                &repository.repository,
                candidate.issue_filter.map(|filter| filter.issue_number),
            )
            .await
            .map_err(|_| AssistantStoreError::SourceUnavailable)?;
        if scope != *repository || filter != candidate.issue_filter {
            return Err(AssistantStoreError::SourceUnavailable);
        }
        Some(connection)
    } else {
        None
    };
    candidate.enabled = body.enabled;
    commit(db, candidate, connection.as_ref()).await
}

async fn read_connection(
    db: &DatabaseConnection,
    actor: &str,
    reference: &str,
) -> Result<IssueConnectionConfig, AssistantStoreError> {
    issue_connections::read(db)
        .await?
        .connection_for_actor(actor, reference)
        .cloned()
        .ok_or(AssistantStoreError::SourceUnavailable)
}

async fn commit(
    db: &DatabaseConnection,
    policy: IssueSubscription,
    connection: Option<&IssueConnectionConfig>,
) -> Result<WriteResult, AssistantStoreError> {
    let txn = crate::db::begin_write(
        db,
        desk_assistant_data_store::entity::agent_issue_subscription::Entity,
    )
    .await?;
    check_workbench(&txn, &policy.actor_id, &policy.conversation_id).await?;
    if let Some(connection) = connection {
        let config = issue_connections::read(&txn).await?;
        if config.connection_for_repository(&policy.actor_id, &policy.repository)
            != Some(connection)
        {
            return Err(AssistantStoreError::SourceUnavailable);
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    let actor = policy.actor_id.clone();
    let subscription = policy.subscription_id.clone();
    let result = settings::put_on(&txn, policy, now).await?;
    let permission_ends = match &result {
        WriteResult::Saved { affected, .. } => {
            crate::agent_subagent_store::apply_issue_changes_on(
                &txn,
                &actor,
                &subscription,
                affected,
                now,
            )
            .await?
        }
        WriteResult::Overlap(_) => Default::default(),
    };
    txn.commit().await?;
    permission_ends.submit(now, crate::model_metrics::runtime::submit);
    Ok(result)
}

pub(super) async fn check_workbench(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
) -> Result<(), AssistantStoreError> {
    if !desk_diagnose_core::subagent::valid_id(workbench) {
        return Err(AssistantStoreError::Inaccessible);
    }
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(workbench))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.actor_id != actor || !row.device_id.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let root = decode_workbench_coordinator(&row.state_json, actor, workbench)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    if root.version != row.version || !root.device_id.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn write_response(
    result: Result<WriteResult, AssistantStoreError>,
) -> Result<HttpResponse, DeskSignalError> {
    match result {
        Ok(WriteResult::Saved { view, .. }) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(view))),
        Ok(WriteResult::Overlap(existing)) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::failed_with_data(
                DeskErrorCode::REVISION_CONFLICT,
                Some("An enabled subscription already covers these issues".into()),
                Some(existing),
            ))),
        Err(error) => error_response(error),
    }
}

fn error_response(error: AssistantStoreError) -> Result<HttpResponse, DeskSignalError> {
    let (code, message) = match error {
        AssistantStoreError::Backend(error) => return Err(error.into()),
        AssistantStoreError::Stale { .. } => (
            DeskErrorCode::REVISION_CONFLICT,
            "Subscription changed. Reload before saving.",
        ),
        AssistantStoreError::SourceUnavailable => (
            DeskErrorCode::PRECONDITION_FAILED,
            "The configured issue connection or target is unavailable.",
        ),
        AssistantStoreError::Invalid(message) => (DeskErrorCode::INVALID_PARAMS, message),
        AssistantStoreError::Capacity => (
            DeskErrorCode::INVALID_PARAMS,
            "Subscription settings exceed the supported size.",
        ),
        AssistantStoreError::Inaccessible => return Ok(not_accessible()),
    };
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<()>::failed(code, message.into())))
}
