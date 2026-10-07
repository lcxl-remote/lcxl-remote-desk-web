//! Root-scoped child cancellation and adjustment with durable operation receipts.
use super::*;
use crate::entity::agent_delegation_reservation as receipt_row;
use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantSubAgentControl, SubAgentControlAction,
};
use sea_orm::{ActiveModelTrait, DatabaseTransaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentControlOutcome {
    pub task: AiAssistantSubAgentSummary,
    pub cancel_request_id: Option<String>,
}

impl SubAgentStore {
    pub async fn control_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        request: &AiAssistantSubAgentControl,
    ) -> Result<SubAgentControlOutcome, DbErr> {
        validate_control(request)?;
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;

        let parent = parent_on(&txn, root, actor, device).await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let result = apply_task_control_on(&txn, &parent, request, now_ms).await?;
        txn.commit().await?;
        Ok(result)
    }
}

fn validate_control(request: &AiAssistantSubAgentControl) -> Result<(), DbErr> {
    if !desk_diagnose_core::subagent::valid_id(&request.client_request_id)
        || !desk_diagnose_core::subagent::valid_id(&request.task_id)
        || request.expected_input_revision == 0
        || request.expected_control_revision == 0
        || request.expected_input_revision > i64::MAX as u64
        || request.expected_control_revision > i64::MAX as u64
        || match &request.action {
            SubAgentControlAction::Cancel => false,
            SubAgentControlAction::Adjust { message } => {
                message.trim().is_empty()
                    || message.len() > desk_diagnose_core::subagent::MAX_DELEGATED_TASK_BYTES
            }
        }
    {
        return Err(invalid());
    }
    Ok(())
}

/// The caller holds owner/root then child control. No model, device or network
/// call occurs in this transaction; cancellation delivery follows the commit.
pub(crate) async fn apply_task_control_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    request: &AiAssistantSubAgentControl,
    now_ms: i64,
) -> Result<SubAgentControlOutcome, DbErr> {
    apply_task_control_with_source_on(txn, parent, request, None, now_ms).await
}

/// Main-model controls retain the actual committed tool caller's restrictions;
/// they cannot pass through the authenticated human adjustment path.
pub(crate) async fn apply_task_control_from_turn_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    call: &desk_diagnose_core::chat::ToolCall,
    now_ms: i64,
) -> Result<SubAgentControlOutcome, DbErr> {
    use desk_diagnose_core::subagent::tools::{self, Operation};
    let operation = tools::parse(parent, call).map_err(|_| invalid())?;
    let (task_id, input, control, action) = match operation {
        Operation::Cancel {
            task_id,
            input_revision,
            control_revision,
        } => (
            task_id,
            input_revision,
            control_revision,
            SubAgentControlAction::Cancel,
        ),
        Operation::Message {
            task_id,
            input_revision,
            control_revision,
            message,
        } => (
            task_id,
            input_revision,
            control_revision,
            SubAgentControlAction::Adjust { message },
        ),
        _ => return Err(invalid()),
    };
    let caller = parent
        .conversation
        .iter()
        .find(|message| {
            message.role == desk_diagnose_core::chat::ChatRole::Assistant
                && message.turn_id == parent.current_turn_id
                && message.tool_calls.iter().any(|reference| {
                    reference.id == call.id
                        && reference.name == call.name
                        && desk_diagnose_core::subagent::tools::same_arguments(
                            &reference.arguments_json,
                            &call.arguments_json,
                        )
                })
        })
        .and_then(|message| message.data_envelope.clone())
        .ok_or_else(invalid)?;
    let id = format!("model-control-{:x}", Sha256::digest(call.id.as_bytes()));
    let request = AiAssistantSubAgentControl {
        client_request_id: id,
        task_id,
        expected_input_revision: input,
        expected_control_revision: control,
        action,
    };
    apply_task_control_with_source_on(txn, parent, &request, Some(caller), now_ms).await
}

async fn apply_task_control_with_source_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    request: &AiAssistantSubAgentControl,
    adjustment_source: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
    now_ms: i64,
) -> Result<SubAgentControlOutcome, DbErr> {
    validate_control(request)?;
    if !parent.agent_role.is_main() || parent.surface != AgentSessionSurface::AiAssistant {
        return Err(invalid());
    }
    let operation_key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &parent.conversation_id,
                &parent.actor_id,
                &parent.device_id,
                "subagent_control",
                &request.client_request_id,
            ))
            .map_err(|_| invalid())?
        )
    );
    let arguments = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(request, &adjustment_source)).map_err(|_| invalid())?)
    );
    if let Some(receipt) = receipt_row::Entity::find()
        .filter(receipt_row::Column::LogicalKeySha256.eq(&operation_key))
        .one(txn)
        .await?
    {
        if receipt.root_conversation_id != parent.conversation_id
            || receipt.operation_kind != "subagent_control"
            || receipt.task_id.as_deref() != Some(request.task_id.as_str())
            || receipt.arguments_sha256 != arguments
            || receipt.state != "control_committed"
        {
            return Err(invalid());
        }
        return serde_json::from_str(&receipt.reservation_json).map_err(|_| invalid());
    }
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&request.task_id))
        .filter(run_row::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(run_row::Column::ActorId.eq(&parent.actor_id))
        .filter(run_row::Column::DeviceId.eq(&parent.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut run = decode_run(&row)?;
    if run.binding.input_revision != request.expected_input_revision
        || run.binding.control_revision != request.expected_control_revision
    {
        return Err(invalid());
    }
    let group_row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(group_row::Column::ActorId.eq(&parent.actor_id))
        .filter(group_row::Column::DeviceId.eq(&parent.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&group_row)?;
    if group.source != run.binding.source {
        return Err(invalid());
    }
    let child_row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .filter(session_row::Column::ActorId.eq(&parent.actor_id))
        .filter(session_row::Column::DeviceId.eq(&parent.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let child = PersistedAgentSession::decode_json(&child_row.state_json).map_err(|_| invalid())?;
    run.validate_session(&child).map_err(|_| invalid())?;
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(invalid)?
        .to_rfc3339();
    let mut cancellation = None;
    let changed = match &request.action {
        SubAgentControlAction::Cancel => {
            let changed = run
                .request_cancel(run.fence(), &now)
                .map_err(|_| invalid())?;
            if changed {
                super::native_cancel::cancel_native_actions_on(txn, &child, &operation_key, now_ms)
                    .await?;
                cancellation = child.current_request_id.clone();
                run.settle_cancel(&now).map_err(|_| invalid())?;
            }
            changed
        }
        SubAgentControlAction::Adjust { message } => {
            if group.source_admission == SourceAdmission::Closed
                || now_ms >= run.binding.deadline_ms
                || group.source_epoch != run.binding.source_epoch
                || group.source != run.binding.source
            {
                return Err(invalid());
            }
            let criteria = run.binding.acceptance_criteria.clone();
            run.adjust(run.fence(), message.clone(), criteria, &now)
                .map_err(|_| invalid())?;
            let creation: desk_diagnose_core::subagent::creation::TaskCreationEnvelope =
                serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
            let label = match &adjustment_source {
                Some(label) => {
                    label.validate().map_err(|_| invalid())?;
                    if !label
                        .allowed_destinations
                        .contains(&creation.source.model_destination)
                    {
                        return Err(invalid());
                    }
                    label.clone()
                }
                None => desk_diagnose_core::model_message_labels::model_bound_user_message(
                    format!("owner-adjustment-{operation_key}"),
                    message.clone(),
                    creation.source.model_destination.clone(),
                )
                .map_err(|_| invalid())?
                .data_envelope
                .ok_or_else(invalid)?,
            };
            let adjusted = creation
                .adjusted(&run.binding, label, &run.child_conversation_id)
                .map_err(|_| invalid())?;
            let changed = run_row::Entity::update_many()
                .set(run_row::ActiveModel {
                    creation_envelope_json: Set(
                        serde_json::to_string(&adjusted).map_err(|_| invalid())?
                    ),
                    result_envelope_json: Set(None),
                    ..Default::default()
                })
                .filter(run_row::Column::Id.eq(row.id))
                .filter(run_row::Column::StateRevision.eq(row.state_revision))
                .exec(txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
            cancellation = child.current_request_id.clone();
            true
        }
    };
    if changed {
        replace_run_on(txn, &row, &run, now_ms).await?;
        synchronize_control_on(txn, &run, now_ms).await?;
        append_state_event_on(txn, &group, &run, now_ms).await?;
    }
    let result = SubAgentControlOutcome {
        task: run.summary(),
        cancel_request_id: cancellation,
    };
    receipt_row::ActiveModel {
        reservation_id: Set(format!("subagent-control-{operation_key}")),
        logical_key_sha256: Set(operation_key),
        root_conversation_id: Set(parent.conversation_id.clone()),
        group_id: Set(group.group_id.clone()),
        conversation_id: Set(parent.conversation_id.clone()),
        task_id: Set(Some(run.binding.task_id.clone())),
        operation_kind: Set("subagent_control".into()),
        arguments_sha256: Set(arguments),
        source_epoch: Set(i64::try_from(run.binding.source_epoch).map_err(|_| invalid())?),
        input_revision: Set(i64::try_from(run.binding.input_revision).map_err(|_| invalid())?),
        control_revision: Set(i64::try_from(run.binding.control_revision).map_err(|_| invalid())?),
        reservation_json: Set(serde_json::to_string(&result).map_err(|_| invalid())?),
        actual_json: Set(None),
        state: Set("control_committed".into()),
        version: Set(1),
        created_at: Set(now_ms),
        settled_at: Set(Some(now_ms)),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    Ok(result)
}
