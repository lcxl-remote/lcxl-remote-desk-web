//! Inspect the original input's durable work while holding the SQLite writer.

use super::*;
use crate::{
    agent_background_task_store::{self, BACKGROUND_ACTION_KIND},
    agent_exec_store::{DELIVERY_PENDING, DELIVERY_PUBLISHED},
    capability_grant_store::CapabilityDispatchPayload,
    entity::{
        agent_action_item, agent_capability_dispatch_outbox as dispatch, agent_exec_task,
        agent_permission_resume,
    },
};
use desk_assistant_data_store::main_request;
use sea_orm::{Condition, QueryTrait, sea_query::Expr};

async fn load_workbench_on(
    txn: &DatabaseTransaction,
    actor: &str,
    workbench_id: &str,
) -> Result<(agent_session::Model, Box<PersistedAgentSession>), AgentError> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(workbench_id))
        .filter(agent_session::Column::ActorId.eq(actor))
        .one(txn)
        .await
        .map_err(|_| internal("workbench coordination storage is unavailable"))?
        .ok_or_else(|| internal("workbench coordinator is unavailable"))?;
    let root = main_request::decode_workbench_coordinator(&row.state_json, actor, workbench_id)
        .map_err(|error| error.into_agent_error())?;
    if root.version != row.version || i64::try_from(root.lease_token).ok() != Some(row.lease_token)
    {
        return Err(internal("workbench coordinator fence changed"));
    }
    Ok((row, root))
}

pub(crate) async fn active_work_on(
    db: &impl sea_orm::ConnectionTrait,
    root: &PersistedAgentSession,
) -> Result<Option<agent_session::Model>, AgentError> {
    let Some(conversation) = main_request::coordinating_work_conversation(root) else {
        return Ok(None);
    };
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&conversation))
        .filter(agent_session::Column::ActorId.eq(&root.actor_id))
        .filter(agent_session::Column::DeviceId.eq(""))
        .one(db)
        .await
        .map_err(|_| internal("workbench coordination storage is unavailable"))?
    else {
        // An expired or removed work no longer owns a native turn.
        return Ok(None);
    };
    let session = PersistedAgentSession::decode_boxed_json(&row.state_json)
        .map_err(|_| internal("invalid coordinating work"))?;
    if session.actor_id != root.actor_id
        || session.conversation_id != conversation
        || session.agent_role.workbench_id() != Some(root.conversation_id.as_str())
        || session.agent_role.workbench_request_id() != root.coordinating_work.as_deref()
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
    {
        return Err(internal("coordinating work identity changed"));
    }
    Ok(session.turn_state.is_active().then_some(row))
}

pub(crate) async fn workbench_ready_on(
    txn: &DatabaseTransaction,
    actor: &str,
    workbench_id: &str,
    now_ms: i64,
) -> Result<bool, AgentError> {
    let (_, root) = load_workbench_on(txn, actor, workbench_id).await?;
    ready_on(txn, &root, now_ms).await
}

pub(crate) async fn ready_on(
    txn: &DatabaseTransaction,
    root: &PersistedAgentSession,
    now_ms: i64,
) -> Result<bool, AgentError> {
    Ok(!root.main_stopped
        && !root.has_live_main_coordination(now_ms)
        && active_work_on(txn, root).await?.is_none())
}

/// The root and native turn are claimed in the same transaction. The pointer
/// borrows the native turn's lease, including heartbeat and orphan recovery.
pub(crate) async fn claim_workbench_on(
    txn: &DatabaseTransaction,
    actor: &str,
    workbench_id: &str,
    request_id: &str,
    now_ms: i64,
) -> Result<bool, AgentError> {
    let (row, mut root) = load_workbench_on(txn, actor, workbench_id).await?;
    if !ready_on(txn, &root, now_ms).await? {
        return Ok(false);
    }
    if root.coordinating_work.as_deref() == Some(request_id) {
        return Ok(true);
    }
    root.coordinating_work = Some(request_id.into());
    root.version = row
        .version
        .checked_add(1)
        .ok_or_else(|| internal("workbench coordinator revision exhausted"))?;
    let json = root
        .encode_json_for_storage()
        .map_err(|_| internal("invalid workbench coordination state"))?;
    let changed = agent_session::Entity::update_many()
        .col_expr(agent_session::Column::Version, Expr::value(root.version))
        .col_expr(agent_session::Column::StateJson, Expr::value(json))
        .filter(agent_session::Column::Id.eq(row.id))
        .filter(agent_session::Column::Version.eq(row.version))
        .filter(agent_session::Column::StateJson.eq(row.state_json))
        .exec(txn)
        .await
        .map_err(|_| internal("workbench coordination storage is unavailable"))?;
    Ok(changed.rows_affected == 1)
}

pub(crate) async fn slot_held(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<bool, AgentError> {
    if main_request::holds_input_slot(session) {
        return Ok(true);
    }
    if session.input_revision == 0 {
        return Ok(false);
    }
    let resume = agent_permission_resume::Entity::find()
        .filter(agent_permission_resume::Column::RunId.eq(&session.conversation_id))
        .filter(agent_permission_resume::Column::ActorId.eq(&session.actor_id))
        .filter(agent_permission_resume::Column::InputRevision.eq(session.input_revision as i64))
        .filter(agent_permission_resume::Column::State.eq("pending"))
        .one(txn)
        .await
        .map_err(|_| internal("assistant approval continuation unavailable"))?;
    if resume.is_some() {
        return Ok(true);
    }
    let command_generations = agent_exec_task::Entity::find()
        .select_only()
        .column(agent_exec_task::Column::ExecutionGeneration)
        .filter(agent_exec_task::Column::ConversationId.eq(&session.conversation_id))
        .filter(
            Condition::any()
                .add(
                    agent_exec_task::Column::Status
                        .is_in(crate::usage_retention::UNRESOLVED_EXEC_STATES),
                )
                .add(
                    agent_exec_task::Column::DeliveryState
                        .is_in([DELIVERY_PENDING, DELIVERY_PUBLISHED]),
                ),
        )
        .into_query();
    let commands: Vec<String> = dispatch::Entity::find()
        .select_only()
        .column(dispatch::Column::PayloadJson)
        .filter(dispatch::Column::DispatchId.in_subquery(command_generations))
        .into_tuple()
        .all(txn)
        .await
        .map_err(|_| internal("assistant command continuation unavailable"))?;
    for raw in commands {
        let command: CapabilityDispatchPayload = serde_json::from_str(&raw)
            .map_err(|_| internal("invalid assistant command continuation"))?;
        if command
            .command_origin
            .as_ref()
            .is_some_and(|origin| main_request::same_input(&origin.turn_fence, session))
        {
            return Ok(true);
        }
    }
    let actions = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_action_item::Column::ActorId.eq(&session.actor_id))
        .filter(
            Condition::any()
                .add(
                    agent_action_item::Column::Status
                        .is_in(crate::usage_retention::UNRESOLVED_ACTION_STATES),
                )
                .add(
                    agent_action_item::Column::CompletionDeliveryState
                        .is_in([DELIVERY_PENDING, DELIVERY_PUBLISHED]),
                ),
        )
        .all(txn)
        .await
        .map_err(|_| internal("assistant action continuation unavailable"))?;
    for action in actions {
        if action.kind == BACKGROUND_ACTION_KIND {
            let record = agent_background_task_store::decode_record(&action)
                .map_err(|_| internal("invalid assistant background action"))?;
            if record.task.input_revision == session.input_revision {
                return Ok(true);
            }
        } else if session.current_turn_id.as_deref() == Some(action.turn_id.as_str())
            || session.chain_id == action.turn_id
        {
            return Ok(true);
        }
    }
    Ok(false)
}
