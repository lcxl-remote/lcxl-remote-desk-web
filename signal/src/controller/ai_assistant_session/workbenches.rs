//! Durable personal coordinators for the Super Agent workbench.

use super::*;
use crate::{config::connection::DatabaseConnection, entity::agent_session};
use desk_agent_protocol::ai_assistant::workbench::{
    AiAssistantWorkbenchTasks, AiAssistantWorkbenchView, CreateAiAssistantWorkbench,
};
use desk_assistant_data_store::{AssistantStoreError, device_roster, workbench};
use desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<Vec<AiAssistantWorkbenchView>>)))]
#[get("/my/ai-assistant-workbenches")]
pub async fn list_ai_assistant_workbenches(
    session: Session,
) -> Result<HttpResponse, DeskSignalError> {
    if !super::device_roster::owner(&session)? {
        return Ok(not_accessible());
    }
    response(list(crate::db::get_db(), &SINGLE_ACCOUNT_USER_ID.to_string()).await)
}

#[derive(serde::Deserialize)]
pub struct WorkbenchTasksQuery {
    request: Option<String>,
    before: Option<String>,
}

#[utoipa::path(tag = TAG, params(("workbench_id" = String, Path), ("request" = Option<String>, Query), ("before" = Option<String>, Query)), responses((status = 200, body = RestResponse<AiAssistantWorkbenchTasks>)))]
#[get("/my/ai-assistant-workbenches/{workbench_id}/tasks")]
pub async fn list_ai_assistant_workbench_tasks(
    session: Session,
    workbench_id: web::Path<String>,
    query: web::Query<WorkbenchTasksQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    if !super::device_roster::owner(&session)? {
        return Ok(not_accessible());
    }
    let db = crate::db::get_db();
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    response(
        async {
            super::issue_subscriptions::check_workbench(db, &actor, &workbench_id).await?;
            workbench::tasks::list_on(
                db,
                &actor,
                &workbench_id,
                query.request.as_deref(),
                query.before.as_deref(),
            )
            .await
        }
        .await,
    )
}

#[utoipa::path(tag = TAG, request_body = CreateAiAssistantWorkbench, responses((status = 200, body = RestResponse<AiAssistantWorkbenchView>)))]
#[post("/my/ai-assistant-workbenches")]
pub async fn create_ai_assistant_workbench(
    session: Session,
    body: web::Json<CreateAiAssistantWorkbench>,
) -> Result<HttpResponse, DeskSignalError> {
    if !super::device_roster::owner(&session)? {
        return Ok(not_accessible());
    }
    response(
        create(
            crate::db::get_db(),
            &SINGLE_ACCOUNT_USER_ID.to_string(),
            body.into_inner(),
        )
        .await,
    )
}

async fn list(
    db: &DatabaseConnection,
    actor: &str,
) -> Result<Vec<AiAssistantWorkbenchView>, AssistantStoreError> {
    let rows = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::DeviceId.eq(""))
        .filter(agent_session::Column::Title.is_not_null())
        .filter(
            agent_session::Column::ConversationId
                .in_subquery(device_roster::configured_conversation_ids()),
        )
        .order_by_asc(agent_session::Column::Id)
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            workbench::view(
                actor,
                &row.conversation_id,
                &row.state_json,
                row.title,
                row.version,
                row.lease_token,
            )
        })
        .collect())
}

async fn create(
    db: &DatabaseConnection,
    actor: &str,
    body: CreateAiAssistantWorkbench,
) -> Result<AiAssistantWorkbenchView, AssistantStoreError> {
    let view = workbench::identity(actor, body)?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    if crate::agent_subagent_store::deleted_on(&txn, &view.workbench_id).await? {
        return Err(AssistantStoreError::Inaccessible);
    }
    if let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&view.workbench_id))
        .one(&txn)
        .await?
    {
        if row.actor_id != actor || !row.device_id.is_empty() {
            return Err(AssistantStoreError::Inaccessible);
        }
        let previous = workbench::view(
            actor,
            &row.conversation_id,
            &row.state_json,
            row.title,
            row.version,
            row.lease_token,
        )
        .filter(|previous| previous == &view)
        .ok_or(AssistantStoreError::Inaccessible)?;
        txn.commit().await?;
        return Ok(previous);
    }
    workbench::initialize_roster_on(&txn, actor, &view.workbench_id).await?;
    let now = chrono::Utc::now();
    let (state_json, version, lease_token) =
        workbench::initial_state(actor, &view, &now.to_rfc3339())?;
    agent_session::ActiveModel {
        conversation_id: Set(view.workbench_id.clone()),
        actor_id: Set(actor.into()),
        device_id: Set(String::new()),
        conversation_schema_version: Set(Some(i32::from(CONVERSATION_SCHEMA_VERSION))),
        title: Set(Some(view.name.clone())),
        state_json: Set(state_json),
        version: Set(version),
        lease_token: Set(lease_token),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(view)
}

fn response<T: serde::Serialize + utoipa::ToSchema>(
    result: Result<T, AssistantStoreError>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match result {
        Ok(view) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(view)),
        Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
        Err(AssistantStoreError::Invalid(message)) => HttpResponse::Ok().json(
            RestResponse::<()>::failed(DeskErrorCode::INVALID_PARAMS, message.into()),
        ),
        Err(_) => not_accessible(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_web::test]
    async fn creates_one_owned_coordinator_with_an_empty_roster() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let actor = SINGLE_ACCOUNT_USER_ID.to_string();
        let body = || CreateAiAssistantWorkbench {
            creation_id: "creation-id".into(),
            name: "  工单助手  ".into(),
        };
        let created = create(&db, &actor, body()).await.unwrap();
        assert_eq!(created.name, "工单助手");
        assert_eq!(create(&db, &actor, body()).await.unwrap(), created);
        assert_eq!(list(&db, &actor).await.unwrap(), vec![created.clone()]);
        assert!(list(&db, "foreign").await.unwrap().is_empty());
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&created.workbench_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let root = desk_assistant_data_store::main_request::decode_workbench_coordinator(
            &row.state_json,
            &actor,
            &created.workbench_id,
        )
        .unwrap();
        assert_eq!(root.input_revision, 0);
        assert!(
            crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
                .list_ai_assistant_sessions(&actor, "", 10)
                .await
                .unwrap()
                .is_empty()
        );
        let roster = device_roster::load(&db, &actor, &created.workbench_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(roster.revision, 1);
        assert!(roster.members.is_empty());
        use crate::agent_approval_store::{load_preference, save_preference};
        use desk_agent_protocol::ai_assistant::approval_mode::{
            AssistantApprovalMode, UpdateAssistantApprovalMode,
        };
        let initial = load_preference(&db, &actor, &created.workbench_id)
            .await
            .unwrap();
        assert_eq!(initial.mode, AssistantApprovalMode::AiReview);
        assert!(
            load_preference(&db, "foreign", &created.workbench_id)
                .await
                .is_err()
        );
        let update = || UpdateAssistantApprovalMode {
            session: created.workbench_id.clone(),
            expected_revision: initial.revision,
            mode: AssistantApprovalMode::Manual,
            no_review_confirmation_id: None,
        };
        assert!(
            save_preference(&db, &actor, update())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            save_preference(&db, &actor, update())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            load_preference(&db, &actor, &created.workbench_id)
                .await
                .unwrap()
                .mode,
            AssistantApprovalMode::Manual
        );
        let saved = agent_session::Entity::find()
            .filter(agent_session::Column::Id.eq(row.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state_json, saved.state_json);
        assert_eq!(row.version, saved.version);
        assert_eq!(row.lease_token, saved.lease_token);
    }
}
