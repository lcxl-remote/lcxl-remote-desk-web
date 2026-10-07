//! Durable cancellation delivery is independent of any model turn or main lease.
use super::*;
use crate::entity::agent_exec_task as task;
use desk_signal_facade::model::{auth_context::AuthKind, signal::RemoteDeskTypeEnum};
use futures_util::{StreamExt, stream};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    id: i64,
    generation: String,
    target_connection_id: String,
    actor_id: String,
    audience: String,
    origin: desk_diagnose_core::action_result::ActionResultOrigin,
    requested_at: chrono::DateTime<chrono::Utc>,
}

fn unavailable() -> AgentError {
    internal("original command stop unavailable")
}

/// Read only the original native task and dispatch facts. Current parent input,
/// grants, model binding and task terminal state cannot relabel this execution.
async fn candidate(db: &DatabaseConnection, id: i64) -> Result<Option<Candidate>, AgentError> {
    let txn = db.begin().await.map_err(|_| unavailable())?;
    let row = task::Entity::find_by_id(id)
        .one(&txn)
        .await
        .map_err(|_| unavailable())?;
    let Some(row) = row.filter(|row| {
        row.cancel_requested_at.is_some()
            && matches!(row.status.as_str(), "dispatching" | "running" | "unknown")
    }) else {
        return Ok(None);
    };
    let (original, work, payload) = crate::capability_grant_store::computer_binding::original_on(
        &txn,
        &row.execution_generation,
    )
    .await
    .map_err(|_| unavailable())?;
    let origin = payload.command_origin.as_ref().ok_or_else(unavailable)?;
    origin.validate().map_err(|_| unavailable())?;
    if original.payload_schema_version != 1
        || payload.work_id != work.id
        || original.work_id != payload.work_id
        || original.call_id != payload.call_id
        || original.dispatch_id != payload.dispatch_id
        || payload.dispatch_id != row.execution_generation
        || work.action_request_id != payload.call_id
        || payload.call_id != row.exec_request_id
        || work.conversation_id != row.conversation_id
        || work.actor_id != origin.turn_fence.actor_id
        || work.target_device_id != origin.turn_fence.device_id
        || work.turn_id != origin.turn_fence.turn_id
        || origin.turn_fence.conversation_id != row.conversation_id
        || origin.tool_call_id != row.tool_call_id
        || origin.tool_name != desk_diagnose_core::command_confirmation::COMMAND_TOOL
        || payload.tool_name != origin.tool_name
        || payload.provider_id != origin.provider_id
        || row.cancel_requested_by.as_deref() != Some(work.actor_id.as_str())
        || row.target_connection_id.is_empty()
        || work.target_device_id.is_empty()
    {
        return Err(unavailable());
    }
    let selected = Candidate {
        id: row.id,
        generation: row.execution_generation,
        target_connection_id: row.target_connection_id,
        actor_id: work.actor_id,
        audience: work.target_device_id,
        origin: origin.clone(),
        requested_at: row.cancel_requested_at.ok_or_else(unavailable)?,
    };
    txn.commit().await.map_err(|_| unavailable())?;
    Ok(Some(selected))
}

pub struct SignalCommandCancelDispatcher {
    db: DatabaseConnection,
    connections: Arc<SharedConnectionMap>,
    pending: Arc<SignalAgentExecPending>,
}

impl SignalCommandCancelDispatcher {
    pub fn new(db: DatabaseConnection, connections: Arc<SharedConnectionMap>) -> Self {
        Self {
            db,
            connections,
            pending: global_agent_exec_pending(),
        }
    }

    pub async fn scan_once(&self, after: i64) -> Result<(Option<i64>, usize), AgentError> {
        let rows = task::Entity::find()
            .filter(task::Column::Id.gt(after))
            .filter(task::Column::CancelRequestedAt.is_not_null())
            .filter(task::Column::Status.is_in(["dispatching", "running", "unknown"]))
            .order_by_asc(task::Column::Id)
            .limit(33)
            .all(&self.db)
            .await
            .map_err(|_| unavailable())?;
        let next = (rows.len() > 32).then(|| rows[31].id);
        let results = stream::iter(
            rows.into_iter()
                .take(32)
                .map(|row| self.send_original(row.id)),
        )
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        let mut sent = 0;
        for result in results {
            match result {
                Ok(true) => sent += 1,
                Ok(false) => {}
                Err(_) => log::warn!(
                    "[agent-exec] original stop delivery unavailable; native outcome unchanged"
                ),
            }
        }
        Ok((next, sent))
    }

    async fn send_original(&self, id: i64) -> Result<bool, AgentError> {
        let Some(selected) = candidate(&self.db, id).await? else {
            return Ok(false);
        };
        let target = self
            .connections
            .read()
            .await
            .get(&selected.target_connection_id)
            .cloned();
        let Some(target) = target else {
            return Ok(false);
        };
        if target.model.connection_id != selected.target_connection_id
            || target.model.version_info.client_id.as_deref() != Some(selected.audience.as_str())
            || target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Err(unavailable());
        }
        let reply = self.pending.register_state_query(
            selected.generation.clone(),
            selected.target_connection_id.clone(),
        );
        let sent = tokio::time::timeout(Duration::from_secs(5), async {
            let mut socket = target.session.write().await;
            if candidate(&self.db, id).await?.as_ref() != Some(&selected) {
                return Ok(false);
            }
            let frame = SignalingModel::new(
                &selected.generation,
                SignalingType::ControlExecution,
                None,
                Some(selected.target_connection_id.clone()),
                Some(
                    serde_json::to_value(ExecControlPayload {
                        execution_generation: selected.generation.clone(),
                        action: ExecControlAction::Cancel {
                            requested_by: selected.actor_id.clone(),
                        },
                    })
                    .map_err(|_| unavailable())?,
                ),
                None,
            );
            socket
                .text(serde_json::to_string(&frame).map_err(|_| unavailable())?)
                .await
                .map_err(|_| unavailable())?;
            Ok(true)
        })
        .await
        .map_err(|_| unavailable())
        .and_then(|sent| sent);
        match sent {
            Ok(true) => {}
            other => {
                let owned_waiter = reply.is_some();
                drop(reply);
                if owned_waiter {
                    self.pending.cancel_state_query(&selected.generation);
                }
                return other;
            }
        }
        if let Some(reply) = reply {
            match tokio::time::timeout(Duration::from_secs(5), reply).await {
                Ok(Ok(reply))
                    if reply.execution_generation == selected.generation
                        && reply.state.is_settled() =>
                {
                    let disposition = EdgeExecDisposition::from_reconciled_state(&reply);
                    crate::agent_exec_store::SignalAgentExecStore::new(self.db.clone())
                        .finalize(
                            &selected.target_connection_id,
                            &selected.generation,
                            &disposition,
                        )
                        .await?;
                }
                Ok(Ok(_)) => {}
                _ => self.pending.cancel_state_query(&selected.generation),
            }
        }
        Ok(true)
    }

    pub async fn run(self) {
        let mut cursor = 0;
        loop {
            match self.scan_once(cursor).await {
                Ok((next, _)) => cursor = next.unwrap_or(0),
                Err(_) => {
                    cursor = 0;
                    log::warn!("[agent-exec] durable command stop scan unavailable");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests;
