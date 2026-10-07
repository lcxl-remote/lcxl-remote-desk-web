//! Owner snapshot projection; command records never grant authority to execute.
use super::*;
use crate::entity::agent_exec_task as task;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

pub(super) async fn list(
    db: &sea_orm::DatabaseConnection,
    run: &str,
) -> Result<Vec<CommandTaskDto>, DeskSignalError> {
    let mut result = Vec::new();
    // Keep active tasks visible even when the conversation has extensive history.
    for active in [true, false] {
        let query = task::Entity::find().filter(task::Column::ConversationId.eq(run));
        let query = if active {
            query.filter(task::Column::Status.is_in(["dispatching", "running", "unknown"]))
        } else {
            query.filter(task::Column::Status.is_not_in(["dispatching", "running", "unknown"]))
        };
        let rows = query
            .order_by_desc(task::Column::UpdatedAt)
            .order_by_desc(task::Column::Id)
            .limit(if active { 128 } else { 50 })
            .all(db)
            .await?;
        for row in rows {
            let disposition = row.disposition_json.as_deref().and_then(|value| {
                serde_json::from_str::<desk_agent_protocol::edge_exec::EdgeExecDisposition>(value)
                    .ok()
            });
            use desk_agent_protocol::edge_exec::EdgeExecDisposition;
            let outcome = match disposition {
                Some(EdgeExecDisposition::Executed { outcome }) => Some(outcome),
                Some(
                    EdgeExecDisposition::RejectedBeforeDispatch { error }
                    | EdgeExecDisposition::DispatchFailedBeforeWorker { error }
                    | EdgeExecDisposition::HostAtCapacity { error },
                ) => Some(desk_agent_protocol::AgentOutcome::Err(error)),
                _ => None,
            };
            result.push(CommandTaskDto::project(
                row.exec_request_id,
                row.tool_call_id,
                row.execution_generation,
                if row.cancel_requested_at.is_some()
                    && matches!(row.status.as_str(), "dispatching" | "running" | "unknown")
                {
                    "cancel_requested"
                } else {
                    &row.status
                },
                row.updated_at.to_rfc3339(),
                outcome,
            ));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[actix_web::test]
    async fn command_task_list_keeps_sessions_separate_and_active_tasks_visible() {
        use sea_orm::{ConnectionTrait, Database, Schema};
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute(&Schema::new(db.get_database_backend()).create_table_from_entity(task::Entity))
            .await
            .unwrap();
        let store = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
        store
            .create(
                "request-a",
                "generation-a",
                "run-a",
                "call-a",
                "device",
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        store
            .create(
                "request-b",
                "generation-b",
                "run-b",
                "call-b",
                "device",
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        let tasks = list(&db, "run-a").await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].execution_generation, "generation-a");
        assert!(list(&db, "unrelated-run").await.unwrap().is_empty());
    }
}

#[utoipa::path(tag = TAG, summary = "Request a durable stop for one original AI command generation",
    request_body = AiAssistantCommandCancelBody, responses((status = 200, body = RestResponse<bool>)))]
#[post("/my/ai-assistant-session/command/cancel")]
pub async fn cancel_ai_assistant_command(
    connections: web::Data<SharedConnectionMap>,
    session: Session,
    body: web::Json<AiAssistantCommandCancelBody>,
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
    let db = crate::db::get_db();
    let Some((run, device)) = recovery::resolve(
        &SignalAgentSessionStore::new(db.clone()),
        connections.get_ref(),
        &actor,
        &body.connection,
        Some(&body.session),
        None,
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let requested = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .cancel_command_for_owner(
            &run,
            &actor,
            &device,
            &body.exec_request_id,
            &body.execution_generation,
        )
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "Original command not found or not accessible",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(requested)))
}
