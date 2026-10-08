//! UI reads, successful model observation and final interpretation are separate.
use super::*;
use crate::entity::{
    agent_delegation_reservation as reservation_row, agent_subagent_inbox as inbox,
};
use desk_diagnose_core::{chat::ChatRole, subagent::seam::AcceptedResultObservation};
use sea_orm::DatabaseTransaction;
use sha2::{Digest, Sha256};

pub(super) async fn accepted_inputs_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    response: &desk_diagnose_core::chat::ChatMessage,
) -> Result<Vec<desk_diagnose_core::model_egress::ModelInputLineage>, DbErr> {
    let output = desk_diagnose_core::model_egress::model_output_message_envelope(response)
        .map_err(|_| invalid())?;
    let reservations = reservation_row::Entity::find()
        .filter(reservation_row::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(reservation_row::Column::ConversationId.eq(&parent.conversation_id))
        .filter(
            reservation_row::Column::GroupId
                .eq(parent.delegation_group_id.as_deref().ok_or_else(invalid)?),
        )
        .filter(
            reservation_row::Column::InputRevision
                .eq(i64::try_from(parent.input_revision).map_err(|_| invalid())?),
        )
        .filter(
            reservation_row::Column::ControlRevision
                .eq(i64::try_from(parent.control_revision).map_err(|_| invalid())?),
        )
        .filter(reservation_row::Column::OperationKind.eq("model"))
        .filter(reservation_row::Column::ProviderReceiptId.is_not_null())
        .order_by_asc(reservation_row::Column::Id)
        .limit(161)
        .all(txn)
        .await?;
    if reservations.len() > 160 {
        return Err(invalid());
    }
    let mut matched = None;
    for reservation in reservations {
        if reservation.provider_receipt_kind.as_deref() != Some("oss_model_egress") {
            return Err(invalid());
        }
        let Some(call) = crate::entity::model_egress_receipt::Entity::find_by_id(
            reservation.provider_receipt_id.ok_or_else(invalid)?,
        )
        .one(txn)
        .await?
        else {
            continue;
        };
        if call.model_output_envelope_id.as_deref() != Some(output.envelope_id.as_str())
            || call.model_output_digest_sha256.as_deref() != Some(output.digest_sha256.as_str())
        {
            continue;
        }
        if matched.is_some() {
            return Err(invalid());
        }
        let evidence = crate::model_egress_store::SignalModelEgressStore::read_output_evidence_on(
            txn,
            &call.receipt_id,
            &call.export_authorization_id,
            u64::try_from(call.model_call_ordinal).map_err(|_| invalid())?,
            response,
        )
        .await?;
        matched = Some(evidence.inputs);
    }
    matched.ok_or_else(invalid)
}

async fn validate_observation_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    accepted: &AcceptedResultObservation,
    interpreted: bool,
) -> Result<inbox::Model, DbErr> {
    accepted.validate().map_err(|_| invalid())?;
    let observation = &accepted.result;
    if !parent.agent_role.is_main()
        || observation.parent_input_revision != parent.input_revision
        || observation.parent_control_revision != parent.control_revision
        || !parent.observed_subagent_results.contains(observation)
    {
        return Err(invalid());
    }
    let tool = parent
        .conversation
        .iter()
        .find(|message| {
            message.message_id == observation.result_message_id
                && message.role == ChatRole::Tool
                && message.tool_call_id.as_deref() == Some(observation.tool_call_id.as_str())
        })
        .ok_or_else(invalid)?;
    let label = tool.data_envelope.as_ref().ok_or_else(invalid)?;
    label.validate().map_err(|_| invalid())?;
    if label.provenance.source_tool_name != desk_diagnose_core::subagent::tools::RESULT
        || label.digest_sha256 != format!("{:x}", Sha256::digest(tool.text.as_bytes()))
    {
        return Err(invalid());
    }
    let result: desk_diagnose_core::subagent::result::SubAgentModelResult =
        serde_json::from_str(&tool.text).map_err(|_| invalid())?;
    if result.task_id != observation.task_id
        || result.state_revision != observation.state_revision
        || result.answer.is_none()
    {
        return Err(invalid());
    }
    let response = parent
        .conversation
        .iter()
        .find(|message| {
            message.message_id == accepted.response_message_id
                && message.role == ChatRole::Assistant
                && message.turn_id == parent.current_turn_id
        })
        .ok_or_else(invalid)?;
    if interpreted {
        if response.tool_calls.is_empty() {
            if response.text.trim().is_empty() {
                return Err(invalid());
            }
        } else {
            let [reference] = response.tool_calls.as_slice() else {
                return Err(invalid());
            };
            let call = desk_diagnose_core::chat::ToolCall {
                id: reference.id.clone(),
                name: reference.name.clone(),
                arguments_json: reference.arguments_json.clone(),
            };
            let decision =
                desk_diagnose_core::goal_tools::recorded_decision(&call).map_err(|_| invalid())?;
            let group = super::main_tools::current_group_on(txn, parent).await?;
            if parent.trigger_origin != desk_diagnose_core::session::TriggerOrigin::GoalContinuation
                || group.source.goal_id().is_none()
                || group.source.goal_id()
                    != parent
                        .focus_epoch
                        .goal_segment
                        .as_ref()
                        .map(|segment| segment.goal_id.as_str())
                || !matches!(
                    decision,
                    desk_diagnose_core::goal_tools::GoalDecision::Complete
                        | desk_diagnose_core::goal_tools::GoalDecision::Blocked
                )
            {
                return Err(invalid());
            }
        }
    }
    let inputs = accepted_inputs_on(txn, parent, response).await?;
    if !inputs.iter().any(|input| {
        input.envelope_id == label.envelope_id && input.digest_sha256 == label.digest_sha256
    }) {
        return Err(invalid());
    }
    let event = inbox::Entity::find()
        .filter(inbox::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(inbox::Column::ActorId.eq(&parent.actor_id))
        .filter(inbox::Column::DeviceId.eq(&parent.device_id))
        .filter(inbox::Column::TaskId.eq(&observation.task_id))
        .filter(
            inbox::Column::StateRevision
                .eq(i64::try_from(observation.state_revision).map_err(|_| invalid())?),
        )
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let original: desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentEvent =
        serde_json::from_str(&event.event_json).map_err(|_| invalid())?;
    if original.task.task_id != result.task_id
        || original.task.state_revision != result.state_revision
        || original.task.state != result.state
        || original.task.wait_reason != result.wait_reason
    {
        return Err(invalid());
    }
    Ok(event)
}

/// The caller holds the root lock and commits the session CAS in this transaction.
/// Nothing is consumed by a failed read, failed egress, failed model or failed save.
pub(crate) async fn acknowledge_results_on(
    txn: &DatabaseTransaction,
    current: &PersistedAgentSession,
    next: &PersistedAgentSession,
    now_ms: i64,
) -> Result<(), DbErr> {
    if !next.agent_role.is_main() {
        return Err(invalid());
    }
    super::notification::acknowledge_notification_on(txn, current, next, now_ms).await?;
    if next
        .accepted_subagent_observations
        .iter()
        .any(|value| !current.accepted_subagent_observations.contains(value))
        || next
            .interpreted_subagent_results
            .iter()
            .any(|value| !current.interpreted_subagent_results.contains(value))
    {
        super::main_tools::current_group_on(txn, next).await?;
    }
    for accepted in &next.accepted_subagent_observations {
        if current.accepted_subagent_observations.contains(accepted) {
            continue;
        }
        let event = validate_observation_on(txn, next, accepted, false).await?;
        if event.model_observed_at_ms.is_none() {
            inbox::Entity::update_many()
                .set(inbox::ActiveModel {
                    model_observed_at_ms: Set(Some(now_ms)),
                    observed_tool_call_id: Set(Some(accepted.result.tool_call_id.clone())),
                    observed_message_id: Set(Some(accepted.result.result_message_id.clone())),
                    ..Default::default()
                })
                .filter(inbox::Column::Id.eq(event.id))
                .filter(inbox::Column::ModelObservedAtMs.is_null())
                .exec(txn)
                .await?;
        }
    }
    for accepted in &next.interpreted_subagent_results {
        if current.interpreted_subagent_results.contains(accepted) {
            continue;
        }
        if !next
            .accepted_subagent_observations
            .iter()
            .any(|observation| observation.result == accepted.result)
        {
            return Err(invalid());
        }
        let event = validate_observation_on(txn, next, accepted, true).await?;
        if event.model_observed_at_ms.is_none() {
            return Err(invalid());
        }
        if event.interpreted_at_ms.is_none() {
            inbox::Entity::update_many()
                .set(inbox::ActiveModel {
                    interpreted_at_ms: Set(Some(now_ms)),
                    interpreted_turn_id: Set(next.current_turn_id.clone()),
                    ..Default::default()
                })
                .filter(inbox::Column::Id.eq(event.id))
                .filter(inbox::Column::InterpretedAtMs.is_null())
                .exec(txn)
                .await?;
        }
    }
    Ok(())
}

pub(crate) async fn save_main_delegation_session(
    db: &DatabaseConnection,
    held: &mut PersistedAgentSession,
) -> Result<(), DbErr> {
    let txn = crate::db::begin_write(db, session_row::Entity).await?;
    let current = parent_on(&txn, &held.conversation_id, &held.actor_id, &held.device_id).await?;
    if current.version != held.version
        || current.lease_token != held.lease_token
        || current.input_revision != held.input_revision
        || current.control_revision != held.control_revision
        || current.current_turn_id != held.current_turn_id
        || current.delegation_group_id != held.delegation_group_id
    {
        return Err(invalid());
    }
    let now = chrono::Utc::now();
    acknowledge_results_on(&txn, &current, held, now.timestamp_millis()).await?;
    let version = write_child_session_on(&txn, held, now).await?;
    txn.commit().await?;
    held.version = version;
    Ok(())
}
