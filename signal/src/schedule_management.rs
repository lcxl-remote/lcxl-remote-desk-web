//! Owner-authenticated task management; no direct tool dispatch.
use crate::config::connection::DatabaseConnection;
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;
mod resume_sources;
use crate::entity::agent_schedule as row;
use crate::schedule_store::{ScheduleStore, ScheduleStoreError};
use desk_agent_protocol::schedule::management::{
    ScheduleManagementRequest as Request, ScheduleManagementResponse as Response, ScheduleView,
};
use desk_signal_facade::{
    model::{connection::ConnectionState, signal::SignalingModel},
    service::{
        ControlFrameOutcome,
        schedule_management::{ScheduleManagementFailure, cookie_owner, reply},
    },
};
use desk_utils::error::DeskErrorCode;

pub async fn handle(
    db: &DatabaseConnection,
    actor: &ConnectionState,
    model: &SignalingModel,
    connections: actix_web::web::Data<desk_signal_facade::model::connection::SharedConnectionMap>,
    gate: std::sync::Arc<crate::ai_assistant_gate::AiAssistantGate>,
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
                if public_target(&db, owner, &current.target_device_id)
                    .await?
                    .is_none()
                {
                    return Err(ScheduleStoreError::NotFound);
                }
                let verifier = super::schedule_store::SignalTaskSubjectVerifier {
                    connections: &connections,
                    gate: &gate,
                };
                let task = store
                    .resume_task(owner, &schedule_id, expected_revision, &verifier)
                    .await?;
                Ok(Response::Task {
                    task: view(&db, owner, task).await?,
                })
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
            )
                .into(),
            ScheduleStoreError::Invalid | ScheduleStoreError::ToolInput(_) => (
                DeskErrorCode::INVALID_PARAMS,
                "invalid task management request",
            )
                .into(),
            ScheduleStoreError::Conflict => (
                DeskErrorCode::PRECONDITION_FAILED,
                "task changed; refresh before retrying",
            )
                .into(),
            ScheduleStoreError::BudgetExceeded => {
                (DeskErrorCode::PRECONDITION_FAILED, "task budget exceeded").into()
            }
            ScheduleStoreError::ExceedsSessionRetention { max_delay_seconds } => {
                ScheduleManagementFailure::exceeds_session_retention(max_delay_seconds)
            }
            ScheduleStoreError::Backend(_) => {
                (DeskErrorCode::SYSTEM_ERROR, "task management unavailable").into()
            }
        }),
    )
    .await
}

// Keep the dispatch state out of callers' futures and construction frames.
#[inline(never)]
pub(crate) fn manage(
    db: &DatabaseConnection,
    owner: i32,
    request: Request,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<Response, ScheduleStoreError>> + '_>>
{
    Box::pin(manage_inner(db, owner, request))
}

async fn manage_inner(
    db: &DatabaseConnection,
    owner: i32,
    request: Request,
) -> Result<Response, ScheduleStoreError> {
    let store = ScheduleStore::new(db.clone());
    let row = match request {
        Request::ResumeTask { .. } => {
            return Err(ScheduleStoreError::Conflict);
        }
        Request::ListResumeSources {
            target_device_id,
            offset,
            limit,
        } => {
            return desk_diagnose_core::future::boxed(|| {
                resume_sources::list(db, owner, &target_device_id, offset, limit)
            })
            .await;
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
            source_conversation_id,
            attention_only,
        } => {
            let after = match after {
                Some(id) => {
                    desk_diagnose_core::future::boxed(|| store.read(owner, &id))
                        .await?
                        .id
                }
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
            let (rows, total, attention_count) = desk_diagnose_core::future::boxed(|| {
                store.search(
                    owner,
                    after,
                    u64::from(limit),
                    kind.as_ref().and_then(|value| value.as_str()),
                    status.as_ref().and_then(|value| value.as_str()),
                    title.as_deref(),
                    target_device_id.as_deref(),
                    source_conversation_id.as_deref(),
                    attention_only,
                )
            })
            .await?;
            let next_cursor =
                (rows.len() == limit as usize).then(|| rows.last().unwrap().schedule_id.clone());
            let mut tasks = Vec::with_capacity(rows.len());
            for row in rows {
                tasks.push(desk_diagnose_core::future::boxed(|| view(db, owner, row)).await?);
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
                Some(id) => {
                    desk_diagnose_core::future::boxed(|| store.read(owner, &id))
                        .await?
                        .id
                }
                None => 0,
            };
            let rows =
                desk_diagnose_core::future::boxed(|| store.list(owner, after, u64::from(limit)))
                    .await?;
            let next_cursor =
                (rows.len() == limit as usize).then(|| rows.last().unwrap().schedule_id.clone());
            let mut tasks = Vec::with_capacity(rows.len());
            for row in rows {
                tasks.push(desk_diagnose_core::future::boxed(|| view(db, owner, row)).await?);
            }
            return Ok(Response::List { tasks, next_cursor });
        }
        Request::ListRuns {
            schedule_id,
            before,
            limit,
        } => {
            let page = desk_diagnose_core::future::boxed(|| {
                store.run_history(owner, &schedule_id, before.as_deref(), limit)
            })
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
            desk_diagnose_core::future::boxed(|| {
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
            })
            .await?;
            desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?
        }
        Request::RevokeRunDirectory {
            schedule_id,
            run_id,
            directory_request_id,
            expected_scope_revision,
            client_request_key,
        } => {
            desk_diagnose_core::future::boxed(|| {
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
            })
            .await?;
            desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?
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
            desk_diagnose_core::future::boxed(|| {
                store.dispose_run_outcome(
                    owner,
                    &schedule_id,
                    &run_id,
                    expected_revision,
                    &client_request_key,
                    &note,
                    work_id,
                    &execution_id,
                )
            })
            .await?;
            desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?
        }
        Request::AcknowledgeRunOutcome {
            schedule_id,
            run_id,
            expected_revision,
            client_request_key,
            note,
        } => {
            desk_diagnose_core::future::boxed(|| {
                store.acknowledge_run_outcome(
                    owner,
                    &schedule_id,
                    &run_id,
                    expected_revision,
                    &client_request_key,
                    &note,
                )
            })
            .await?;
            desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?
        }
        Request::RunTaskNow {
            schedule_id,
            expected_revision,
            client_request_key,
        } => {
            desk_diagnose_core::future::boxed(|| {
                store.enqueue_manual_at_revision(
                    owner,
                    &schedule_id,
                    &client_request_key,
                    expected_revision,
                )
            })
            .await?;
            desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?
        }
        Request::CancelTaskRun { run_id } => {
            let work =
                desk_diagnose_core::future::send_phase(|| store.cancel_run(owner, &run_id)).await?;
            desk_diagnose_core::future::boxed(|| store.read(owner, &work.schedule_id)).await?
        }
        Request::Get { schedule_id } => {
            desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?
        }
        Request::CreateTask { mut task } => {
            // Only the model proposal transaction can stamp AI origin.
            task.creation_source = desk_agent_protocol::schedule::ScheduleCreationSource::Manual;
            let task = desk_diagnose_core::future::boxed(|| resolve_draft(db, owner, task)).await?;
            desk_diagnose_core::future::boxed(|| store.create_task(owner, &task)).await?
        }
        Request::DecideScheduleProposal {
            schedule_id,
            expected_revision,
            approve,
        } => {
            let task =
                desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?;
            if public_target(db, owner, &task.target_device_id)
                .await?
                .is_none()
            {
                return Err(ScheduleStoreError::NotFound);
            }
            desk_diagnose_core::future::boxed(|| {
                store.decide_proposal(
                    owner,
                    &schedule_id,
                    expected_revision,
                    approve,
                    crate::schedule_store::ProposalDecider::Owner,
                )
            })
            .await?
        }
        Request::Rename {
            schedule_id,
            expected_revision,
            title,
        } => {
            desk_diagnose_core::future::boxed(|| {
                store.rename(owner, &schedule_id, expected_revision, &title)
            })
            .await?
        }
        Request::SetFailureThreshold {
            schedule_id,
            expected_revision,
            failure_threshold,
        } => {
            desk_diagnose_core::future::boxed(|| {
                store.set_failure_threshold(
                    owner,
                    &schedule_id,
                    expected_revision,
                    failure_threshold,
                )
            })
            .await?
        }
        Request::ChangePrompt {
            schedule_id,
            expected_revision,
            prompt,
        } => {
            let task =
                desk_diagnose_core::future::boxed(|| store.read(owner, &schedule_id)).await?;
            if public_target(db, owner, &task.target_device_id)
                .await?
                .is_none()
            {
                return Err(ScheduleStoreError::NotFound);
            }
            desk_diagnose_core::future::boxed(|| {
                store.change_prompt(owner, &schedule_id, expected_revision, &prompt)
            })
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
            desk_diagnose_core::future::boxed(|| {
                store.change_time(owner, &schedule_id, expected_revision, &spec)
            })
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
                    desk_diagnose_core::future::boxed(|| store.database_time()).await?,
                )
                .await?
        }
        Request::Delete {
            schedule_id,
            expected_revision,
        } => {
            desk_diagnose_core::future::boxed(|| {
                store.delete(owner, &schedule_id, expected_revision)
            })
            .await?
        }
    };
    Ok(Response::Task {
        task: desk_diagnose_core::future::boxed(|| view(db, owner, row)).await?,
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
    let upcoming_runs = if matches!(row.status.as_str(), "deleted" | "completed" | "expired")
        || (row.status == "pending_review"
            && matches!(
                spec.rule,
                desk_agent_protocol::schedule::ScheduleRule::AfterConfirmation { .. }
            )) {
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
    use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let Some(source) = draft.source_conversation_id.as_deref() else {
        return Ok(());
    };
    if desk_agent_protocol::ai_assistant::source::validate_source_id(source).is_err() {
        return Err(ScheduleStoreError::Invalid);
    }
    let actor = owner.to_string();
    let key = source.to_owned();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&key))
        .filter(session_row::Column::ActorId.eq(&actor))
        .filter(session_row::Column::DeviceId.is_in([draft.target_device_id.as_str(), ""]))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.conversation_id != key
        || session.check_subject(&actor, &row.device_id).is_err()
        || session
            .check_surface(AgentSessionSurface::AiAssistant)
            .is_err()
    {
        return Err(ScheduleStoreError::NotFound);
    }
    if !session
        .admitted_assistant_request_targets()
        .is_some_and(|targets| {
            targets
                .iter()
                .any(|target| target.device_id == draft.target_device_id)
        })
    {
        return Err(ScheduleStoreError::Invalid);
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
    if id.is_empty() {
        return Ok(None);
    }
    Ok(Some(id.into()))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn run_history_projects_only_public_occurrence_metadata() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let (store, task) =
            crate::schedule_store::tests::enabled_fresh_task_on(db.clone(), None, 1, None).await;
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

    use super::*;
    use desk_agent_protocol::schedule::{
        ScheduleCreationSource, ScheduleDraft, ScheduleRule, ScheduleSpec, ScheduledTaskKind,
        ScheduledTaskStatus,
    };
    use sea_orm::{ConnectionTrait, EntityTrait, PaginatorTrait, Schema};

    async fn fixture() -> DatabaseConnection {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.entity_table(crate::entity::agent_assistant_runtime::Entity))
            .await
            .unwrap();
        db.execute(&schema.entity_table(row::Entity)).await.unwrap();
        db.execute(&schema.entity_table(crate::entity::agent_schedule_run::Entity))
            .await
            .unwrap();
        crate::db::ensure_lifecycle_tables(&db).await;
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
        db.execute(&schema.entity_table(session_row::Entity))
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
        session.surface = AgentSessionSurface::AiAssistant;
        session.client_conversation_id = Some("chat-1".into());
        session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
        session.input_revision = 1;
        session.bind_assistant_input_targets(&[]).unwrap();
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
        request.spec.rule = ScheduleRule::AfterConfirmation { delay_seconds: 300 };
        request.source_conversation_id = Some(key.clone());
        request.creation_source = ScheduleCreationSource::AiProposal;
        request.requirement_revision = Some(1);
        // Model proposals must be readable before the owner decides; their delay
        // has no absolute execution time until approval.
        let mut proposal = resolve_draft(&db, 1, request.clone()).await.unwrap();
        proposal.client_create_key = "pending-review-detail".into();
        let store = ScheduleStore::new(db.clone());
        let pending = store
            .create_draft(1, &proposal, chrono::Utc::now().timestamp_millis())
            .await
            .unwrap();
        let reviewed = task(
            manage(
                &db,
                1,
                Request::Get {
                    schedule_id: pending.schedule_id.clone(),
                },
            )
            .await
            .unwrap(),
        );
        assert_eq!(reviewed.status, ScheduledTaskStatus::PendingReview);
        assert!(reviewed.upcoming_runs.is_empty());
        assert!(reviewed.next_run_at.is_none());
        assert_eq!(
            reviewed.spec.rule,
            ScheduleRule::AfterConfirmation { delay_seconds: 300 }
        );
        let rejected = task(
            manage(
                &db,
                1,
                Request::Delete {
                    schedule_id: pending.schedule_id,
                    expected_revision: pending.revision,
                },
            )
            .await
            .unwrap(),
        );
        assert_eq!(rejected.status, ScheduledTaskStatus::Deleted);
        let created = task(
            manage(
                &db,
                1,
                Request::CreateTask {
                    task: request.clone(),
                },
            )
            .await
            .unwrap(),
        );
        assert!(!created.upcoming_runs.is_empty());
        assert!(matches!(created.spec.rule, ScheduleRule::Once { .. }));
        let fetched = task(
            manage(
                &db,
                1,
                Request::Get {
                    schedule_id: created.schedule_id.clone(),
                },
            )
            .await
            .unwrap(),
        );
        assert_eq!(fetched.spec, created.spec);
        assert!(
            manage(
                &db,
                2,
                Request::DecideScheduleProposal {
                    schedule_id: created.schedule_id.clone(),
                    expected_revision: created.revision,
                    approve: true
                }
            )
            .await
            .is_err()
        );
        let activated = created.clone();
        assert_eq!(
            activated.status,
            desk_agent_protocol::schedule::ScheduledTaskStatus::Active
        );
        assert!(activated.next_run_at.is_some());
        assert!(
            manage(
                &db,
                1,
                Request::DecideScheduleProposal {
                    schedule_id: created.schedule_id.clone(),
                    expected_revision: created.revision,
                    approve: true
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
        assert_eq!(stored.creation_source, "manual");
        assert_eq!(created.status, ScheduledTaskStatus::Active);
        let mut stale = request.clone();
        stale.requirement_revision = Some(2);
        assert!(matches!(
            manage(&db, 1, Request::CreateTask { task: stale }).await,
            Err(ScheduleStoreError::Conflict)
        ));
        let mut raw = request.clone();
        raw.source_conversation_id = Some("chat-1".into());
        assert!(matches!(
            manage(&db, 1, Request::CreateTask { task: raw }).await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert!(matches!(
            manage(
                &db,
                2,
                Request::CreateTask {
                    task: request.clone()
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
                Request::CreateTask {
                    task: request.clone()
                }
            )
            .await,
            Err(ScheduleStoreError::NotFound)
        ));
        session.actor_id = "1".into();
        session.surface = AgentSessionSurface::TerminalAiAssistant;
        let mut changed: session_row::ActiveModel = row.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.update(&db).await.unwrap();
        assert!(matches!(
            manage(&db, 1, Request::CreateTask { task: request }).await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert_eq!(row::Entity::find().count(&db).await.unwrap(), 2);
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
    async fn management_creates_enabled_tasks_projects_public_identity_and_preserves_owner_cas() {
        let db = fixture().await;
        let original = task(
            manage(&db, 1, Request::CreateTask { task: draft() })
                .await
                .unwrap(),
        );
        assert_eq!(
            original.target_device_id.as_deref(),
            Some("public-device-1")
        );
        assert_eq!(original.status, ScheduledTaskStatus::Active);
        assert!(original.next_run_at.is_some());
        assert!(original.active_run_id.is_none());
        assert!(original.created_at.ends_with('Z'));
        let replay = task(
            manage(&db, 1, Request::CreateTask { task: draft() })
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

mod run_directory;
