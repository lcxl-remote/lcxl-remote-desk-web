//! Final publication-model admission retains the original native holder and scope.
use super::*;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter};

pub(crate) async fn authorize_model_on(
    txn: &crate::config::connection::DatabaseTransaction,
    request: &desk_diagnose_core::seam::ModelRequest,
) -> Result<(), DbErr> {
    let Some(binding) = request.issue_reply.as_ref() else {
        return Ok(());
    };
    let denied = || DbErr::Custom("Original issue model work is no longer authorized".into());
    let origin = binding.origin();

    let row = crate::entity::agent_session::Entity::find()
        .filter(
            crate::entity::agent_session::Column::ConversationId
                .eq(&origin.execution_conversation_id),
        )
        .one(txn)
        .await?
        .ok_or_else(denied)?;
    let current =
        desk_diagnose_core::session::PersistedAgentSession::decode_boxed_json(&row.state_json)
            .map_err(|_| denied())?;
    if !binding.native_input_matches(&current) {
        return Err(denied());
    }
    let destination = request
        .messages
        .first()
        .and_then(|message| message.data_envelope.as_ref())
        .and_then(|label| label.allowed_destinations.first())
        .ok_or_else(denied)?;
    let original = SignalAgentRunEventStore::checked_external_read_input_on(
        txn,
        &current,
        destination,
        Some(&origin.tool_call_id),
    )
    .await
    .map_err(|_| denied())?;
    let call = current
        .conversation
        .iter()
        .flat_map(|message| &message.tool_calls)
        .find(|call| call.id == origin.tool_call_id)
        .ok_or_else(denied)?;
    let call = desk_diagnose_core::chat::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments_json.clone(),
    };
    let prepared = desk_assistant_data_store::issue_reply::preparation::prepare_on(
        txn,
        &current,
        &original,
        &call,
        destination,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .map_err(|_| denied())?;
    let desk_assistant_data_store::issue_reply::preparation::Preparation::Generate(work) = prepared
    else {
        return Err(denied());
    };
    if work.origin != *origin
        || work.access != *binding.access()
        || request.messages.get(1..) != Some(std::slice::from_ref(&work.material))
    {
        return Err(denied());
    }
    let config = issue_connections::read(txn).await.map_err(|_| denied())?;
    if config
        .connection_for_issue(&origin.actor_id, &work.access)
        .is_none()
    {
        return Err(denied());
    }
    Ok(())
}
