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
use sea_orm::{Condition, QueryTrait};

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
