//! UI reads, successful model observation and final interpretation are separate.
use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::{
    agent_delegation_reservation as reservation_row, agent_subagent_inbox as inbox,
};
use desk_diagnose_core::{chat::ChatRole, subagent::seam::AcceptedResultObservation};

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
            reservation_row::Column::GroupId.eq(parent
                .delegation_group_id
                .as_deref()
                .ok_or_else(|| invalid())?),
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
            reservation.provider_receipt_id.ok_or_else(|| invalid())?,
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
    matched.ok_or_else(|| invalid())
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
        .ok_or_else(|| invalid())?;
    let label = tool.data_envelope.as_ref().ok_or_else(|| invalid())?;
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
        .ok_or_else(|| invalid())?;
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
        .ok_or_else(|| invalid())?;
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
    desk_diagnose_core::future::boxed(|| async {
        super::notification::acknowledge_notification_on(txn, current, next, now_ms).await
    })
    .await?;
    if next
        .accepted_subagent_observations
        .iter()
        .any(|value| !current.accepted_subagent_observations.contains(value))
        || next
            .interpreted_subagent_results
            .iter()
            .any(|value| !current.interpreted_subagent_results.contains(value))
    {
        desk_diagnose_core::future::boxed(|| async {
            super::main_tools::current_group_on(txn, next).await
        })
        .await?;
    }
    for accepted in &next.accepted_subagent_observations {
        if current.accepted_subagent_observations.contains(accepted) {
            continue;
        }
        let event = desk_diagnose_core::future::boxed(|| async {
            validate_observation_on(txn, next, accepted, false).await
        })
        .await?;
        if event.model_observed_at_ms.is_none() {
            desk_diagnose_core::future::boxed(|| async {
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
                    .await
            })
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
        let event = desk_diagnose_core::future::boxed(|| async {
            validate_observation_on(txn, next, accepted, true).await
        })
        .await?;
        if event.model_observed_at_ms.is_none() {
            return Err(invalid());
        }
        if event.interpreted_at_ms.is_none() {
            desk_diagnose_core::future::boxed(|| async {
                inbox::Entity::update_many()
                    .set(inbox::ActiveModel {
                        interpreted_at_ms: Set(Some(now_ms)),
                        interpreted_turn_id: Set(next.current_turn_id.clone()),
                        ..Default::default()
                    })
                    .filter(inbox::Column::Id.eq(event.id))
                    .filter(inbox::Column::InterpretedAtMs.is_null())
                    .exec(txn)
                    .await
            })
            .await?;
        }
    }
    Ok(())
}

pub(crate) async fn save_main_delegation_session(
    db: &DatabaseConnection,
    held: &mut PersistedAgentSession,
) -> Result<(), DbErr> {
    let txn = desk_diagnose_core::future::boxed(|| async {
        crate::db::begin_write(db, session_row::Entity).await
    })
    .await?;
    let current = desk_diagnose_core::future::boxed(|| async {
        parent_on(&txn, &held.conversation_id, &held.actor_id, &held.device_id).await
    })
    .await?;
    txn.mark_phase("parent_read");
    if !desk_diagnose_core::future::boxed(|| async {
        crate::goal_work::rebase_background_events_on(&txn, &current, held).await
    })
    .await?
        || (current.assistant_data.is_some() && held.assistant_data.is_none())
        || current.lease_token != held.lease_token
        || current.input_revision != held.input_revision
        || current.control_revision != held.control_revision
        || current.current_turn_id != held.current_turn_id
        || current.delegation_group_id != held.delegation_group_id
    {
        return Err(invalid());
    }
    let now = chrono::Utc::now();
    let version = if held.surface == desk_diagnose_core::session::AgentSessionSurface::AiAssistant
        && held.assistant_data.is_some()
    {
        desk_diagnose_core::future::send_phase(|| save_main_history_on(&txn, &current, held, now))
            .await?
    } else {
        desk_diagnose_core::future::boxed(|| async {
            acknowledge_results_on(&txn, &current, held, now.timestamp_millis()).await
        })
        .await?;
        desk_diagnose_core::future::boxed(|| async {
            write_child_session_on(&txn, held, now).await
        })
        .await?
    };
    desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
    held.version = version;
    Ok(())
}

async fn save_main_history_on(
    txn: &DatabaseTransaction,
    current: &PersistedAgentSession,
    held: &PersistedAgentSession,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<i64, DbErr> {
    use super::super::{agent_session_store::history_save, assistant_data_store};
    use desk_agent_protocol::ai_assistant::history::HistoryAdmission;

    let mut prepared = desk_diagnose_core::future::send_phase(|| {
        assistant_data_store::prepare_visible_history_on(txn, held)
    })
    .await
    .map_err(history_error)?;
    txn.mark_phase("history_prepared");
    // Keep child observation and the original session update in this same
    // transaction. No native/source read follows the final history quota phase.
    desk_diagnose_core::future::send_phase(|| {
        acknowledge_results_on(txn, current, held, now.timestamp_millis())
    })
    .await?;
    txn.mark_phase("results_acknowledged");
    desk_diagnose_core::future::send_phase(|| prepared.observe_native_status_on(txn))
        .await
        .map_err(history_error)?;
    txn.mark_phase("native_status_observed");
    let captures = prepared.captures();
    let requests = prepared.requests(&captures).map_err(history_error)?;
    let deadlines = prepared.deadlines().map_err(history_error)?;
    let pending = desk_diagnose_core::future::send_phase(|| {
        assistant_data_store::history::staged::prepare_requests_on(txn, &requests, &prepared.policy)
    })
    .await
    .map_err(history_error)?;

    txn.mark_phase("history_requests_prepared");
    let held_until = desk_diagnose_core::future::send_phase(|| prepared.finish_preparation_on(txn))
        .await
        .map_err(history_error)?;
    txn.mark_phase("preparation_finished");
    let version = held.version.checked_add(1).ok_or_else(|| invalid())?;
    let state_json = {
        let mut stored = Box::new(held.clone());
        desk_diagnose_core::image_input::retain_latest_session_image(&mut stored.conversation)
            .map_err(|_| invalid())?;
        desk_diagnose_core::image_input::strip_session_images(&mut stored.conversation);
        stored.version = version;
        stored.encode_json_for_storage().map_err(|_| invalid())?
    };
    txn.mark_phase("state_encoded");
    let deadline = held
        .turn_state
        .is_active()
        .then_some(now + chrono::Duration::seconds(90));
    let changed = history_save::persist_on(
        txn,
        held,
        &state_json,
        version,
        now,
        deadline,
        Some(prepared.original.as_ref()),
    )
    .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }

    txn.mark_phase("session_persisted");
    let outcomes = desk_diagnose_core::future::send_phase(|| pending.finalize_on())
        .await
        .map_err(history_error)?;
    txn.mark_phase("history_finalized");
    let completed = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| invalid())?;
    if outcomes.len() != deadlines.len()
        || held_until.is_some_and(|until| until <= completed)
        || outcomes.iter().zip(deadlines).any(|(outcome, deadline)| {
            matches!(
                outcome.admission,
                HistoryAdmission::Indexed | HistoryAdmission::Redacted
            ) && deadline.is_some_and(|until| until <= completed)
        })
    {
        return Err(invalid());
    }
    Ok(version)
}

fn history_error(error: super::super::assistant_data_store::AssistantStoreError) -> DbErr {
    match error {
        super::super::assistant_data_store::AssistantStoreError::Backend(error) => error,
        _ => invalid(),
    }
}
