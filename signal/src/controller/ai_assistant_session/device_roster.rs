use super::*;
use crate::{
    config::connection::{DatabaseConnection, DatabaseTransaction},
    entity::agent_session,
};
use actix_web::put;
use desk_assistant_data_store::{AssistantStoreError, device_roster};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};

#[utoipa::path(
    tag = TAG,
    summary = "Read the coordinator's device roster",
    params(("conversation" = String, Query, description = "Personal conversation intent")),
    responses((status = 200, body = RestResponse<AiAssistantDeviceRosterDto>)),
)]
#[get("/my/ai-assistant-session/device-roster")]
pub async fn get_ai_assistant_device_roster(
    session: Session,
    query: web::Query<AiAssistantDeviceRosterQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    if !owner(&session)? {
        return Ok(not_accessible());
    }
    roster_response(read_roster(crate::db::get_db(), &query.conversation).await)
}

#[utoipa::path(
    tag = TAG,
    summary = "Replace the coordinator's device roster at the observed revision",
    request_body = AiAssistantDeviceRosterBody,
    responses((status = 200, body = RestResponse<AiAssistantDeviceRosterDto>)),
)]
#[put("/my/ai-assistant-session/device-roster")]
pub async fn update_ai_assistant_device_roster(
    session: Session,
    body: web::Json<AiAssistantDeviceRosterBody>,
) -> Result<HttpResponse, DeskSignalError> {
    if !owner(&session)? {
        return Ok(not_accessible());
    }
    roster_response(update_roster(crate::db::get_db(), &body).await)
}

pub(super) fn owner(session: &Session) -> Result<bool, DeskSignalError> {
    Ok(session
        .get_current_user::<CurrentUser>()
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
        .is_some())
}

async fn read_roster(
    db: &DatabaseConnection,
    conversation: &str,
) -> Result<AiAssistantDeviceRosterDto, AssistantStoreError> {
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let run = selected_permission_session_id(&actor, "", None, Some(conversation)).ok_or(
        AssistantStoreError::Invalid("Invalid personal conversation"),
    )?;
    let txn = db.begin().await?;
    let _ = check_session(&txn, &actor, &run).await?;
    let roster = device_roster::load(&txn, &actor, &run).await?;
    txn.commit().await?;
    let (revision, target_refs) = roster.map_or((0, vec![]), |roster| {
        (roster.revision, roster.members.into_keys().collect())
    });
    Ok(AiAssistantDeviceRosterDto {
        session_id: run,
        revision,
        target_refs,
    })
}

pub(crate) async fn update_roster(
    db: &DatabaseConnection,
    body: &AiAssistantDeviceRosterBody,
) -> Result<AiAssistantDeviceRosterDto, AssistantStoreError> {
    desk_agent_protocol::ai_assistant::validate_target_refs(
        body.target_refs.iter().map(String::as_str),
    )
    .map_err(AssistantStoreError::Invalid)?;
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let run = selected_permission_session_id(&actor, "", None, Some(&body.conversation)).ok_or(
        AssistantStoreError::Invalid("Invalid personal conversation"),
    )?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    if check_session(&txn, &actor, &run).await?
        && body.expected_revision == 0
        && device_roster::load(&txn, &actor, &run).await?.is_none()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let targets = crate::agent_run_event_store::resolve_targets_on(&txn, &body.target_refs)
        .await
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let roster = device_roster::replace(
        &txn,
        &actor,
        &run,
        body.expected_revision,
        targets.into_iter().map(|target| target.device_id).collect(),
    )
    .await?;
    txn.commit().await?;
    Ok(AiAssistantDeviceRosterDto {
        session_id: run,
        revision: roster.revision,
        target_refs: body.target_refs.clone(),
    })
}

async fn check_session(
    txn: &DatabaseTransaction,
    actor: &str,
    run: &str,
) -> Result<bool, AssistantStoreError> {
    if crate::agent_subagent_store::deleted_on(txn, run).await? {
        return Err(AssistantStoreError::Inaccessible);
    }
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(run))
        .one(txn)
        .await?
    else {
        return Ok(false);
    };
    if row.actor_id != actor || !row.device_id.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    device_roster::has_existing_input(&row.state_json, actor, run, row.version)
}

fn roster_response(
    result: Result<AiAssistantDeviceRosterDto, AssistantStoreError>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match result {
        Ok(roster) => HttpResponse::Ok().json(RestResponse::succeed_with_data(roster)),
        Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
        Err(AssistantStoreError::Stale { .. }) => {
            HttpResponse::Ok().json(RestResponse::<()>::failed(
                DeskErrorCode::REVISION_CONFLICT,
                "Device roster changed. Reload it before saving.".into(),
            ))
        }
        Err(AssistantStoreError::Invalid(message)) => HttpResponse::Ok().json(
            RestResponse::<()>::failed(DeskErrorCode::INVALID_PARAMS, message.into()),
        ),
        Err(_) => not_accessible(),
    })
}
