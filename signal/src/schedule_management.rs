//! Owner-authenticated task management and explicit publication; no direct tool dispatch.
mod resume_sources;
use crate::entity::agent_schedule as row;
use crate::schedule_store::{ScheduleStore, ScheduleStoreError};
use desk_agent_protocol::schedule::management::{
    RehearsalView, ScheduleManagementRequest as Request, ScheduleManagementResponse as Response,
    ScheduleView,
};
use desk_signal_facade::{
    model::{connection::ConnectionState, signal::SignalingModel},
    service::{
        ControlFrameOutcome,
        schedule_management::{cookie_owner, reply},
    },
};
use desk_utils::error::DeskErrorCode;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

pub async fn handle(
    db: &DatabaseConnection,
    actor: &ConnectionState,
    model: &SignalingModel,
    connections: actix_web::web::Data<desk_signal_facade::model::connection::SharedConnectionMap>,
    gate: std::sync::Arc<crate::device_assistant_gate::DeviceAssistantGate>,
) -> ControlFrameOutcome {
    let result = async {
        let owner = cookie_owner(&actor.auth_context).ok_or(ScheduleStoreError::NotFound)?;
        if model.to_connection_id.is_some()
            || model.response_state.is_some()
            || model.request_id.is_empty()
            || model.request_id.len() > 256
        {
            return Err(ScheduleStoreError::Invalid);
        }
        if owner != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID {
            return Err(ScheduleStoreError::NotFound);
        }
        let request = model
            .get_data::<Request>()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if let Request::ResumeTask {
            schedule_id,
            expected_revision,
        } = request
        {
            let db = db.clone();
            return actix_web::rt::spawn(async move {
                let store = ScheduleStore::new(db.clone());
                let current = store.read(owner, &schedule_id).await?;
                let verifier = super::schedule_store::SignalTaskPublicationVerifier {
                    connections: &connections,
                    gate: &gate,
                    maximum_budget: &crate::schedule_budget_policy::read(&db).await?.maximum,
                };
                if current.kind == "conversation_resume" {
                    if public_target(&db, owner, &current.target_device_id)
                        .await?
                        .is_none()
                    {
                        return Err(ScheduleStoreError::NotFound);
                    }
                    let task = store
                        .resume_conversation_task(owner, &schedule_id, expected_revision, &verifier)
                        .await?;
                    return Ok(Response::Task {
                        task: view(&db, owner, task).await?,
                    });
                }
                let task = ScheduleStore::new(db.clone())
                    .resume_published_task(owner, &schedule_id, expected_revision, &verifier)
                    .await?;
                Ok(Response::Task {
                    task: view(&db, owner, task).await?,
                })
            })
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        if let Request::PublishTask {
            schedule_id,
            expected_revision,
            contract_revision,
            contract_sha256,
            rehearsal_run_id,
            expires_at,
            client_publish_key,
        } = request
        {
            let db = db.clone();
            // The database transaction remains owned by this task if the browser disconnects.
            return actix_web::rt::spawn(async move {
                let verifier = crate::schedule_store::SignalTaskPublicationVerifier {
                    connections: &connections,
                    gate: &gate,
                    maximum_budget: &crate::schedule_budget_policy::read(&db).await?.maximum,
                };
                publish(
                    &db,
                    owner,
                    super::schedule_store::PublishTask {
                        schedule_id,
                        expected_revision,
                        contract_revision,
                        contract_sha256,
                        rehearsal_run_id,
                        expires_at,
                        client_publish_key,
                    },
                    &verifier,
                )
                .await
            })
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        manage(db, owner, request).await
    }
    .await;
    reply(
        actor,
        model,
        result.map_err(|error| match error {
            ScheduleStoreError::NotFound => (
                DeskErrorCode::PERMISSION_ERROR,
                "task not found or not accessible",
            ),
            ScheduleStoreError::Invalid => (
                DeskErrorCode::INVALID_PARAMS,
                "invalid task management request",
            ),
            ScheduleStoreError::Conflict => (
                DeskErrorCode::PRECONDITION_FAILED,
                "task changed; refresh before retrying",
            ),
            ScheduleStoreError::BudgetExceeded => {
                (DeskErrorCode::PRECONDITION_FAILED, "task budget exceeded")
            }
            ScheduleStoreError::Backend(_) => {
                (DeskErrorCode::SYSTEM_ERROR, "task management unavailable")
            }
        }),
    )
    .await
}

pub(crate) async fn manage(
    db: &DatabaseConnection,
    owner: i32,
    request: Request,
) -> Result<Response, ScheduleStoreError> {
    let store = ScheduleStore::new(db.clone());
    let row = match request {
        Request::PublishTask { .. } | Request::ResumeTask { .. } => {
            return Err(ScheduleStoreError::Conflict);
        }
        Request::ListResumeSources {
            target_device_id,
            offset,
            limit,
        } => {
            return resume_sources::list(db, owner, &target_device_id, offset, limit).await;
        }
        Request::ConvertTime { input } => {
            let conversion = desk_diagnose_core::schedule::timezone::convert(&input)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            let upcoming_runs = desk_diagnose_core::schedule::timezone::upcoming_runs(
                &conversion.spec,
                chrono::Utc::now().timestamp_millis(),
            )
            .map_err(|_| ScheduleStoreError::Invalid)?;
            return Ok(Response::ConvertedTime {
                conversion,
                upcoming_runs,
            });
        }
        Request::Search {
            after,
            limit,
            kind,
            status,
            title,
            target_device_id,
            attention_only,
        } => {
            let after = match after {
                Some(id) => store.read(owner, &id).await?.id,
                None => 0,
            };
            let kind = kind
                .map(serde_json::to_value)
                .transpose()
                .map_err(|_| ScheduleStoreError::Invalid)?;
            let status = status
                .map(serde_json::to_value)
                .transpose()
                .map_err(|_| ScheduleStoreError::Invalid)?;
            let (rows, total, attention_count) = store
                .search(
                    owner,
                    after,
                    u64::from(limit),
                    kind.as_ref().and_then(|value| value.as_str()),
                    status.as_ref().and_then(|value| value.as_str()),
                    title.as_deref(),
                    target_device_id.as_deref(),
                    attention_only,
                )
                .await?;
            let next_cursor =
                (rows.len() == limit as usize).then(|| rows.last().unwrap().schedule_id.clone());
            let mut tasks = Vec::with_capacity(rows.len());
            for row in rows {
                tasks.push(view(db, owner, row).await?);
            }
            return Ok(Response::SearchResults {
                tasks,
                next_cursor,
                total_count: total,
                attention_count,
            });
        }
        Request::List { after, limit } => {
            let after = match after {
                Some(id) => store.read(owner, &id).await?.id,
                None => 0,
            };
            let rows = store.list(owner, after, u64::from(limit)).await?;
            let next_cursor =
                (rows.len() == limit as usize).then(|| rows.last().unwrap().schedule_id.clone());
            let mut tasks = Vec::with_capacity(rows.len());
            for row in rows {
                tasks.push(view(db, owner, row).await?);
            }
            return Ok(Response::List { tasks, next_cursor });
        }
        Request::ListRuns {
            schedule_id,
            before,
            limit,
        } => {
            let page = store
                .run_history(owner, &schedule_id, before.as_deref(), limit)
                .await?;
            let runs = page
                .runs
                .into_iter()
                .map(run_view)
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Response::Runs {
                schedule_id,
                runs,
                next_cursor: page.next_cursor,
            });
        }
        Request::DecideRunDirectory {
            schedule_id,
            run_id,
            directory_request_id,
            expected_scope_revision,
            approve,
            client_request_key,
        } => {
            run_directory::decide(
                db,
                owner,
                &schedule_id,
                &run_id,
                &directory_request_id,
                expected_scope_revision,
                Some(approve),
                &client_request_key,
            )
            .await?;
            store.read(owner, &schedule_id).await?
        }
        Request::RevokeRunDirectory {
            schedule_id,
            run_id,
            directory_request_id,
            expected_scope_revision,
            client_request_key,
        } => {
            run_directory::decide(
                db,
                owner,
                &schedule_id,
                &run_id,
                &directory_request_id,
                expected_scope_revision,
                None,
                &client_request_key,
            )
            .await?;
            store.read(owner, &schedule_id).await?
        }
        Request::DisposeRunOutcome {
            schedule_id,
            run_id,
            expected_revision,
            client_request_key,
            work_id,
            execution_id,
            note,
        } => {
            store
                .dispose_run_outcome(
                    owner,
                    &schedule_id,
                    &run_id,
                    expected_revision,
                    &client_request_key,
                    &note,
                    work_id,
                    &execution_id,
                )
                .await?;
            store.read(owner, &schedule_id).await?
        }
        Request::AcknowledgeRunOutcome {
            schedule_id,
            run_id,
            expected_revision,
            client_request_key,
            note,
        } => {
            store
                .acknowledge_run_outcome(
                    owner,
                    &schedule_id,
                    &run_id,
                    expected_revision,
                    &client_request_key,
                    &note,
                )
                .await?;
            store.read(owner, &schedule_id).await?
        }
        Request::RunTaskNow {
            schedule_id,
            expected_revision,
            client_request_key,
        } => {
            store
                .enqueue_manual_at_revision(
                    owner,
                    &schedule_id,
                    &client_request_key,
                    expected_revision,
                )
                .await?;
            store.read(owner, &schedule_id).await?
        }
        Request::CancelTaskRun { run_id } => {
            let work = store.cancel_run(owner, &run_id).await?;
            store.read(owner, &work.schedule_id).await?
        }
        Request::Get { schedule_id } => store.read(owner, &schedule_id).await?,
        Request::GenerateTaskContract {
            schedule_id,
            expected_revision,
        } => {
            let task = store.read(owner, &schedule_id).await?;
            if public_target(db, owner, &task.target_device_id)
                .await?
                .is_none()
            {
                return Err(ScheduleStoreError::NotFound);
            }
            let contract = store
                .generate_task_contract(owner, &schedule_id, expected_revision)
                .await?;
            store
                .save_contract(owner, expected_revision, &contract)
                .await?;
            return contract_response(db, owner, store.read(owner, &schedule_id).await?).await;
        }
        Request::GetTaskContract { schedule_id } => {
            let task = store.read(owner, &schedule_id).await?;
            return contract_response(db, owner, task).await;
        }
        Request::SaveTaskContract {
            expected_revision,
            mut contract,
        } => {
            let task = store.read(owner, &contract.schedule_id).await?;
            let target = public_target(db, owner, &task.target_device_id)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
            if contract.target_device_id != target {
                return Err(ScheduleStoreError::NotFound);
            }
            // Resolve the public handle on the server; clients never choose an internal ID.
            contract.target_device_id = task.target_device_id;
            store
                .save_contract(owner, expected_revision, &contract)
                .await?;
            let task = store.read(owner, &contract.schedule_id).await?;
            return contract_response(db, owner, task).await;
        }
        Request::CreateDraft { draft } => {
            let draft = resolve_draft(db, owner, draft).await?;
            store
                .create_draft(owner, &draft, store.database_time().await?)
                .await?
        }
        Request::ActivateConversationResume {
            schedule_id,
            expected_revision,
        } => {
            let task = store.read(owner, &schedule_id).await?;
            if public_target(db, owner, &task.target_device_id)
                .await?
                .is_none()
            {
                return Err(ScheduleStoreError::NotFound);
            }
            store
                .activate_conversation_resume(owner, &schedule_id, expected_revision)
                .await?
        }
        Request::ReserveRehearsal {
            schedule_id,
            expected_revision,
            client_request_key,
        } => {
            let task = store.read(owner, &schedule_id).await?;
            if public_target(db, owner, &task.target_device_id)
                .await?
                .is_none()
            {
                return Err(ScheduleStoreError::NotFound);
            }
            let rehearsal = store
                .reserve_rehearsal(owner, &schedule_id, expected_revision, &client_request_key)
                .await?;
            return rehearsal_response(db, owner, rehearsal).await;
        }
        Request::GetTaskRehearsal { schedule_id } => {
            let latest = store.read_latest_rehearsal(owner, &schedule_id).await?;
            let rehearsal = match latest {
                Some(row) => Some(rehearsal_view(db, owner, row).await?),
                None => None,
            };
            return Ok(Response::TaskRehearsal {
                task: Box::new(view(db, owner, store.read(owner, &schedule_id).await?).await?),
                rehearsal,
            });
        }
        Request::GetRehearsal { rehearsal_id } => {
            return rehearsal_response(
                db,
                owner,
                store.read_rehearsal(owner, &rehearsal_id).await?,
            )
            .await;
        }
        Request::GetRehearsalPermissions { rehearsal_id } => {
            return rehearsal_permissions::read(db, owner, &rehearsal_id).await;
        }
        Request::CancelPendingRehearsal {
            rehearsal_id,
            expected_revision,
        } => {
            let rehearsal = store
                .cancel_pending_rehearsal(owner, &rehearsal_id, expected_revision)
                .await?;
            return rehearsal_response(db, owner, rehearsal).await;
        }
        Request::Rename {
            schedule_id,
            expected_revision,
            title,
        } => {
            store
                .rename(owner, &schedule_id, expected_revision, &title)
                .await?
        }
        Request::SetFailureThreshold {
            schedule_id,
            expected_revision,
            failure_threshold,
        } => {
            store
                .set_failure_threshold(owner, &schedule_id, expected_revision, failure_threshold)
                .await?
        }
        Request::ChangePrompt {
            schedule_id,
            expected_revision,
            prompt,
        } => {
            let task = store.read(owner, &schedule_id).await?;
            if public_target(db, owner, &task.target_device_id)
                .await?
                .is_none()
            {
                return Err(ScheduleStoreError::NotFound);
            }
            store
                .change_prompt(owner, &schedule_id, expected_revision, &prompt)
                .await?
        }
        Request::ChangeTime {
            schedule_id,
            expected_revision,
            spec,
            time_confirmation,
        } => {
            if let Some(confirmation) = &time_confirmation {
                desk_diagnose_core::schedule::timezone::verify_confirmation(&spec, confirmation)
                    .map_err(|_| ScheduleStoreError::Invalid)?;
            }
            store
                .change_time(owner, &schedule_id, expected_revision, &spec)
                .await?
        }
        Request::RevokeTaskAuthorization {
            schedule_id,
            expected_revision,
        } => {
            store
                .revoke_current_task_authorization(owner, &schedule_id, expected_revision)
                .await?
        }
        Request::Pause {
            schedule_id,
            expected_revision,
        } => {
            store
                .pause(
                    owner,
                    &schedule_id,
                    expected_revision,
                    store.database_time().await?,
                )
                .await?
        }
        Request::Delete {
            schedule_id,
            expected_revision,
        } => store.delete(owner, &schedule_id, expected_revision).await?,
    };
    Ok(Response::Task {
        task: view(db, owner, row).await?,
    })
}

async fn contract_response(
    db: &DatabaseConnection,
    owner: i32,
    task: row::Model,
) -> Result<Response, ScheduleStoreError> {
    use sha2::{Digest, Sha256};
    if task.kind != "fresh_task" || task.status == "deleted" {
        return Err(ScheduleStoreError::Conflict);
    }
    let target = public_target(db, owner, &task.target_device_id)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let (contract, contract_sha256) = if let Some(revision) = task.contract_revision {
        let stored = ScheduleStore::new(db.clone())
            .read_contract(owner, &task.schedule_id, revision)
            .await?;
        let mut contract: desk_agent_protocol::schedule::contract::TaskContract =
            serde_json::from_str(&stored.canonical_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
        contract.target_device_id = target.clone();
        (Some(Box::new(contract)), Some(stored.digest_sha256))
    } else {
        (None, None)
    };
    let previous_contract = if let Some(revision) = task.contract_revision {
        use crate::entity::agent_task_contract as history;
        use sea_orm::QueryOrder;
        if let Some(previous) = history::Entity::find()
            .filter(history::Column::OwnerUserId.eq(owner))
            .filter(history::Column::ScheduleId.eq(&task.schedule_id))
            .filter(history::Column::ContractRevision.lt(revision))
            .order_by_desc(history::Column::ContractRevision)
            .one(db)
            .await?
        {
            let parsed =
                desk_diagnose_core::schedule::contract::parse_contract(&previous.canonical_json)
                    .map_err(|_| ScheduleStoreError::Invalid)?;
            if parsed.digest() != previous.digest_sha256
                || parsed.contract().target_device_id != task.target_device_id
                || parsed.contract().schedule_id != task.schedule_id
                || i64::try_from(parsed.contract().contract_revision).ok()
                    != Some(previous.contract_revision)
                || i64::try_from(parsed.contract().task_revision).ok()
                    != Some(previous.task_revision)
            {
                return Err(ScheduleStoreError::Invalid);
            }
            let mut previous = parsed.contract().clone();
            previous.target_device_id = target.clone();
            Some(Box::new(previous))
        } else {
            None
        }
    } else {
        None
    };
    let authorization = if let Some(revision) = task.authorization_revision {
        use crate::entity::agent_task_authorization as grant;
        let row = grant::Entity::find()
            .filter(grant::Column::OwnerUserId.eq(owner))
            .filter(grant::Column::ScheduleId.eq(&task.schedule_id))
            .filter(grant::Column::AuthorizationRevision.eq(revision))
            .one(db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        Some(
            desk_agent_protocol::schedule::management::TaskAuthorizationView {
                authorization_revision: u64::try_from(row.authorization_revision)
                    .map_err(|_| ScheduleStoreError::Invalid)?,
                task_revision: u64::try_from(row.task_revision)
                    .map_err(|_| ScheduleStoreError::Invalid)?,
                contract_revision: u64::try_from(row.contract_revision)
                    .map_err(|_| ScheduleStoreError::Invalid)?,
                approved_at: utc(row.approved_at)?,
                expires_at: row.expires_at.map(utc).transpose()?,
                revoked_at: row.revoked_at.map(utc).transpose()?,
            },
        )
    } else {
        None
    };
    Ok(Response::TaskContract {
        previous_contract,
        authorization,
        task_revision: u64::try_from(task.task_revision)
            .map_err(|_| ScheduleStoreError::Invalid)?,
        prompt_sha256: format!("{:x}", Sha256::digest(&task.prompt)),
        task: Box::new(view(db, owner, task).await?),
        contract,
        contract_sha256,
    })
}

async fn rehearsal_response(
    db: &DatabaseConnection,
    owner: i32,
    row: crate::entity::agent_task_rehearsal::Model,
) -> Result<Response, ScheduleStoreError> {
    let task = ScheduleStore::new(db.clone())
        .read(owner, &row.schedule_id)
        .await?;
    Ok(Response::Rehearsal {
        task: Box::new(view(db, owner, task).await?),
        rehearsal: rehearsal_view(db, owner, row).await?,
    })
}

async fn rehearsal_view(
    db: &DatabaseConnection,
    owner: i32,
    row: crate::entity::agent_task_rehearsal::Model,
) -> Result<RehearsalView, ScheduleStoreError> {
    let target_device_id = public_target(db, owner, &row.target_device_id).await?;
    Ok(RehearsalView {
        rehearsal_id: row.rehearsal_id.clone(),
        schedule_id: row.schedule_id,
        task_revision: row.task_revision,
        target_device_id,
        client_conversation_id: row.client_conversation_id,
        initial_message_id: format!("rehearsal:{}:input", row.rehearsal_id),
        prompt: row.prompt,
        locale: row.locale,
        model_id: row.model_id,
        status: serde_json::from_value(serde_json::Value::String(row.status))
            .map_err(|_| ScheduleStoreError::Invalid)?,
        started_at: row.started_at.map(utc).transpose()?,
        finished_at: row.finished_at.map(utc).transpose()?,
    })
}

fn run_view(
    row: crate::entity::agent_schedule_run::Model,
) -> Result<desk_agent_protocol::schedule::management::ScheduledRunView, ScheduleStoreError> {
    use desk_agent_protocol::schedule::management::ScheduledRunView;
    Ok(ScheduledRunView {
        issue: desk_diagnose_core::schedule::history::public_issue(
            &row.status,
            row.error_kind.as_deref(),
        ),
        run_id: row.run_id,
        status: serde_json::from_value(serde_json::Value::String(row.status))
            .map_err(|_| ScheduleStoreError::Invalid)?,
        source: serde_json::from_value(serde_json::Value::String(row.source))
            .map_err(|_| ScheduleStoreError::Invalid)?,
        scheduled_at: row.scheduled_at.map(utc).transpose()?,
        requested_at: utc(row.requested_at)?,
        started_at: row.started_at.map(utc).transpose()?,
        finished_at: row.finished_at.map(utc).transpose()?,
        receipts_reconciled_at: row.receipts_reconciled_at.map(utc).transpose()?,
        outcome_reviewed_at: super::schedule_store::outcome_reviewed_at(
            row.outcome_review_json.as_deref(),
        )?
        .map(utc)
        .transpose()?,
        cancel_requested_at: row.cancel_requested_at.map(utc).transpose()?,
        missed_count: row.missed_count,
    })
}

fn utc(value: i64) -> Result<String, ScheduleStoreError> {
    chrono::DateTime::from_timestamp_millis(value)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .ok_or(ScheduleStoreError::Invalid)
}
async fn view(
    db: &DatabaseConnection,
    owner: i32,
    row: row::Model,
) -> Result<ScheduleView, ScheduleStoreError> {
    let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
        serde_json::from_str(&row.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    let target_device_id = public_target(db, owner, &row.target_device_id).await?;
    let spec = desk_diagnose_core::schedule::parse_json(&row.spec_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let upcoming_runs = if matches!(row.status.as_str(), "deleted" | "completed") {
        vec![]
    } else {
        desk_diagnose_core::schedule::timezone::upcoming_runs(
            &spec,
            chrono::Utc::now().timestamp_millis(),
        )
        .map_err(|_| ScheduleStoreError::Invalid)?
    };
    Ok(ScheduleView {
        upcoming_runs,
        schedule_id: row.schedule_id,
        target_device_id,
        kind: serde_json::from_value(serde_json::Value::String(row.kind))
            .map_err(|_| ScheduleStoreError::Invalid)?,
        title: row.title,
        prompt: row.prompt,
        status: serde_json::from_value(serde_json::Value::String(row.status))
            .map_err(|_| ScheduleStoreError::Invalid)?,
        revision: row.revision,
        spec,
        next_run_at: row.next_run_at.map(utc).transpose()?,
        active_run_id: row.active_run_id,
        pause_reasons: failures.pause_reasons.into_iter().collect(),
        consecutive_failures: failures.consecutive_failures,
        failure_threshold: failures.failure_threshold,
        created_at: utc(row.created_at)?,
        updated_at: utc(row.updated_at)?,
    })
}

async fn resolve_draft(
    db: &DatabaseConnection,
    owner: i32,
    mut draft: desk_agent_protocol::schedule::ScheduleDraft,
) -> Result<desk_agent_protocol::schedule::ScheduleDraft, ScheduleStoreError> {
    // The single owner may name a future/offline device. This is only a draft;
    // execution resolves an authenticated Server audience before any authority.
    resolve_source(db, owner, &mut draft).await?;
    Ok(draft)
}
// Source IDs on the wire are client conversation intents, never authority or
// raw session storage keys. Persist only the resolved subject-namespaced key.
async fn resolve_source(
    db: &DatabaseConnection,
    owner: i32,
    draft: &mut desk_agent_protocol::schedule::ScheduleDraft,
) -> Result<(), ScheduleStoreError> {
    use crate::entity::agent_session as session_row;
    use desk_diagnose_core::{
        conversation_key::{derive_conversation_key, is_valid_client_conversation_id},
        session::{AgentSessionSurface, PersistedAgentSession},
    };
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let Some(source) = draft.source_conversation_id.as_deref() else {
        return Ok(());
    };
    if !is_valid_client_conversation_id(source) {
        return Err(ScheduleStoreError::Invalid);
    }
    let actor = owner.to_string();
    let key = derive_conversation_key(&actor, &draft.target_device_id, Some(source), "");
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&key))
        .filter(session_row::Column::ActorId.eq(&actor))
        .filter(session_row::Column::DeviceId.eq(&draft.target_device_id))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.conversation_id != key
        || session.client_conversation_id.as_deref() != Some(source)
        || session
            .check_subject(&actor, &draft.target_device_id)
            .is_err()
        || session
            .check_surface(AgentSessionSurface::DeviceAssistant)
            .is_err()
    {
        return Err(ScheduleStoreError::NotFound);
    }
    if draft
        .requirement_revision
        .is_some_and(|revision| revision != session.input_revision)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    draft.source_conversation_id = Some(key);
    Ok(())
}

async fn public_target(
    _: &DatabaseConnection,
    _: i32,
    id: &str,
) -> Result<Option<String>, ScheduleStoreError> {
    Ok(Some(id.into()))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn run_history_projects_only_public_occurrence_metadata() {
        use crate::schedule_store::{
            TestPublicationVerifier as Verifier, publication_test_fixture as fixture_on,
        };
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let (store, task, _, publication) = fixture_on(db.clone()).await;
        store
            .publish_task(1, &publication, &Verifier(true))
            .await
            .unwrap();
        let row = store
            .enqueue_manual(1, &task.schedule_id, "history-api")
            .await
            .unwrap();
        let response = manage(
            &db,
            1,
            Request::ListRuns {
                schedule_id: task.schedule_id.clone(),
                before: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["result"], "runs");
        assert_eq!(value["schedule_id"], task.schedule_id);
        assert_eq!(value["runs"][0]["run_id"], row.run_id);
        assert_eq!(value["runs"][0]["status"], "queued");
        assert_eq!(value["runs"][0]["source"], "manual");
        assert_eq!(value["runs"][0].as_object().unwrap().len(), 12);
        assert!(value["runs"][0]["receipts_reconciled_at"].is_null());
        assert!(value["runs"][0]["outcome_reviewed_at"].is_null());
        for private in [
            "task_snapshot_json",
            "conversation_id",
            "turn_id",
            "lease_owner",
            "error_kind",
            "result_ref",
            "owner_user_id",
        ] {
            assert!(value["runs"][0].get(private).is_none());
        }
        assert!(value["runs"][0]["issue"].is_null());
        let mut failed = row.clone();
        failed.status = "failed".into();
        failed.error_kind = Some("provider secret internal text".into());
        let projected = serde_json::to_value(run_view(failed.clone()).unwrap()).unwrap();
        assert_eq!(
            projected["issue"],
            serde_json::json!({"kind":"unavailable"})
        );
        assert!(!projected.to_string().contains("provider secret"));
        failed.error_kind = Some("timeout".into());
        assert_eq!(
            serde_json::to_value(run_view(failed).unwrap()).unwrap()["issue"],
            serde_json::json!({"kind":"agent", "error":"timeout"})
        );
        assert!(matches!(
            manage(
                &db,
                2,
                Request::ListRuns {
                    schedule_id: task.schedule_id,
                    before: None,
                    limit: 10
                }
            )
            .await,
            Err(ScheduleStoreError::NotFound)
        ));
    }

    mod contracts;
    mod rehearsal;
    use super::*;
    use desk_agent_protocol::schedule::{
        ScheduleCreationSource, ScheduleDraft, ScheduleRule, ScheduleSpec, ScheduledTaskKind,
        ScheduledTaskStatus,
    };
    use sea_orm::{ConnectionTrait, Database, EntityTrait, PaginatorTrait, Schema};

    async fn fixture() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(row::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(crate::entity::agent_schedule_run::Entity))
            .await
            .unwrap();

        db
    }
    #[tokio::test]
    async fn source_is_resolved_against_real_subject_surface_and_input_revision() {
        use crate::entity::agent_session as session_row;
        use desk_agent_protocol::{AgentScope, ExecutionMode};
        use desk_diagnose_core::{
            conversation_key::derive_conversation_key,
            session::{AgentSessionSurface, PersistedAgentSession},
        };
        use sea_orm::{ActiveModelTrait, Set};
        let db = fixture().await;
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(session_row::Entity))
            .await
            .unwrap();
        let key = derive_conversation_key("1", "public-device-1", Some("chat-1"), "");
        let mut session = PersistedAgentSession::new(
            &key,
            "1",
            "public-device-1",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::SuggestOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-06T00:00:00Z",
        );
        session.surface = AgentSessionSurface::DeviceAssistant;
        session.client_conversation_id = Some("chat-1".into());
        session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
        session.input_revision = 1;
        let original = session.encode_json_for_storage().unwrap();
        PersistedAgentSession::decode_json(&original).unwrap();
        let row = session_row::ActiveModel {
            conversation_id: Set(key.clone()),
            actor_id: Set("1".into()),
            device_id: Set("public-device-1".into()),
            state_json: Set(original),
            version: Set(0),
            lease_token: Set(0),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let mut request = draft();
        request.kind = ScheduledTaskKind::ConversationResume;
        request.spec.rule = ScheduleRule::Once {
            at: (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        request.source_conversation_id = Some("chat-1".into());
        request.requirement_revision = Some(1);
        let created = task(
            manage(
                &db,
                1,
                Request::CreateDraft {
                    draft: request.clone(),
                },
            )
            .await
            .unwrap(),
        );
        assert!(
            manage(
                &db,
                2,
                Request::ActivateConversationResume {
                    schedule_id: created.schedule_id.clone(),
                    expected_revision: created.revision,
                }
            )
            .await
            .is_err()
        );
        let activated = task(
            manage(
                &db,
                1,
                Request::ActivateConversationResume {
                    schedule_id: created.schedule_id.clone(),
                    expected_revision: created.revision,
                },
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            activated.status,
            desk_agent_protocol::schedule::ScheduledTaskStatus::Active
        );
        assert!(activated.next_run_at.is_some());
        assert!(
            manage(
                &db,
                1,
                Request::ActivateConversationResume {
                    schedule_id: created.schedule_id.clone(),
                    expected_revision: created.revision,
                }
            )
            .await
            .is_err()
        );
        let stored = ScheduleStore::new(db.clone())
            .read(1, &created.schedule_id)
            .await
            .unwrap();
        assert_eq!(stored.source_conversation_id.as_deref(), Some(key.as_str()));
        assert_eq!(created.status, ScheduledTaskStatus::Draft);
        let mut stale = request.clone();
        stale.requirement_revision = Some(2);
        assert!(matches!(
            manage(&db, 1, Request::CreateDraft { draft: stale }).await,
            Err(ScheduleStoreError::Conflict)
        ));
        let mut raw = request.clone();
        raw.source_conversation_id = Some(key);
        assert!(matches!(
            manage(&db, 1, Request::CreateDraft { draft: raw }).await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert!(matches!(
            manage(
                &db,
                2,
                Request::CreateDraft {
                    draft: request.clone()
                }
            )
            .await,
            Err(ScheduleStoreError::NotFound)
        ));
        // Denormalized query columns cannot conceal a different JSON subject.
        session.actor_id = "2".into();
        let mut changed: session_row::ActiveModel = row.clone().into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.update(&db).await.unwrap();
        assert!(matches!(
            manage(
                &db,
                1,
                Request::CreateDraft {
                    draft: request.clone()
                }
            )
            .await,
            Err(ScheduleStoreError::NotFound)
        ));
        session.actor_id = "1".into();
        session.surface = AgentSessionSurface::TerminalCopilot;
        let mut changed: session_row::ActiveModel = row.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.update(&db).await.unwrap();
        assert!(matches!(
            manage(&db, 1, Request::CreateDraft { draft: request }).await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert_eq!(row::Entity::find().count(&db).await.unwrap(), 1);
    }

    fn draft() -> ScheduleDraft {
        ScheduleDraft {
            time_confirmation: None,
            client_create_key: "management-request-1".into(),
            kind: ScheduledTaskKind::FreshTask,
            target_device_id: "public-device-1".into(),
            title: "Daily report".into(),
            prompt: "Report device status".into(),
            locale: None,
            model_id: None,
            spec: ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Daily {
                    utc_time: "06:00:00".into(),
                },
            },
            source_conversation_id: None,
            requirement_revision: None,
            creation_source: ScheduleCreationSource::Manual,
        }
    }
    fn task(response: Response) -> ScheduleView {
        match response {
            Response::Task { task } => task,
            _ => panic!("expected task"),
        }
    }

    #[tokio::test]
    async fn management_creates_only_drafts_projects_public_identity_and_preserves_owner_cas() {
        let db = fixture().await;
        let original = task(
            manage(&db, 1, Request::CreateDraft { draft: draft() })
                .await
                .unwrap(),
        );
        assert_eq!(
            original.target_device_id.as_deref(),
            Some("public-device-1")
        );
        assert_eq!(original.status, ScheduledTaskStatus::Draft);
        assert!(original.next_run_at.is_none());
        assert!(original.active_run_id.is_none());
        assert!(original.created_at.ends_with('Z'));
        let replay = task(
            manage(&db, 1, Request::CreateDraft { draft: draft() })
                .await
                .unwrap(),
        );
        assert_eq!(replay.schedule_id, original.schedule_id);
        assert_eq!(row::Entity::find().count(&db).await.unwrap(), 1);
        assert_eq!(
            crate::entity::agent_schedule_run::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
        assert!(matches!(
            manage(
                &db,
                2,
                Request::Get {
                    schedule_id: original.schedule_id.clone()
                }
            )
            .await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert!(matches!(
            manage(
                &db,
                2,
                Request::List {
                    after: Some(original.schedule_id.clone()),
                    limit: 10
                }
            )
            .await,
            Err(ScheduleStoreError::NotFound)
        ));
        let renamed = task(
            manage(
                &db,
                1,
                Request::Rename {
                    schedule_id: original.schedule_id.clone(),
                    expected_revision: original.revision,
                    title: "Renamed".into(),
                },
            )
            .await
            .unwrap(),
        );
        assert!(matches!(
            manage(
                &db,
                1,
                Request::Rename {
                    schedule_id: original.schedule_id.clone(),
                    expected_revision: original.revision,
                    title: "Stale".into()
                }
            )
            .await,
            Err(ScheduleStoreError::Conflict)
        ));
        let Response::List { tasks, next_cursor } = manage(
            &db,
            1,
            Request::List {
                after: None,
                limit: 1,
            },
        )
        .await
        .unwrap() else {
            panic!("expected list")
        };
        assert_eq!(tasks.len(), 1);
        assert_eq!(next_cursor.as_deref(), Some(original.schedule_id.as_str()));
        let deleted = task(
            manage(
                &db,
                1,
                Request::Delete {
                    schedule_id: original.schedule_id,
                    expected_revision: renamed.revision,
                },
            )
            .await
            .unwrap(),
        );
        assert_eq!(deleted.status, ScheduledTaskStatus::Deleted);
        let Response::List { tasks, .. } = manage(
            &db,
            1,
            Request::List {
                after: None,
                limit: 10,
            },
        )
        .await
        .unwrap() else {
            panic!("expected list")
        };
        assert!(tasks.is_empty());
    }

    #[test]
    fn wire_request_cannot_smuggle_an_owner_or_authorization() {
        for extra in [
            "owner_user_id",
            "authorization_revision",
            "org_id",
            "status",
        ] {
            let mut request = serde_json::json!({"operation":"get", "schedule_id":"task-1"});
            request[extra] = serde_json::json!(1);
            assert!(serde_json::from_value::<Request>(request).is_err());
        }
    }
    #[tokio::test]
    async fn local_time_conversion_returns_utc_without_creating_a_task() {
        let db = fixture().await;
        let request: Request = serde_json::from_value(serde_json::json!({
            "operation": "convert_time",
            "input": { "timezone": "Asia/Shanghai", "reference_date": "2026-09-06", "local_time": "14:00:00", "fold": null, "rule": { "kind": "once" } }
        })).unwrap();
        let Response::ConvertedTime { conversion, .. } = manage(&db, 1, request).await.unwrap()
        else {
            panic!("expected converted time")
        };
        assert_eq!(
            conversion.spec.rule,
            ScheduleRule::Once {
                at: "2026-09-06T06:00:00Z".into()
            }
        );
        assert_eq!(row::Entity::find().count(&db).await.unwrap(), 0);
        assert_eq!(
            crate::entity::agent_schedule_run::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
    }
}

pub(crate) mod rehearsal_permissions;

/// Only the authenticated runtime handler supplies the current-policy verifier.
async fn publish(
    db: &DatabaseConnection,
    owner: i32,
    input: super::schedule_store::PublishTask,
    verifier: &dyn super::schedule_store::TaskPublicationVerifier,
) -> Result<Response, ScheduleStoreError> {
    let store = ScheduleStore::new(db.clone());
    let task = store.read(owner, &input.schedule_id).await?;
    if public_target(db, owner, &task.target_device_id)
        .await?
        .is_none()
    {
        return Err(ScheduleStoreError::NotFound);
    }
    store.publish_task(owner, &input, verifier).await?;
    Ok(Response::Task {
        task: view(db, owner, store.read(owner, &input.schedule_id).await?).await?,
    })
}

mod run_directory;
