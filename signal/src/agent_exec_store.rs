//! Durable background executions for the single-node OSS signal brain.

use std::time::Duration;

use chrono::{DateTime, Utc};
use desk_agent_protocol::edge_exec::EdgeExecDisposition;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

use crate::agent_session_store::{EventAppend, SignalAgentSessionStore};
use crate::entity::agent_exec_task;

pub const STATUS_DISPATCHING: &str = "dispatching";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_DONE: &str = "done";
pub const STATUS_UNKNOWN: &str = "unknown";
pub const DELIVERY_PENDING: &str = "pending";
pub const DELIVERY_CONSUMED: &str = "consumed";

fn internal(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

fn disposition_text(disposition: &EdgeExecDisposition) -> Option<String> {
    match disposition {
        EdgeExecDisposition::Executed { outcome } => Some(match outcome {
            AgentOutcome::Ok(output) => {
                serde_json::to_string(output).unwrap_or_else(|_| "{}".to_string())
            }
            AgentOutcome::Err(error) if error.safe_for_model => {
                format!("execution failed: {}", error.message)
            }
            AgentOutcome::Err(_) => "execution failed".to_string(),
        }),
        EdgeExecDisposition::RejectedBeforeDispatch { error }
        | EdgeExecDisposition::DispatchFailedBeforeWorker { error }
        | EdgeExecDisposition::HostAtCapacity { error } => Some(if error.safe_for_model {
            format!("execution did not complete: {}", error.message)
        } else {
            "execution did not complete".to_string()
        }),
        EdgeExecDisposition::ExecutionStateUnknown { .. } => None,
    }
}

#[derive(Clone)]
pub struct SignalAgentExecStore {
    db: DatabaseConnection,
}

impl SignalAgentExecStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn create(
        &self,
        exec_request_id: &str,
        execution_generation: &str,
        conversation_id: &str,
        tool_call_id: &str,
        target_connection_id: &str,
        deadline: DateTime<Utc>,
    ) -> Result<agent_exec_task::Model, AgentError> {
        let now = Utc::now();
        let event_id = format!("signal-exec:{exec_request_id}:done");
        agent_exec_task::ActiveModel {
            exec_request_id: Set(exec_request_id.to_string()),
            execution_generation: Set(execution_generation.to_string()),
            conversation_id: Set(conversation_id.to_string()),
            tool_call_id: Set(tool_call_id.to_string()),
            target_connection_id: Set(target_connection_id.to_string()),
            status: Set(STATUS_DISPATCHING.to_string()),
            disposition_json: Set(None),
            result_text: Set(None),
            event_id: Set(event_id),
            delivery_state: Set(DELIVERY_PENDING.to_string()),
            deadline: Set(deadline),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .map_err(|e| internal(format!("create agent execution task: {e}")))
    }

    pub async fn mark_running(&self, execution_generation: &str) -> Result<(), AgentError> {
        agent_exec_task::Entity::update_many()
            .col_expr(agent_exec_task::Column::Status, Expr::value(STATUS_RUNNING))
            .col_expr(agent_exec_task::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(agent_exec_task::Column::ExecutionGeneration.eq(execution_generation))
            .filter(agent_exec_task::Column::Status.eq(STATUS_DISPATCHING))
            .exec(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| internal(format!("mark agent execution running: {e}")))
    }

    pub async fn mark_unsent(&self, execution_generation: &str) -> Result<(), AgentError> {
        agent_exec_task::Entity::update_many()
            .col_expr(agent_exec_task::Column::Status, Expr::value(STATUS_DONE))
            .col_expr(
                agent_exec_task::Column::DeliveryState,
                Expr::value(DELIVERY_CONSUMED),
            )
            .col_expr(
                agent_exec_task::Column::ResultText,
                Expr::value(Some("execution was not sent to the host".to_string())),
            )
            .col_expr(agent_exec_task::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(agent_exec_task::Column::ExecutionGeneration.eq(execution_generation))
            .exec(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| internal(format!("settle unsent agent execution: {e}")))
    }

    /// Persist the terminal before waking any foreground waiter. A duplicate host
    /// result is harmless because only a non-terminal row may transition.
    pub async fn finalize(
        &self,
        source_connection_id: &str,
        execution_generation: &str,
        disposition: &EdgeExecDisposition,
    ) -> Result<Option<agent_exec_task::Model>, AgentError> {
        let Some(row) = self.find_by_generation(execution_generation).await? else {
            return Ok(None);
        };
        if row.target_connection_id != source_connection_id {
            return Ok(None);
        }
        if matches!(row.status.as_str(), STATUS_DONE | STATUS_UNKNOWN) {
            return Ok(Some(row));
        }
        let status = if matches!(
            disposition,
            EdgeExecDisposition::ExecutionStateUnknown { .. }
        ) {
            STATUS_UNKNOWN
        } else {
            STATUS_DONE
        };
        let disposition_json = serde_json::to_string(disposition)
            .map_err(|e| internal(format!("encode agent execution result: {e}")))?;
        agent_exec_task::Entity::update_many()
            .col_expr(agent_exec_task::Column::Status, Expr::value(status))
            .col_expr(
                agent_exec_task::Column::DispositionJson,
                Expr::value(Some(disposition_json)),
            )
            .col_expr(
                agent_exec_task::Column::ResultText,
                Expr::value(disposition_text(disposition)),
            )
            .col_expr(agent_exec_task::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(agent_exec_task::Column::Id.eq(row.id))
            .filter(agent_exec_task::Column::Status.is_in([STATUS_DISPATCHING, STATUS_RUNNING]))
            .exec(&self.db)
            .await
            .map_err(|e| internal(format!("finalize agent execution: {e}")))?;
        self.find_by_generation(execution_generation).await
    }

    pub async fn find(
        &self,
        exec_request_id: &str,
        execution_generation: &str,
    ) -> Result<Option<agent_exec_task::Model>, AgentError> {
        agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ExecRequestId.eq(exec_request_id))
            .filter(agent_exec_task::Column::ExecutionGeneration.eq(execution_generation))
            .one(&self.db)
            .await
            .map_err(|e| internal(format!("load agent execution task: {e}")))
    }

    pub async fn find_by_generation(
        &self,
        execution_generation: &str,
    ) -> Result<Option<agent_exec_task::Model>, AgentError> {
        agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ExecutionGeneration.eq(execution_generation))
            .one(&self.db)
            .await
            .map_err(|e| internal(format!("load agent execution generation: {e}")))
    }

    pub async fn consume_event(&self, event_id: &str) -> Result<(), AgentError> {
        agent_exec_task::Entity::update_many()
            .col_expr(
                agent_exec_task::Column::DeliveryState,
                Expr::value(DELIVERY_CONSUMED),
            )
            .col_expr(agent_exec_task::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(agent_exec_task::Column::EventId.eq(event_id))
            .filter(agent_exec_task::Column::DeliveryState.eq(DELIVERY_PENDING))
            .exec(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| internal(format!("consume agent execution delivery: {e}")))
    }

    async fn pending_deliveries(&self) -> Result<Vec<agent_exec_task::Model>, AgentError> {
        agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::DeliveryState.eq(DELIVERY_PENDING))
            .filter(agent_exec_task::Column::Status.is_in([STATUS_DONE, STATUS_UNKNOWN]))
            .order_by_asc(agent_exec_task::Column::Id)
            .limit(32)
            .all(&self.db)
            .await
            .map_err(|e| internal(format!("load pending agent execution deliveries: {e}")))
    }

    async fn publish_once(&self) -> Result<(), AgentError> {
        let sessions = SignalAgentSessionStore::new(self.db.clone());
        for task in self.pending_deliveries().await? {
            let now = Utc::now().to_rfc3339();
            let outcome = if task.status == STATUS_UNKNOWN {
                sessions
                    .mark_execution_unknown(
                        &task.conversation_id,
                        &task.execution_generation,
                        &task.tool_call_id,
                        &now,
                    )
                    .await?
            } else {
                sessions
                    .deliver_completion(
                        &task.conversation_id,
                        task.id,
                        &task.event_id,
                        &task.execution_generation,
                        &task.tool_call_id,
                        &task.exec_request_id,
                        task.result_text.as_deref().unwrap_or("execution completed"),
                        &now,
                    )
                    .await?
            };
            if matches!(outcome, EventAppend::Appended | EventAppend::AlreadyPresent) {
                if task.status == STATUS_UNKNOWN
                    || self.follow_up_completion(&sessions, &task, &now).await?
                {
                    self.consume_event(&task.event_id).await?;
                }
            }
        }
        Ok(())
    }

    /// Run the completion-triggered, read-only model turn before consuming the
    /// durable delivery. Returning `false` leaves the task pending for the next
    /// publisher tick (a live user turn owns the conversation, or a transient
    /// model error still has retry budget).
    async fn follow_up_completion(
        &self,
        sessions: &SignalAgentSessionStore,
        task: &agent_exec_task::Model,
        now: &str,
    ) -> Result<bool, AgentError> {
        const MAX_AUTO_FOLLOW_UP_TURNS: u32 = 3;

        let Some(session) = sessions
            .pending_auto_trigger(&task.conversation_id, &task.event_id)
            .await?
        else {
            // The foreground path already reacted to this result, or a prior
            // automatic turn drained it before the publisher was interrupted.
            return Ok(true);
        };
        let Some(pending) = session
            .pending_auto_triggers
            .iter()
            .find(|pending| pending.event_id == task.event_id)
        else {
            return Ok(true);
        };
        let expired = session
            .scope_snapshot
            .expires_at
            .as_ref()
            .is_some_and(|raw| {
                DateTime::parse_from_rfc3339(raw)
                    .map(|deadline| deadline < Utc::now())
                    .unwrap_or(true)
            });
        if pending.chain_id != session.chain_id
            || session.automation_turns_used >= MAX_AUTO_FOLLOW_UP_TURNS
            || expired
        {
            return Ok(!matches!(
                sessions
                    .prune_auto_trigger(&task.conversation_id, &task.event_id, now)
                    .await?,
                EventAppend::Busy
            ));
        }

        match crate::agent_runtime::resume_completion_turn(
            self.db.clone(),
            session,
            desk_diagnose_core::session::WorkKind::AgentExec,
        )
        .await
        {
            Ok(desk_diagnose_core::agent_loop::LoopOutcome::TurnBusy) => Ok(false),
            Ok(outcome) => {
                log::info!(
                    "[agent-exec] automatic completion follow-up settled \
                     conversation={} event={} outcome={outcome:?}",
                    task.conversation_id,
                    task.event_id
                );
                // Answer/tool-use reactions drain the trigger themselves. A
                // truncated or circuit-broken turn deliberately leaves it; this
                // attempt already spent its bounded automation budget, so remove
                // the entry instead of redialing the same completion immediately.
                if sessions
                    .pending_auto_trigger(&task.conversation_id, &task.event_id)
                    .await?
                    .is_some()
                {
                    return Ok(!matches!(
                        sessions
                            .prune_auto_trigger(
                                &task.conversation_id,
                                &task.event_id,
                                &Utc::now().to_rfc3339(),
                            )
                            .await?,
                        EventAppend::Busy
                    ));
                }
                Ok(true)
            }
            Err(error) => {
                log::warn!(
                    "[agent-exec] automatic completion follow-up failed \
                     conversation={} event={}: {}",
                    task.conversation_id,
                    task.event_id,
                    error.message
                );
                let exhausted = sessions
                    .pending_auto_trigger(&task.conversation_id, &task.event_id)
                    .await?
                    .is_some_and(|latest| latest.automation_turns_used >= MAX_AUTO_FOLLOW_UP_TURNS);
                if exhausted {
                    Ok(!matches!(
                        sessions
                            .prune_auto_trigger(
                                &task.conversation_id,
                                &task.event_id,
                                &Utc::now().to_rfc3339(),
                            )
                            .await?,
                        EventAppend::Busy
                    ))
                } else {
                    Ok(false)
                }
            }
        }
    }
}

/// Start the crash-replayable completion publisher after schema initialization
/// has made the task table available. The database is authoritative; this loop
/// is only a delivery worker and can restart freely.
pub fn start_completion_publisher(db: DatabaseConnection) {
    // The auto-follow-up model seam uses `awc` and is intentionally `!Send`;
    // portable Signal runs on Actix's local arbiter, so keep the publisher there.
    actix_web::rt::spawn(async move {
        let store = SignalAgentExecStore::new(db);
        loop {
            if let Err(error) = store.publish_once().await {
                log::warn!(
                    "[agent-exec] completion publisher failed: {}",
                    error.message
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::seam::{ClaimTurnParams, SessionSeam};
    use desk_diagnose_core::session::{ExecutionState, TriggerOrigin, TurnState};
    use sea_orm::Database;

    use super::*;

    async fn store() -> SignalAgentExecStore {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        SignalAgentExecStore::new(db)
    }

    #[test]
    fn cancelled_execution_is_preserved_for_the_automatic_follow_up() {
        let disposition = EdgeExecDisposition::Executed {
            outcome: AgentOutcome::Err(AgentError {
                kind: AgentErrorKind::Cancelled,
                message: "the command was cancelled and its process tree reclaimed".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }),
        };

        assert_eq!(
            disposition_text(&disposition).as_deref(),
            Some("execution failed: the command was cancelled and its process tree reclaimed")
        );
    }

    #[tokio::test]
    async fn result_is_source_bound_and_persisted_before_delivery() {
        let store = store().await;
        store
            .create(
                "task-1",
                "generation-1",
                "conversation-1",
                "call-1",
                "edge-a",
                Utc::now() + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();
        store.mark_running("generation-1").await.unwrap();
        let disposition = EdgeExecDisposition::RejectedBeforeDispatch {
            error: EdgeExecDisposition::safe_error(
                AgentErrorKind::PermissionDenied,
                "policy changed",
                false,
            ),
        };

        assert!(
            store
                .finalize("edge-b", "generation-1", &disposition)
                .await
                .unwrap()
                .is_none()
        );
        let running = store.find("task-1", "generation-1").await.unwrap().unwrap();
        assert_eq!(running.status, STATUS_RUNNING);

        let done = store
            .finalize("edge-a", "generation-1", &disposition)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert_eq!(done.delivery_state, DELIVERY_PENDING);
        assert_eq!(
            done.result_text.as_deref(),
            Some("execution did not complete: policy changed")
        );
    }

    #[tokio::test]
    async fn foreground_ack_consumes_the_stable_delivery() {
        let store = store().await;
        let task = store
            .create(
                "task-2",
                "generation-2",
                "conversation-2",
                "call-2",
                "edge-a",
                Utc::now() + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();

        store.consume_event(&task.event_id).await.unwrap();
        let consumed = store.find("task-2", "generation-2").await.unwrap().unwrap();
        assert_eq!(consumed.delivery_state, DELIVERY_CONSUMED);
    }

    #[tokio::test]
    async fn publisher_queues_follow_up_and_prunes_an_expired_chain() {
        let store = store().await;
        let sessions = SignalAgentSessionStore::new(store.db.clone());
        let mut session = sessions
            .claim_turn(ClaimTurnParams {
                conversation_id: "conversation-expired".into(),
                actor_id: "1".into(),
                device_id: "device-1".into(),
                policy_revision: 0,
                current_pdp_scope: AgentScope {
                    granted: vec![],
                    mode: ExecutionMode::ReadOnly,
                    expires_at: Some("2020-01-01T00:00:00Z".into()),
                    policy_name: None,
                },
                turn_id: "turn-1".into(),
                request_id: Some("request-1".into()),
                connection_id: Some("browser-1".into()),
                trigger_origin: TriggerOrigin::User,
                now: Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        session.execution_state = ExecutionState::Executing {
            action: desk_diagnose_core::session::ActionIdentity::agent_exec(
                1,
                "task-expired",
                "generation-expired",
            ),
        };
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        sessions.save(&mut session).await.unwrap();

        let task = store
            .create(
                "task-expired",
                "generation-expired",
                "conversation-expired",
                "call-expired",
                "edge-a",
                Utc::now() + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();
        store.mark_running("generation-expired").await.unwrap();
        store
            .finalize(
                "edge-a",
                "generation-expired",
                &EdgeExecDisposition::RejectedBeforeDispatch {
                    error: EdgeExecDisposition::safe_error(
                        AgentErrorKind::Timeout,
                        "timed out",
                        true,
                    ),
                },
            )
            .await
            .unwrap();

        store.publish_once().await.unwrap();

        let delivered = store
            .find("task-expired", "generation-expired")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered.event_id, task.event_id);
        assert_eq!(delivered.delivery_state, DELIVERY_CONSUMED);
        let persisted = sessions
            .read_snapshot("conversation-expired")
            .await
            .unwrap()
            .unwrap();
        assert!(
            persisted
                .messages
                .iter()
                .any(|message| message.message_id == task.event_id)
        );
        assert!(
            sessions
                .pending_auto_trigger("conversation-expired", &task.event_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
