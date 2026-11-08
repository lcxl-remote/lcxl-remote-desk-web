//! Bounded provenance GC. Only metadata without a live native/session/history,
//! fact or child dependency can be removed. Actor fences serialize parent pins.

use super::*;
use crate::entity::{
    agent_assistant_history as history, agent_assistant_quota as quota, agent_session as session,
    model_egress_receipt as native,
};
use sea_orm::{PaginatorTrait, QueryOrder, sea_query::Condition};

pub(crate) mod schedule;

pub(crate) use desk_assistant_data_store::output_gc::OutputGcOutcome;

/// The caller owns this complete actor transaction through polling and drop.
/// Type erasure keeps its real poll/drop frames in the library's stack report.
pub(crate) fn collect_actor(
    db: &crate::config::connection::DatabaseConnection,
    now: u64,
    maximum: u64,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<OutputGcOutcome, AssistantStoreError>> + Send + '_>,
> {
    desk_diagnose_core::future::send_phase(|| collect_actor_transaction(db, now, maximum))
}

async fn collect_actor_transaction(
    db: &crate::config::connection::DatabaseConnection,
    now: u64,
    maximum: u64,
) -> Result<OutputGcOutcome, AssistantStoreError> {
    use crate::config::ConfigConnection;
    let txn = crate::db::begin_write(db, archived::Entity).await?;
    let result = async {
        let policy = txn.config_read().await.assistant_data_policy.clone();
        collect_on(
            &txn,
            &crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string(),
            &policy,
            now,
            maximum,
        )
        .await
    }
    .await;
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

/// The caller holds the original actor fence. Cursor updates and all
/// native/source SELECTs finish before the one final quota mutation.
async fn collect_on(
    db: &DatabaseTransaction,
    actor: &str,
    policy: &AssistantDataPolicy,
    now: u64,
    maximum: u64,
) -> Result<OutputGcOutcome, AssistantStoreError> {
    let now = signed(now)?;
    let keep = i64::from(policy.history_retention_days)
        .checked_mul(86_400_000)
        .ok_or(AssistantStoreError::Capacity)?;
    let cutoff = now.checked_sub(keep).ok_or(AssistantStoreError::Capacity)?;
    let state = quota::Entity::find()
        .filter(quota::Column::ActorId.eq(actor))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    // A scan hint grants no source use. Invalid hints restart the bounded scan
    // and are repaired with their original raw values fenced at final quota CAS.
    let cursor = state
        .output_gc_completed_at_ms
        .zip(state.output_gc_archive_id)
        .filter(|(at, id)| *at > 0 && *id > 0);
    let mut query = archived::Entity::find()
        .filter(archived::Column::ActorId.eq(actor))
        .filter(archived::Column::CompletedAtMs.gt(0))
        .filter(archived::Column::Id.gt(0))
        .filter(archived::Column::CompletedAtMs.lte(cutoff));
    if let Some((at, id)) = cursor {
        query = query.filter(
            Condition::any()
                .add(archived::Column::CompletedAtMs.lt(at))
                .add(
                    Condition::all()
                        .add(archived::Column::CompletedAtMs.eq(at))
                        .add(archived::Column::Id.lt(id)),
                ),
        );
    }
    let keys = query
        .select_only()
        .column(archived::Column::CompletedAtMs)
        .column(archived::Column::Id)
        .order_by_desc(archived::Column::CompletedAtMs)
        .order_by_desc(archived::Column::Id)
        .limit(maximum)
        .into_tuple::<(i64, i64)>()
        .all(db)
        .await?;
    let next = if keys.len() as u64 == maximum {
        keys.last().copied()
    } else {
        None
    };
    let mut outcome = OutputGcOutcome {
        scanned: keys.len() as u64,
        cycle_finished: next.is_none(),
        ..Default::default()
    };
    for (at, id) in keys {
        desk_diagnose_core::future::send_phase(|| {
            collect_archive_on(db, actor, at, id, now, &mut outcome)
        })
        .await?;
    }
    // A durable cursor makes progress past held or corrupt rows. Every scan
    // cycle restarts so newly unpinned parents and new eligible rows are revisited.
    let receipts =
        desk_assistant_data_store::memory_write::receipts::collect_on(db, actor, now, maximum)
            .await?;
    outcome.refunded_metadata_bytes = outcome
        .refunded_metadata_bytes
        .checked_add(receipts)
        .ok_or(AssistantStoreError::Capacity)?;
    let citations = super::super::citations::collect_expired_on(db, actor, now, maximum).await?;
    outcome.refunded_metadata_bytes = outcome
        .refunded_metadata_bytes
        .checked_add(citations.refunded_bytes)
        .ok_or(AssistantStoreError::Capacity)?;
    outcome.cycle_finished &= !citations.has_more;
    outcome.invalid = outcome
        .invalid
        .checked_add(citations.invalid)
        .ok_or(AssistantStoreError::Capacity)?;
    desk_assistant_data_store::output_gc::finish_quota_on(
        db,
        desk_assistant_data_store::output_gc::GcCursor {
            id: state.id,
            actor_id: &state.actor_id,
            output: (state.output_gc_completed_at_ms, state.output_gc_archive_id),
        },
        next,
        outcome.refunded_metadata_bytes,
    )
    .await?;
    Ok(outcome)
}

async fn collect_archive_on(
    db: &DatabaseTransaction,
    actor: &str,
    at: i64,
    id: i64,
    now: i64,
    outcome: &mut OutputGcOutcome,
) -> Result<(), AssistantStoreError> {
    let row = archived::Entity::find()
        .filter(archived::Column::ActorId.eq(actor))
        .filter(archived::Column::Id.eq(id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.completed_at_ms != at {
        return Err(AssistantStoreError::Inaccessible);
    }
    if held_on(db, &row, now).await? {
        outcome.held += 1;
        return Ok(());
    }
    let stored = match decode_row_on(db, row).await {
        Ok(value) => value,
        Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
        Err(_) => {
            outcome.invalid += 1;
            return Ok(());
        }
    };
    let mut refund = metadata_bytes(&stored.row)?;
    let removed = archived::Entity::delete_many()
        .filter(archived::Column::Id.eq(stored.row.id))
        .filter(archived::Column::ActorId.eq(actor))
        .filter(archived::Column::PhysicalCallId.eq(&stored.row.physical_call_id))
        .filter(archived::Column::ProofJson.eq(&stored.row.proof_json))
        .exec(db)
        .await?;
    if removed.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    let remaining = archived::Entity::find()
        .filter(archived::Column::ActorId.eq(actor))
        .filter(archived::Column::ConversationId.eq(&stored.row.conversation_id))
        .count(db)
        .await?;
    if remaining == 0 {
        refund = refund
            .checked_add(
                desk_assistant_data_store::output_store::collect_items_on(
                    db,
                    actor,
                    &stored.row.conversation_id,
                )
                .await?,
            )
            .ok_or(AssistantStoreError::Capacity)?;
    }
    outcome.removed += 1;
    outcome.refunded_metadata_bytes = outcome
        .refunded_metadata_bytes
        .checked_add(refund)
        .ok_or(AssistantStoreError::Capacity)?;
    Ok(())
}

async fn held_on<C: ConnectionTrait>(
    db: &C,
    row: &archived::Model,
    now: i64,
) -> Result<bool, AssistantStoreError> {
    // Later calls in a conversation may reference any earlier output.
    if archived::Entity::find()
        .filter(archived::Column::ActorId.eq(&row.actor_id))
        .filter(archived::Column::ConversationId.eq(&row.conversation_id))
        .filter(archived::Column::Id.gt(row.id))
        .count(db)
        .await?
        > 0
        || native::Entity::find()
            .filter(native::Column::ReceiptId.eq(&row.physical_call_id))
            .count(db)
            .await?
            > 0
        || session::Entity::find()
            .filter(session::Column::ActorId.eq(&row.actor_id))
            .filter(session::Column::ConversationId.eq(&row.conversation_id))
            .count(db)
            .await?
            > 0
        || history::Entity::find()
            .filter(history::Column::ActorId.eq(&row.actor_id))
            .filter(history::Column::ConversationId.eq(&row.conversation_id))
            .filter(history::Column::DeletedAtMs.is_null())
            .filter(history::Column::ExpiresAtMs.gt(now))
            .count(db)
            .await?
            > 0
    {
        return Ok(true);
    }
    Ok(false)
}
