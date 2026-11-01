//! Native main-assistant forget, committed with its exact tool results.
//! Targets are decoded at the model boundary; native state still grants control.

use super::super::{signed, unsigned};
use super::*;
use crate::{
    config::connection::{DatabaseConnection, DatabaseTransaction},
    entity::agent_session,
};
use desk_agent_protocol::ai_assistant::memory::DataControlOutcome;
use desk_assistant_data_store::{
    entity::agent_assistant_control_receipt as receipt, memory_control::project_receipt,
};
use desk_diagnose_core::{
    ai_assistant::{
        assistant_data_admission::OriginalAssistantDataCall, data_control, data_tools,
        seam::AssistantDataCall,
    },
    chat::ToolCall,
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

/// The returned session image becomes visible only after the transaction commits.
/// Replays recover only the original active claim; later turns stay untouched.
pub(crate) async fn forget(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    call: &desk_diagnose_core::ai_assistant::seam::MemoryForgetCall<'_>,
) -> Result<
    (
        AssistantDataControlReceipt,
        Option<Box<PersistedAgentSession>>,
    ),
    AssistantStoreError,
> {
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let result = desk_diagnose_core::future::boxed(|| forget_on(&txn, session, call)).await;
    match result {
        Ok(value) => {
            txn.commit().await?;
            Ok(value)
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

async fn forget_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    call: &desk_diagnose_core::ai_assistant::seam::MemoryForgetCall<'_>,
) -> Result<
    (
        AssistantDataControlReceipt,
        Option<Box<PersistedAgentSession>>,
    ),
    AssistantStoreError,
> {
    let now = super::super::conversation_search::now_ms()?;
    if now < call.now_unix_ms {
        return Err(AssistantStoreError::Inaccessible);
    }
    let producer = session
        .conversation
        .iter()
        .find(|message| message.message_id == call.producer_message_id)
        .ok_or(AssistantStoreError::Inaccessible)?;
    let hash = data_control::tool_group_sha256(std::slice::from_ref(producer))
        .map_err(AssistantStoreError::Invalid)?;
    if let Some(previous) = receipt::Entity::find()
        .filter(receipt::Column::ActorId.eq(&session.actor_id))
        .filter(receipt::Column::OperationId.eq(call.operation_id))
        .one(db)
        .await?
    {
        let row = root_on(db, session).await?;
        let stored = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AssistantStoreError::Inaccessible)?;
        let group = data_control::read_memory_forget_group(&stored, call.producer_message_id)
            .map_err(AssistantStoreError::Invalid)?;
        let request = ForgetGroupRequest {
            operation_id: call.operation_id.into(),
            producer_message_id: call.producer_message_id.into(),
            call_message_sha256: hash,
            owner_input_seq: unsigned(previous.accepted_input_seq)?,
            targets: call.targets.to_vec(),
        };
        let mut bound_receipt = group.receipt.clone();
        bound_receipt.cleanup_pending = previous.cleanup_pending;
        if project_receipt(&previous, false)? != bound_receipt
            || previous.tool_group_id.as_deref() != Some(group.assistant_message_id.as_str())
            || group.assistant_message_id != call.producer_message_id
            || previous.tool_group_sha256.as_deref() != Some(group.group_sha256.as_str())
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let ControlPreparation::Unchanged(receipt) =
            prepare_forget_group_on(db, &session.actor_id, &request, now).await?
        else {
            return Err(AssistantStoreError::Inaccessible);
        };
        let same_claim = stored.current_turn_id == session.current_turn_id
            && stored.input_revision == session.input_revision
            && stored.control_revision == session.control_revision
            && stored.lease_token == session.lease_token
            && stored.turn_state == desk_diagnose_core::session::TurnState::Running;
        return Ok((receipt, same_claim.then(|| Box::new(stored))));
    }
    let first = producer
        .tool_calls
        .first()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let first = ToolCall {
        id: first.id.clone(),
        name: first.name.clone(),
        arguments_json: first.arguments_json.clone(),
    };
    let target = call
        .targets
        .first()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let logical_id = desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::AssistantReadAuthorization,
        &(
            &session.conversation_id,
            call.producer_message_id,
            &first.id,
        ),
    );
    let arguments = data_tools::ParsedDataCall::ManageMemory(
        desk_agent_protocol::ai_assistant::memory::MemoryWriteRequest::Forget {
            memory_id: target.memory_id.clone(),
            expected_revision: target.expected_revision,
        },
    );
    let context = AssistantDataCall {
        session,
        tool_call: &first,
        arguments: &arguments,
        producer_message: producer,
        logical_call_id: &logical_id,
        retrieval_reservation: None,
        producer_output_envelope: producer
            .data_envelope
            .as_ref()
            .ok_or(AssistantStoreError::Inaccessible)?,
        maximum_response_bytes: 2048,
        destination: call.destination,
        now_unix_ms: now,
    };
    let fence = desk_diagnose_core::future::boxed(|| {
        super::super::retrieval_native::prepare_memory_control_on(db, context, call)
    })
    .await?;
    if let Some(receipt) = fence
        .stale_control_receipt_on(
            call.operation_id,
            super::super::conversation_search::now_ms()?,
        )
        .await?
    {
        return Ok((receipt, None));
    }
    #[cfg(test)]
    eprintln!("memory forget: original native admitted");
    let OriginalAssistantDataCall::MemoryControl(original) = &fence.original else {
        return Err(AssistantStoreError::Inaccessible);
    };
    let request = ForgetGroupRequest {
        operation_id: call.operation_id.into(),
        producer_message_id: call.producer_message_id.into(),
        call_message_sha256: original.call_message_sha256().into(),
        owner_input_seq: original.owner_input_seq(),
        targets: original.targets().to_vec(),
    };
    match prepare_forget_group_on(db, &session.actor_id, &request, now).await? {
        ControlPreparation::Unchanged(receipt)
            if receipt.outcome == DataControlOutcome::StaleTarget =>
        {
            Ok((receipt, None))
        }
        ControlPreparation::Unchanged(_) => Err(AssistantStoreError::Inaccessible),
        ControlPreparation::Prepared(pending) => {
            desk_diagnose_core::future::boxed(|| commit_on(&fence, pending, now)).await
        }
    }
}

async fn commit_on(
    fence: &super::super::retrieval_native::NativeAssistantDataFence<'_>,
    mut pending: Box<PendingControl>,
    not_before: u64,
) -> Result<
    (
        AssistantDataControlReceipt,
        Option<Box<PersistedAgentSession>>,
    ),
    AssistantStoreError,
> {
    let db = fence.db;
    let receipt = pending.receipt()?;
    let now = super::super::conversation_search::now_ms()?;
    let at = chrono::DateTime::from_timestamp_millis(signed(now)?)
        .ok_or(AssistantStoreError::Inaccessible)?;
    let mut committed = Box::new(fence.context.session.clone());
    let OriginalAssistantDataCall::MemoryControl(original) = &fence.original else {
        return Err(AssistantStoreError::Inaccessible);
    };
    let group = data_control::commit_memory_forget_group(
        &mut committed,
        fence.context.producer_message.message_id.as_str(),
        original.call_message_sha256(),
        &receipt,
        &at.to_rfc3339(),
    )
    .map_err(AssistantStoreError::Invalid)?;
    pending
        .bind_tool_group_on(db, &group.assistant_message_id, &group.group_sha256)
        .await?;
    #[cfg(test)]
    eprintln!("memory forget: exact group bound");
    let deadline = fence.finish_after_control_on(now, &pending).await?;
    #[cfg(test)]
    eprintln!("memory forget: native after-control sources validated");
    let before = root_on(db, fence.context.session).await?;
    let original_row = fence.session_row();
    if before.version != original_row.version
        || before.lease_token != original_row.lease_token
        || before.state_json != original_row.state_json
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let mut expected = before.clone();
    expected.version = committed.version;
    expected.state_json = committed
        .encode_json_for_storage()
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    expected.updated_at = at;
    let changed = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(expected.state_json.clone()),
        )
        .col_expr(
            agent_session::Column::Version,
            Expr::value(expected.version),
        )
        .col_expr(agent_session::Column::UpdatedAt, Expr::value(at))
        .filter(agent_session::Column::Id.eq(before.id))
        .filter(agent_session::Column::ActorId.eq(&before.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&before.device_id))
        .filter(agent_session::Column::ConversationId.eq(&before.conversation_id))
        .filter(agent_session::Column::Version.eq(before.version))
        .filter(agent_session::Column::LeaseToken.eq(before.lease_token))
        .filter(agent_session::Column::StateJson.eq(&before.state_json))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 || root_on(db, &committed).await? != expected {
        return Err(AssistantStoreError::Inaccessible);
    }
    #[cfg(test)]
    eprintln!("memory forget: session after-image validated");
    let receipt = pending.finalize_on(db).await?;
    let completed = super::super::conversation_search::now_ms()?;
    if completed < now.max(not_before) || completed >= deadline {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok((receipt, Some(committed)))
}

async fn root_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<agent_session::Model, AssistantStoreError> {
    agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&session.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)
}
