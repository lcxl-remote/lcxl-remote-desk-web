//! Desired native stops commit with owner/task control; immutable results remain untouched.
use super::*;
use crate::entity::{
    agent_action_item as work, agent_capability_dispatch_outbox as dispatch,
    agent_exec_task as command,
};
use sea_orm::ActiveModelTrait;

pub(crate) async fn cancel_native_actions_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    txn: &C,
    session: &PersistedAgentSession,
    operation: &str,
    now_ms: i64,
) -> Result<(), DbErr> {
    let now = chrono::DateTime::from_timestamp_millis(now_ms).ok_or_else(invalid)?;
    let rows = work::Entity::find()
        .filter(work::Column::ConversationId.eq(&session.conversation_id))
        .filter(work::Column::ActorId.eq(&session.actor_id))
        .filter(work::Column::TargetDeviceId.eq(&session.device_id))
        .filter(
            sea_orm::Condition::any()
                .add(work::Column::ResultJson.is_null())
                .add(
                    work::Column::Status
                        .eq(crate::capability_grant_store::CAPABILITY_WORK_OUTCOME_UNKNOWN),
                ),
        )
        .order_by_asc(work::Column::Id)
        .limit(201)
        .all(txn)
        .await?;
    if rows.len() > 200 {
        return Err(invalid());
    }
    for row in rows {
        if row.cancel_requested_at.is_some() {
            continue;
        }
        let original = dispatch::Entity::find()
            .filter(dispatch::Column::WorkId.eq(row.id))
            .one(txn)
            .await?;
        if let Some(original) = original {
            let payload: crate::capability_grant_store::CapabilityDispatchPayload =
                serde_json::from_str(&original.payload_json).map_err(|_| invalid())?;
            if payload.work_id != row.id || payload.call_id != row.action_request_id {
                return Err(invalid());
            }
            if original.computer_binding_json.is_some() {
                let key = format!("owner-stop:{operation}:{}", row.id);
                crate::capability_grant_store::SignalCapabilityGrantStore::request_computer_execution_cancel_on(
                    txn, &row.action_request_id, &session.conversation_id, &session.actor_id, &session.device_id,
                    &key, "Owner stopped this task").await?;
                continue;
            }
        }
        // Undispatched work and commands are fenced before their next send.
        // Original command generations also receive a separate durable stop below.
        let generation = row.execution_id.clone();
        let mut active: work::ActiveModel = row.into();
        active.cancel_requested_at = Set(Some(now));
        active.cancel_requested_by = Set(Some(session.actor_id.clone()));
        active.cancel_generation = Set(generation);
        active.update(txn).await?;
    }
    let commands = command::Entity::find()
        .filter(command::Column::ConversationId.eq(&session.conversation_id))
        .filter(command::Column::Status.is_in(["dispatching", "running", "unknown"]))
        .filter(command::Column::CancelRequestedAt.is_null())
        .order_by_asc(command::Column::Id)
        .limit(201)
        .all(txn)
        .await?;
    if commands.len() > 200 {
        return Err(invalid());
    }
    for row in commands {
        if !desk_diagnose_core::subagent::valid_id(&row.execution_generation)
            || !desk_diagnose_core::subagent::valid_id(&row.exec_request_id)
        {
            return Err(invalid());
        }
        let mut active: command::ActiveModel = row.into();
        active.cancel_requested_at = Set(Some(now));
        active.cancel_requested_by = Set(Some(session.actor_id.clone()));
        active.update(txn).await?;
    }
    Ok(())
}

impl SubAgentStore {
    /// Cleanup preserves execution facts and remains available after task termination.
    pub async fn cancel_command_for_owner(
        &self,
        session_id: &str,
        actor: &str,
        device: &str,
        request_id: &str,
        generation: &str,
    ) -> Result<bool, DbErr> {
        if !desk_diagnose_core::subagent::valid_id(request_id)
            || !desk_diagnose_core::subagent::valid_id(generation)
        {
            return Err(invalid());
        }
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(session_id))
            .filter(session_row::Column::ActorId.eq(actor))
            .filter(session_row::Column::DeviceId.eq(device))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.actor_id != actor
            || session.device_id != device
            || session.conversation_id != session_id
        {
            return Err(invalid());
        }
        let row = command::Entity::find()
            .filter(command::Column::ConversationId.eq(session_id))
            .filter(command::Column::ExecRequestId.eq(request_id))
            .filter(command::Column::ExecutionGeneration.eq(generation))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        if !matches!(row.status.as_str(), "dispatching" | "running" | "unknown") {
            txn.commit().await?;
            return Ok(false);
        }
        if row.cancel_requested_at.is_none() {
            let mut active: command::ActiveModel = row.into();
            active.cancel_requested_at = Set(Some(chrono::Utc::now()));
            active.cancel_requested_by = Set(Some(actor.into()));
            active.update(&txn).await?;
        } else if row.cancel_requested_by.as_deref() != Some(actor) {
            return Err(invalid());
        }
        txn.commit().await?;
        Ok(true)
    }
}
