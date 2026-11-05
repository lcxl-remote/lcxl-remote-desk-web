//! One claim's preparation, dispatch and settlement, with no session continuation.

use super::*;
use desk_agent_protocol::AgentError;
use desk_diagnose_core::issue_reply::{ReplyTransport, ReplyTransportError};
use std::num::NonZeroU32;

pub const CLAIM_LEASE_MS: i64 = 90_000;
pub const RETRY_DELAY_MS: i64 = 30_000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Only `commit_dispatch` may grant a POST. Hosts recheck their current policy
/// and connection in that transaction and commit the exact claim's intent.
pub async fn process(
    db: &impl ConnectionTrait,
    claim: &Claim,
    transport: &impl ReplyTransport,
    commit_dispatch: impl AsyncFn(&Claim) -> Result<bool, AgentError>,
) -> Result<(), AgentError> {
    if claim.needs_reconciliation() {
        return reconcile(db, claim, transport).await;
    }
    let artifact = claim.publication().map_err(store_error)?;
    let prepared = match transport.prepare(artifact).await {
        Ok(prepared) => prepared,
        Err(ReplyTransportError::Unavailable) => {
            let now = now_ms();
            release_unsent(db, claim, now, now.saturating_add(RETRY_DELAY_MS))
                .await
                .map_err(store_error)?;
            return Ok(());
        }
        Err(ReplyTransportError::InvalidBinding) => {
            keep_internal(db, claim, "external_binding_changed", now_ms())
                .await
                .map_err(store_error)?;
            return Ok(());
        }
    };
    if !commit_dispatch(claim).await? {
        let now = now_ms();
        release_unsent(db, claim, now, now.saturating_add(RETRY_DELAY_MS))
            .await
            .map_err(store_error)?;
        return Ok(());
    }
    let dispatch = claim
        .row
        .claim_token
        .as_deref()
        .ok_or_else(|| store_error(invalid()))?;
    match transport.send(prepared).await {
        Ok(receipt) => {
            record_receipt(db, claim, dispatch, &receipt, now_ms())
                .await
                .map_err(store_error)?;
        }
        Err(_) => {
            mark_unknown(db, &claim.row.artifact_id, dispatch, now_ms())
                .await
                .map_err(store_error)?;
        }
    }
    Ok(())
}

async fn reconcile(
    db: &impl ConnectionTrait,
    claim: &Claim,
    transport: &impl ReplyTransport,
) -> Result<(), AgentError> {
    let page = u32::try_from(claim.row.reconciliation_page)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| store_error(invalid()))?;
    let dispatch = claim
        .row
        .dispatch_token
        .as_deref()
        .ok_or_else(|| store_error(invalid()))?;
    let next = match transport.reconcile(&claim.receipt, page).await {
        Ok(result) => {
            if let Some(receipt) = result.receipt {
                record_receipt(db, claim, dispatch, &receipt, now_ms())
                    .await
                    .map_err(store_error)?;
                return Ok(());
            }
            result.next_page.unwrap_or(NonZeroU32::MIN)
        }
        Err(_) => page,
    };
    let now = now_ms();
    defer_reconciliation(db, claim, now, now.saturating_add(RETRY_DELAY_MS), next)
        .await
        .map_err(store_error)?;
    Ok(())
}

fn store_error(error: AssistantStoreError) -> AgentError {
    error.into_agent_error()
}
