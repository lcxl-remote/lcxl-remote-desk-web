//! Native publication acceptance against the canonical original-work authority.

use crate::{
    agent_run_event_store::SignalAgentRunEventStore, config::connection::DatabaseConnection,
    issue_connections, model_egress_store::SignalModelEgressStore,
};
use desk_agent_protocol::AgentError;
use desk_assistant_data_store::{
    entity::agent_issue_reply,
    issue_reply::{self, DeliveryMode, ReplyPreparation},
};
use desk_diagnose_core::issue_reply::FrozenIssueReply;

/// The original caller retains the model, lease, budget and cancellation scope.
pub async fn freeze(
    db: &DatabaseConnection,
    prepared: ReplyPreparation<'_>,
) -> Result<agent_issue_reply::Model, AgentError> {
    prepared
        .check_context()
        .map_err(|error| error.into_agent_error())?;
    let txn = crate::db::begin_write(db, crate::entity::agent_session::Entity)
        .await
        .map_err(|_| unavailable())?;
    let sources = SignalModelEgressStore::read_context_output_on(
        &txn,
        &prepared.held.conversation_id,
        &prepared.output,
    )
    .await
    .map_err(|_| unavailable())?;
    let original = SignalAgentRunEventStore::checked_external_read_input_on(
        &txn,
        prepared.held,
        &sources.evidence.destination,
        Some(&prepared.origin.tool_call_id),
    )
    .await?;
    if !original.external_issues.contains(&prepared.access) {
        return Err(unavailable());
    }
    let artifact = FrozenIssueReply::freeze(
        prepared.origin,
        prepared.access,
        prepared.body,
        prepared.output,
        sources,
        prepared.comment_tool_signature,
        &prepared.random_marker,
    )?;
    let config = issue_connections::read(&txn)
        .await
        .map_err(|_| unavailable())?;
    let mode = if config
        .connection_for_issue(&artifact.origin.actor_id, &artifact.access)
        .is_some()
        && desk_assistant_data_store::issue_authority::publication_revision_for_input(
            &txn,
            prepared.held,
            &original,
            &artifact.origin,
            &artifact.access,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|error| error.into_agent_error())?
        .is_some_and(|revision| revision >= artifact.origin.authorization_revision)
    {
        DeliveryMode::Automatic
    } else {
        DeliveryMode::Internal
    };
    let row = issue_reply::freeze(&txn, &artifact, mode, chrono::Utc::now().timestamp_millis())
        .await
        .map_err(|error| error.into_agent_error())?;
    txn.commit().await.map_err(|_| unavailable())?;
    Ok(row)
}

mod delivery;
pub use delivery::deliver_next;

fn unavailable() -> AgentError {
    desk_diagnose_core::subagent::invalid("Original issue publication is unavailable")
}

mod model;
pub(crate) use model::authorize_model_on;
