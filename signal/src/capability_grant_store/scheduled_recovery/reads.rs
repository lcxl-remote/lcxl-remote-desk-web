//! Interrupted reads have unavailable results, not proof of non-execution.
use super::*;
use crate::entity::agent_exec_task;
use desk_agent_protocol::capability_provider::CapabilityEffect;

pub(super) async fn close_untracked_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    matched: &mut BTreeSet<String>,
) -> Result<(), DbErr> {
    let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
    let turn = session.current_turn_id.clone().ok_or_else(invalid)?;
    let calls: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| {
            message.role == ChatRole::Assistant && message.turn_id.as_deref() == Some(turn.as_str())
        })
        .flat_map(|message| {
            message
                .tool_calls
                .iter()
                .map(move |call| (message.data_envelope.clone(), call.clone()))
        })
        .filter(|(_, call)| !matched.contains(&call.id))
        .collect();
    let open: BTreeSet<_> = session.unclosed_tool_call_ids().into_iter().collect();
    for (parent, call) in calls {
        let capability = registry
            .capability_for_tool(&call.name)
            .ok_or_else(invalid)?;
        if !matches!(
            capability.wire.effect,
            CapabilityEffect::ReadDevice
                | CapabilityEffect::ReadFile
                | CapabilityEffect::ReadExternal
                | CapabilityEffect::CaptureScreen
        ) || session
            .conversation
            .iter()
            .flat_map(|message| &message.tool_calls)
            .filter(|other| other.id == call.id)
            .count()
            != 1
            || !matched.insert(call.id.clone())
        {
            return Err(invalid());
        }
        if agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ConversationId.eq(&session.conversation_id))
            .filter(agent_exec_task::Column::ToolCallId.eq(&call.id))
            .one(txn)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
        if !open.contains(&call.id) {
            continue;
        }
        let id = stable_id(
            "scheduled-read-unavailable",
            &format!(
                "{}:{turn}:{}:{}",
                session.conversation_id, session.lease_token, call.id
            ),
        );
        if session
            .conversation
            .iter()
            .any(|message| message.message_id == id)
        {
            return Err(invalid());
        }
        let mut result = ChatMessage::tool_result(
            id,
            &call.id,
            "The executor stopped before a durable read result was available. The result is unavailable; this does not establish whether the read completed. Do not automatically retry this interrupted turn.",
        );
        result.turn_id = Some(turn.clone());
        result.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                parent.as_ref(),
                &call.id,
                &result.text,
                "scheduled_read_unavailable",
            )
            .map_err(|_| invalid())?;
        session.conversation.push(result);
    }
    Ok(())
}
