//! Retain only an authenticated native send result after the original writes.
//! Original transport, row subjects and exact JSON are selected again before
//! the final history quota phase; no execution or delivery is replayed here.

use super::*;
use crate::assistant_data_store::native_result::{self, NativeSendResult};

pub(super) async fn capture_on(
    txn: &DatabaseTransaction,
    generation: &str,
    original_outbox: &agent_capability_dispatch_outbox::Model,
    original_work: &agent_action_item::Model,
    original_binding: &ComputerBinding,
    expected: &Record,
) -> Result<(), DbErr> {
    if original_binding.origin.turn_fence.actor_id
        != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
    {
        return Ok(());
    }
    let computer_original = original_on(txn, generation).await?;
    let (outbox, work, payload) = computer_original.as_ref();
    if work.id != original_work.id
        || work.actor_id != original_work.actor_id
        || work.conversation_id != original_work.conversation_id
        || work.turn_id != original_work.turn_id
        || work.tool_call_id != original_work.tool_call_id
        || work.target_device_id != original_work.target_device_id
        || work.payload_json != original_work.payload_json
        || work.dispatch_intent_at != original_work.dispatch_intent_at
        || outbox.id != original_outbox.id
        || outbox.payload_json != original_outbox.payload_json
        || outbox.computer_binding_json != original_outbox.computer_binding_json
        || work.result_json.as_deref()
            != Some(
                serde_json::to_string(expected)
                    .map_err(|_| invalid())?
                    .as_str(),
            )
    {
        return Err(invalid());
    }
    let frozen = binding(outbox, work, payload)?;
    if frozen.as_ref() != original_binding {
        return Err(invalid());
    }
    if !matches!(
        frozen.origin.tool_name.as_str(),
        "send_gmail_message" | "send_slack_message"
    ) || frozen.origin.turn_fence.delegation.is_some()
    {
        let fence = &frozen.origin.turn_fence;
        let root = fence
            .delegation
            .as_ref()
            .map_or(fence.conversation_id.as_str(), |value| {
                value.root_conversation_id.as_str()
            });
        if let Some(scope) = crate::assistant_data_store::native_scope_on(
            txn,
            &fence.actor_id,
            &fence.device_id,
            root,
        )
        .await?
        {
            scope.capture_on(txn).await?;
        }
        return Ok(());
    }
    let record = decode(outbox, work, payload, &frozen)?;
    let (observation, output, receipt) = if let Some(terminal) = record.terminal {
        (
            terminal.observation,
            ToolRunOutput {
                format: desk_diagnose_core::seam::ToolOutputFormat::Json,
                content: terminal.projection.content,
                image_data_url: None,
                document_preview: None,
            },
            terminal.receipt,
        )
    } else if let Some(unknown) = record.unknown {
        let receipt = desk_diagnose_core::communication_handoff::project_web_send_receipt(
            &payload.tool_name,
            &payload.canonical_input_json,
            &unknown.native,
        )
        .map_err(|_| invalid())?
        .ok_or_else(invalid)?;
        let content =
            desk_diagnose_core::communication_handoff::project_send_receipt_output(&receipt)
                .map_err(|_| invalid())?;
        let output = ToolRunOutput {
            format: desk_diagnose_core::seam::ToolOutputFormat::Json,
            content,
            image_data_url: None,
            document_preview: None,
        };
        // This is a deterministic view of the original durable unknown
        // observation. Its timestamp never comes from a delivery or retry.
        let receipt = frozen
            .origin
            .receipt(
                identity(work, payload),
                1,
                unknown.observed_at_unix_ms,
                &output,
            )
            .map_err(|_| invalid())?;
        (unknown, output, receipt)
    } else {
        return Err(invalid());
    };
    let dispatched = u64::try_from(
        work.dispatch_intent_at
            .ok_or_else(invalid)?
            .timestamp_millis(),
    )
    .map_err(|_| invalid())?;
    native_result::capture_on(
        txn,
        NativeSendResult {
            origin: &frozen.origin,
            receipt: &receipt,
            output: &output,
            canonical_input: &payload.canonical_input_json,
            completion: &observation.native,
            provider_device_id: &frozen.plan.device_id,
            dispatched_at_unix_ms: dispatched,
        },
    )
    .await
    .map_err(|_| DbErr::Custom("native send history could not be committed".into()))
}
