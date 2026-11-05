//! Dispatch runs outside the model's session and settles only its original artifact.

use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_assistant_data_store::issue_reply::{
    Claim,
    delivery::{self, CLAIM_LEASE_MS, RETRY_DELAY_MS},
};
use desk_signal_facade::mcp_client::{
    replies::IssueReplyTransport, settings::IssueConnectionConfig,
};

/// Resolve the original admitted scope and current subscription at both dispatch
/// checks. Reconciliation of an already dispatched write needs no new grant.
pub async fn deliver_next(
    db: &DatabaseConnection,
    node: &str,
    token: &str,
) -> Result<bool, AgentError> {
    deliver_next_with_authority(db, node, token, async |txn, artifact| {
        desk_assistant_data_store::issue_authority::publication_revision(
            txn,
            &artifact.origin,
            &artifact.access,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|error| error.into_agent_error())
    })
    .await
}

async fn deliver_next_with_authority(
    db: &DatabaseConnection,
    node: &str,
    token: &str,
    automatic: impl AsyncFn(&DatabaseTransaction, &FrozenIssueReply) -> Result<Option<u64>, AgentError>,
) -> Result<bool, AgentError> {
    let now = chrono::Utc::now().timestamp_millis();
    let txn = crate::db::begin_write(db, agent_issue_reply::Entity)
        .await
        .map_err(|_| unavailable())?;
    let claim = issue_reply::claim_next(&txn, node, token, now, now.saturating_add(CLAIM_LEASE_MS))
        .await
        .map_err(|error| error.into_agent_error())?;
    if let Some(claim) = &claim
        && !claim.needs_reconciliation()
    {
        let artifact = claim
            .publication()
            .map_err(|error| error.into_agent_error())?;
        let revision = automatic(&txn, artifact)
            .await?
            .filter(|revision| *revision >= artifact.origin.authorization_revision);
        let now = chrono::Utc::now().timestamp_millis();
        let reason = if revision.is_none() {
            Some("authorization_unavailable")
        } else if artifact.authorize_delivery(now as u64).is_err() {
            Some("source_not_publishable")
        } else {
            None
        };
        if let Some(reason) = reason {
            issue_reply::keep_internal(&txn, claim, reason, now)
                .await
                .map_err(|error| error.into_agent_error())?;
            txn.commit().await.map_err(|_| unavailable())?;
            return Ok(true);
        }
    }
    txn.commit().await.map_err(|_| unavailable())?;
    let Some(claim) = claim else { return Ok(false) };
    let config = issue_connections::read(db)
        .await
        .map_err(|_| unavailable())?;
    let Some(connection) = config
        .connection_for_issue(&claim.row.actor_id, &claim.receipt.access)
        .cloned()
    else {
        unavailable_connection(db, &claim).await?;
        return Ok(true);
    };
    let transport = match IssueReplyTransport::from_config(connection.clone()).await {
        Ok(transport) => transport,
        Err(desk_diagnose_core::issue_reply::ReplyTransportError::InvalidBinding) => {
            unavailable_connection(db, &claim).await?;
            return Ok(true);
        }
        Err(desk_diagnose_core::issue_reply::ReplyTransportError::Unavailable) => {
            let now = chrono::Utc::now().timestamp_millis();
            if claim.needs_reconciliation() {
                unavailable_connection(db, &claim).await?;
            } else {
                issue_reply::release_unsent(db, &claim, now, now.saturating_add(RETRY_DELAY_MS))
                    .await
                    .map_err(|error| error.into_agent_error())?;
            }
            return Ok(true);
        }
    };
    delivery::process(db, &claim, &transport, async |claim| {
        commit_dispatch(db, claim, &connection, &automatic).await
    })
    .await?;
    Ok(true)
}

async fn unavailable_connection(db: &DatabaseConnection, claim: &Claim) -> Result<(), AgentError> {
    let now = chrono::Utc::now().timestamp_millis();
    if claim.needs_reconciliation() {
        let page = u32::try_from(claim.row.reconciliation_page)
            .ok()
            .and_then(std::num::NonZeroU32::new)
            .ok_or_else(unavailable)?;
        issue_reply::defer_reconciliation(db, claim, now, now.saturating_add(RETRY_DELAY_MS), page)
            .await
            .map_err(|error| error.into_agent_error())?;
    } else {
        issue_reply::keep_internal(db, claim, "connection_unavailable", now)
            .await
            .map_err(|error| error.into_agent_error())?;
    }
    Ok(())
}

async fn commit_dispatch(
    db: &DatabaseConnection,
    claim: &Claim,
    connection: &IssueConnectionConfig,
    automatic: &impl AsyncFn(&DatabaseTransaction, &FrozenIssueReply) -> Result<Option<u64>, AgentError>,
) -> Result<bool, AgentError> {
    let txn = crate::db::begin_write(db, agent_issue_reply::Entity)
        .await
        .map_err(|_| unavailable())?;
    let current = issue_connections::read(&txn)
        .await
        .map_err(|_| unavailable())?;
    if current.connection_for_issue(&claim.row.actor_id, &claim.receipt.access) != Some(connection)
    {
        txn.commit().await.map_err(|_| unavailable())?;
        return Ok(false);
    }
    let artifact = claim
        .publication()
        .map_err(|error| error.into_agent_error())?;
    let revision = automatic(&txn, artifact)
        .await?
        .filter(|revision| *revision >= artifact.origin.authorization_revision);
    let now = chrono::Utc::now().timestamp_millis();
    if revision.is_none() || artifact.authorize_delivery(now as u64).is_err() {
        issue_reply::keep_internal(
            &txn,
            claim,
            if revision.is_some() {
                "source_not_publishable"
            } else {
                "authorization_unavailable"
            },
            now,
        )
        .await
        .map_err(|error| error.into_agent_error())?;
        txn.commit().await.map_err(|_| unavailable())?;
        return Ok(false);
    }
    let marked = issue_reply::mark_dispatched(&txn, claim, revision.ok_or_else(unavailable)?, now)
        .await
        .map_err(|error| error.into_agent_error())?;
    txn.commit().await.map_err(|_| unavailable())?;
    Ok(marked)
}

#[cfg(test)]
mod tests;
