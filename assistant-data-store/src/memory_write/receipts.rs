//! Commit metadata lives at least as long as its native output can be replayed.

use super::*;
use crate::entity::agent_assistant_model_output as output;
use sea_orm::{QueryOrder, QuerySelect, QueryTrait};

pub fn metadata_bytes(row: &receipt::Model) -> Result<i64, AssistantStoreError> {
    let text = [
        &row.commit_id,
        &row.actor_id,
        row.instance_id.as_deref().unwrap_or(""),
        &row.source_call_id,
        &row.request_sha256,
        &row.result_json,
        &row.result_sha256,
    ];
    text.iter().try_fold(5_i64 * 8, |sum, value| {
        sum.checked_add(value.len() as i64)
            .ok_or(AssistantStoreError::Capacity)
    })
}

/// The host includes this refund in its existing final GC quota write.
pub async fn collect_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    now: i64,
    maximum: u64,
) -> Result<i64, AssistantStoreError> {
    let live = output::Entity::find()
        .select_only()
        .column(output::Column::PhysicalCallId)
        .filter(output::Column::ActorId.eq(actor))
        .into_query();
    let rows = receipt::Entity::find()
        .filter(receipt::Column::ActorId.eq(actor))
        .filter(receipt::Column::ExpiresAtMs.lte(now))
        .filter(receipt::Column::SourceCallId.not_in_subquery(live))
        .order_by_asc(receipt::Column::Id)
        .limit(maximum)
        .all(db)
        .await?;
    let mut refund = 0_i64;
    for row in rows {
        let removed = receipt::Entity::delete_many()
            .filter(receipt::Column::ActorId.eq(actor))
            .filter(receipt::Column::Id.eq(row.id))
            .filter(receipt::Column::CommitId.eq(&row.commit_id))
            .filter(receipt::Column::RequestSha256.eq(&row.request_sha256))
            .filter(receipt::Column::ResultSha256.eq(&row.result_sha256))
            .exec(db)
            .await?;
        if removed.rows_affected != 1 {
            return Err(AssistantStoreError::Inaccessible);
        }
        refund = refund
            .checked_add(metadata_bytes(&row)?)
            .ok_or(AssistantStoreError::Capacity)?;
    }
    Ok(refund)
}
