use super::*;
use crate::{
    config::connection::{DatabaseConnection, DatabaseTransaction},
    entity::agent_session,
};
use actix_web::put;
use desk_assistant_data_store::{AssistantStoreError, device_roster};
use desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};

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
    roster_response(update_configuration(crate::db::get_db(), &body, false).await)
}

pub(super) fn owner(session: &Session) -> Result<bool, DeskSignalError> {
    Ok(session
        .get_current_user::<CurrentUser>()
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
        .is_some_and(|user| user.target_connection_id.is_none()))
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
    let roster = device_roster::load_configuration(&txn, &actor, &run).await?;
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

#[cfg(test)]
pub(crate) async fn update_roster(
    db: &DatabaseConnection,
    body: &AiAssistantDeviceRosterBody,
) -> Result<AiAssistantDeviceRosterDto, AssistantStoreError> {
    update_configuration(db, body, true).await
}

async fn update_configuration(
    db: &DatabaseConnection,
    body: &AiAssistantDeviceRosterBody,
    initialize_roster: bool,
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
    let session = check_session(&txn, &actor, &run).await?;
    let has_roster = device_roster::load(&txn, &actor, &run).await?.is_some();
    if initialize_roster && session == Some(true) && body.expected_revision == 0 && !has_roster {
        return Err(AssistantStoreError::Inaccessible);
    }
    let targets = crate::agent_run_event_store::resolve_targets_on(&txn, &body.target_refs)
        .await
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let key = if has_roster || initialize_roster {
        run.clone()
    } else {
        device_roster::personal_selection_key(&run)
    };
    let roster = device_roster::replace(
        &txn,
        &actor,
        &key,
        body.expected_revision,
        targets.into_iter().map(|target| target.device_id).collect(),
    )
    .await?;
    if key != run && session.is_none() {
        let now = chrono::Utc::now();
        let (state_json, version) = device_roster::initial_personal_state(
            &actor,
            &run,
            &body.conversation,
            &now.to_rfc3339(),
        )?;
        agent_session::ActiveModel {
            conversation_id: Set(run.clone()),
            actor_id: Set(actor),
            device_id: Set(String::new()),
            conversation_schema_version: Set(Some(i32::from(CONVERSATION_SCHEMA_VERSION))),
            state_json: Set(state_json),
            version: Set(version),
            lease_token: Set(0),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }
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
) -> Result<Option<bool>, AssistantStoreError> {
    if crate::agent_subagent_store::deleted_on(txn, run).await? {
        return Err(AssistantStoreError::Inaccessible);
    }
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(run))
        .one(txn)
        .await?
    else {
        return Ok(None);
    };
    if row.actor_id != actor || !row.device_id.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    device_roster::has_existing_input(&row.state_json, actor, run, row.version).map(Some)
}

fn roster_response(
    result: Result<AiAssistantDeviceRosterDto, AssistantStoreError>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match result {
        Ok(roster) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(roster)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_web::test]
    async fn personal_device_selection_is_saved_per_conversation_without_creating_a_workbench() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        crate::device_code_store::get_or_create(&db, "selected-mac")
            .await
            .unwrap();
        let body = AiAssistantDeviceRosterBody {
            conversation: "personal-selection-test".into(),
            expected_revision: 0,
            target_refs: vec!["selected-mac".into()],
        };
        let saved = update_configuration(&db, &body, false).await.unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(
            read_roster(&db, &body.conversation)
                .await
                .unwrap()
                .target_refs,
            body.target_refs
        );
        assert!(
            read_roster(&db, "separate-session")
                .await
                .unwrap()
                .target_refs
                .is_empty()
        );
        let actor = SINGLE_ACCOUNT_USER_ID.to_string();
        assert!(
            device_roster::load(&db, &actor, &saved.session_id)
                .await
                .unwrap()
                .is_none()
        );
        let draft = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&saved.session_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            desk_diagnose_core::session::PersistedAgentSession::decode_boxed_json(
                &draft.state_json
            )
            .unwrap()
            .input_revision,
            0,
        );
        assert!(matches!(
            update_configuration(&db, &body, false).await,
            Err(AssistantStoreError::Stale {
                current_revision: 1
            })
        ));
        device_roster::delete(&db, &actor, &saved.session_id)
            .await
            .unwrap();
        assert!(
            read_roster(&db, &body.conversation)
                .await
                .unwrap()
                .target_refs
                .is_empty()
        );
    }
}
