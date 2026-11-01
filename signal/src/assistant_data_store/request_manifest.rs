//! Native preflight for the exact direct-read request manifest. No projection
//! is consumed and no physical-call authorization is created here.

use super::{AssistantStoreError, conversation_search::now_ms, unsigned};
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use desk_diagnose_core::{
    ai_assistant::{data_access::retained_context_eligible, seam::AssistantModelDataManifest},
    seam::ModelRequest,
    session::PersistedAgentSession,
};
use sea_orm::TransactionTrait;

pub(crate) async fn prepare(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    request: &ModelRequest,
    not_before: u64,
) -> Result<Option<AssistantModelDataManifest>, AssistantStoreError> {
    if !retained_context_eligible(session) {
        AssistantModelDataManifest::validate_absence(request)
            .map_err(AssistantStoreError::Invalid)?;
        return Ok(None);
    }
    let txn = db.begin().await?;
    match desk_diagnose_core::future::boxed(|| prepare_on(&txn, session, request, not_before)).await
    {
        Ok(result) => {
            txn.commit().await?;
            Ok(Some(result))
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

async fn prepare_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &ModelRequest,
    not_before: u64,
) -> Result<AssistantModelDataManifest, AssistantStoreError> {
    // The caller and this transaction can straddle a wall-clock adjustment.
    let now = now_ms()?.max(not_before);
    let fence =
        desk_diagnose_core::future::boxed(|| super::current_native::prepare_on(db, session, now))
            .await?;
    let ids = AssistantModelDataManifest::selected_read_ids(session, request)
        .map_err(AssistantStoreError::Invalid)?;
    let mut stored = Vec::with_capacity(ids.len());
    for id in ids {
        let projection = super::projection::load_on(db, &session.actor_id, &id)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
        if projection.row.revoked_at_ms.is_some()
            || !projection.proof.preparation_available(
                now,
                projection
                    .row
                    .first_consumed_at_ms
                    .map(unsigned)
                    .transpose()?,
            )
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        stored.push(projection);
    }
    let proofs = stored
        .iter()
        .map(|stored| stored.proof.as_ref())
        .collect::<Vec<_>>();
    let manifest =
        AssistantModelDataManifest::for_direct_request(session, request, &fence.access, &proofs)
            .map_err(AssistantStoreError::Invalid)?;
    let finished_at = now_ms()?.max(now);
    fence.finish_on(finished_at).await?;
    let final_now = now_ms()?.max(finished_at);
    for stored in &stored {
        let consumed_at = stored.row.first_consumed_at_ms.map(unsigned).transpose()?;
        if !stored.proof.preparation_available(final_now, consumed_at) {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    Ok(manifest)
}
