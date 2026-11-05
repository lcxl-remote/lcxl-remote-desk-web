//! Durable REST polling. Source reads never acquire a device or model turn.

use crate::entity::agent_session;
use crate::{
    ai_assistant_gate::AiAssistantGate,
    config::connection::{DatabaseConnection, DatabaseTransaction},
    issue_connections,
};
use desk_agent_protocol::AgentError;
use desk_assistant_data_store::{
    entity::agent_issue_source,
    issue_authority,
    issue_events::{self, SourceClaim},
    main_request,
};
use desk_diagnose_core::{issue_authority::IssueSubscription, session::PersistedAgentSession};
use desk_signal_facade::mcp_client::{
    issue_connection::RepositoryCommentPage, settings::IssueConnectionConfig,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

mod routing;
pub use routing::scan_routing_once;

const BATCH_SIZE: u64 = 16;
const LEASE_MS: i64 = 120_000;
const POLL_DELAY_MS: i64 = 30_000;
const RETRY_DELAY_MS: i64 = 5_000;

pub async fn scan_once(
    db: &DatabaseConnection,
    gate: &AiAssistantGate,
    after: i64,
) -> Result<i64, AgentError> {
    if !gate.is_enabled() {
        return Ok(0);
    }
    let now = chrono::Utc::now().timestamp_millis();
    let rows = issue_events::candidates_on(db, after, now, BATCH_SIZE)
        .await
        .map_err(store)?;
    let next = if rows.len() == BATCH_SIZE as usize {
        rows.last().unwrap().id
    } else {
        0
    };
    for row in rows {
        if poll_source(db, gate, &row, &uuid::Uuid::new_v4().to_string())
            .await
            .is_err()
        {
            log::debug!("[issue-source] source page deferred");
        }
    }
    Ok(next)
}

async fn poll_source(
    db: &DatabaseConnection,
    gate: &AiAssistantGate,
    hint: &agent_issue_source::Model,
    token: &str,
) -> Result<(), AgentError> {
    let txn = crate::db::begin_write(db, agent_issue_source::Entity)
        .await
        .map_err(|_| unavailable())?;
    let Some(policy) = issue_authority::current(&txn, &hint.actor_id, &hint.subscription_id)
        .await
        .map_err(store)?
    else {
        return Ok(());
    };
    if !policy.enabled || coordinator_on(&txn, &policy).await?.is_none() {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp_millis();
    let Some(claim) =
        issue_events::claim_on(&txn, &policy, token, now, now.saturating_add(LEASE_MS))
            .await
            .map_err(store)?
    else {
        return Ok(());
    };
    let config = issue_connections::read(&txn)
        .await
        .map_err(|_| unavailable())?;
    let connection = config
        .connection_for_repository(&policy.actor_id, &policy.repository)
        .cloned();
    txn.commit().await.map_err(|_| unavailable())?;
    let Some(connection) = connection else {
        return finish_page(db, gate, &claim, None, Err("connection_unavailable")).await;
    };
    let page = read_page(&claim, connection.clone()).await;
    finish_page(db, gate, &claim, Some(&connection), page).await
}

async fn read_page(
    claim: &SourceClaim,
    connection: IssueConnectionConfig,
) -> Result<RepositoryCommentPage, &'static str> {
    let client = tokio::task::spawn_blocking(move || connection.client())
        .await
        .map_err(|_| "source_unavailable")?
        .map_err(|_| "connection_unavailable")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        client.repository_comments_page(
            &claim.policy.repository,
            claim.scan.since_ms,
            claim.scan.page,
        ),
    )
    .await
    .map_err(|_| "source_timeout")?
    .map_err(|_| "source_unavailable")
}

async fn finish_page(
    db: &DatabaseConnection,
    gate: &AiAssistantGate,
    claim: &SourceClaim,
    connection: Option<&IssueConnectionConfig>,
    page: Result<RepositoryCommentPage, &'static str>,
) -> Result<(), AgentError> {
    let txn = crate::db::begin_write(db, agent_issue_source::Entity)
        .await
        .map_err(|_| unavailable())?;
    if !gate.is_enabled() || coordinator_on(&txn, &claim.policy).await?.is_none() {
        return Err(unavailable());
    }
    let config = issue_connections::read(&txn)
        .await
        .map_err(|_| unavailable())?;
    let current =
        config.connection_for_repository(&claim.policy.actor_id, &claim.policy.repository);
    let page = if current == connection {
        page
    } else {
        Err("connection_changed")
    };
    let now = chrono::Utc::now().timestamp_millis();
    match page {
        Ok(page) => {
            let comments = page
                .comments
                .into_iter()
                .map(|comment| comment.freeze(&claim.policy.repository))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| unavailable())?;
            // The incomplete window remains immediately eligible; completed
            // scans use the source interval without holding a database lease.
            issue_events::commit_page_on(
                &txn,
                claim,
                &comments,
                page.next_page,
                now,
                now.saturating_add(POLL_DELAY_MS),
            )
            .await
            .map_err(store)?;
        }
        Err(reason) => {
            issue_events::retry_on(&txn, claim, now, now.saturating_add(RETRY_DELAY_MS), reason)
                .await
                .map_err(store)?
        }
    }
    txn.commit().await.map_err(|_| unavailable())?;
    Ok(())
}

async fn coordinator_on(
    db: &DatabaseTransaction,
    policy: &IssueSubscription,
) -> Result<Option<Box<PersistedAgentSession>>, AgentError> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(&policy.actor_id))
        .filter(agent_session::Column::ConversationId.eq(&policy.conversation_id))
        .one(db)
        .await
        .map_err(|_| unavailable())?;
    let Some(row) = row else {
        return Ok(None);
    };
    let held =
        PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|_| unavailable())?;
    Ok((held.actor_id == policy.actor_id
        && held.conversation_id == policy.conversation_id
        && held.version == row.version
        && !held.main_stopped
        && main_request::is_workbench_root(&held))
    .then_some(held))
}

fn store(error: desk_assistant_data_store::AssistantStoreError) -> AgentError {
    error.into_agent_error()
}
fn unavailable() -> AgentError {
    desk_diagnose_core::subagent::invalid("External issue source is unavailable")
}

#[cfg(test)]
mod tests;
