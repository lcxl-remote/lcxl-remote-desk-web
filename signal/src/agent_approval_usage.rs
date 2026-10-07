//! Cumulative reviewer accounting; late usage never grants new authority.
use crate::entity::{
    agent_approval_delegation as delegation_row, agent_approval_review as review_row,
};
use desk_diagnose_core::{
    approval_cost::ReviewUsageSettlement, subagent::reservation::ReviewCallAuthority,
};
use sea_orm::{
    ColumnTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait, Set,
};

fn invalid() -> DbErr {
    DbErr::Custom("review usage accounting is inconsistent".into())
}

// These links contain only durable identities. Keeping them does not extend
// execution authority or preserve the reviewed prompt body.
fn accounting_pending() -> sea_orm::Condition {
    sea_orm::Condition::any()
        .add(review_row::Column::Status.eq("reviewing"))
        .add(review_row::Column::UsageSettlementState.eq("unknown"))
}

pub(crate) fn pinned_delegation_ids() -> sea_orm::sea_query::SelectStatement {
    review_row::Entity::find()
        .select_only()
        .column(review_row::Column::DelegationId)
        .filter(accounting_pending())
        .into_query()
}

pub(crate) fn pinned_reservation_ids() -> sea_orm::sea_query::SelectStatement {
    review_row::Entity::find()
        .select_only()
        .column(review_row::Column::DelegationReservationId)
        .filter(review_row::Column::DelegationReservationId.is_not_null())
        .filter(accounting_pending())
        .into_query()
}

pub(crate) async fn record_usage_on(
    txn: &DatabaseTransaction,
    row: &review_row::Model,
    usage: ReviewUsageSettlement,
    now_ms: i64,
) -> Result<(), DbErr> {
    usage
        .validate(
            u64::try_from(row.reserved_tokens).map_err(|_| invalid())?,
            u64::try_from(row.reserved_cost_micros).map_err(|_| invalid())?,
        )
        .ok_or_else(invalid)?;
    let changed = review_row::Entity::update_many()
        .set(review_row::ActiveModel {
            usage_settlement_json: Set(Some(serde_json::to_string(&usage).map_err(|_| invalid())?)),
            usage_settlement_state: Set(Some(usage.state().into())),
            usage_reconcile_at_ms: Set(Some(now_ms)),
            ..Default::default()
        })
        .filter(review_row::Column::Id.eq(row.id))
        .filter(review_row::Column::Status.eq("reviewing"))
        .filter(review_row::Column::LeaseEpoch.eq(row.lease_epoch))
        .filter(review_row::Column::UsageSettlementJson.is_null())
        .filter(review_row::Column::UsageSettlementState.is_null())
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

async fn stamp_attempt(
    db: &DatabaseConnection,
    snapshot: &review_row::Model,
    now_ms: i64,
) -> Result<(), DbErr> {
    let mut update = review_row::Entity::update_many()
        .set(review_row::ActiveModel {
            usage_reconcile_at_ms: Set(Some(now_ms)),
            ..Default::default()
        })
        .filter(review_row::Column::Id.eq(snapshot.id))
        .filter(review_row::Column::UsageSettlementState.eq("unknown"));
    update = match snapshot.usage_reconcile_at_ms {
        Some(previous) => update.filter(review_row::Column::UsageReconcileAtMs.eq(previous)),
        None => update.filter(review_row::Column::UsageReconcileAtMs.is_null()),
    };
    update.exec(db).await?;
    Ok(())
}

async fn reconcile_one(
    db: &DatabaseConnection,
    snapshot: &review_row::Model,
    now_ms: i64,
) -> Result<bool, DbErr> {
    let authority: ReviewCallAuthority = serde_json::from_str(
        snapshot
            .call_authority_json
            .as_deref()
            .ok_or_else(invalid)?,
    )
    .map_err(|_| invalid())?;
    authority.validate().map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, review_row::Entity).await?;

    let Some(row) = review_row::Entity::find_by_id(snapshot.id)
        .one(&txn)
        .await?
    else {
        return Ok(false);
    };
    if row.status == "reviewing" || row.usage_settlement_state.as_deref() != Some("unknown") {
        return Ok(false);
    }
    let current: ReviewCallAuthority =
        serde_json::from_str(row.call_authority_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    if current != authority {
        return Err(invalid());
    }
    let encoded = row.usage_settlement_json.as_deref().ok_or_else(invalid)?;
    let previous: ReviewUsageSettlement = serde_json::from_str(encoded).map_err(|_| invalid())?;
    previous
        .validate(
            u64::try_from(row.reserved_tokens).map_err(|_| invalid())?,
            u64::try_from(row.reserved_cost_micros).map_err(|_| invalid())?,
        )
        .ok_or_else(invalid)?;
    if previous.state() != "unknown" || row.provider_receipt_id.is_none() {
        return Err(invalid());
    }
    let physical =
        crate::agent_subagent_store::settle_review_call_on(&txn, &row, None, None, now_ms).await?;
    let actual = physical.settlement(row.reserved_tokens, row.reserved_cost_micros)?;
    if !actual.usage_known {
        return Ok(false);
    }
    if !actual.provider_started {
        return Err(invalid());
    }
    let stored = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&row.delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(&row.conversation_id))
        .filter(delegation_row::Column::ActorId.eq(&row.actor_id))
        .filter(delegation_row::Column::DeviceId.eq(&row.device_id))
        .lock_exclusive()
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut delegation = crate::agent_approval_store::decode(&stored)?;
    delegation
        .reconcile_review_usage(previous, actual)
        .map_err(|_| invalid())?;
    let changed = delegation_row::Entity::update_many()
        .set(delegation_row::ActiveModel {
            state_json: Set(serde_json::to_string(&delegation).map_err(|_| invalid())?),
            version: Set(i64::try_from(delegation.ledger_version).map_err(|_| invalid())?),
            updated_at: Set(now_ms),
            ..Default::default()
        })
        .filter(delegation_row::Column::Id.eq(stored.id))
        .filter(delegation_row::Column::Version.eq(stored.version))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    let changed = review_row::Entity::update_many()
        .set(review_row::ActiveModel {
            usage_settlement_json: Set(Some(
                serde_json::to_string(&actual).map_err(|_| invalid())?,
            )),
            usage_settlement_state: Set(Some("known".into())),
            usage_reconcile_at_ms: Set(Some(now_ms)),
            ..Default::default()
        })
        .filter(review_row::Column::Id.eq(row.id))
        .filter(review_row::Column::Status.eq(&row.status))
        .filter(review_row::Column::UsageSettlementState.eq("unknown"))
        .filter(review_row::Column::UsageSettlementJson.eq(encoded))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    txn.commit().await?;
    Ok(true)
}

/// Ordering by the independent attempt clock rotates unknown and malformed
/// records. An error on one row cannot prevent later rows from reconciling.
pub async fn reconcile_usage(
    db: &DatabaseConnection,
    now_ms: i64,
    limit: u64,
) -> Result<u64, DbErr> {
    if now_ms <= 0 || !(1..=64).contains(&limit) {
        return Err(invalid());
    }
    let rows = review_row::Entity::find()
        .filter(review_row::Column::UsageSettlementState.eq("unknown"))
        .filter(review_row::Column::Status.ne("reviewing"))
        .order_by_asc(review_row::Column::UsageReconcileAtMs)
        .order_by_asc(review_row::Column::Id)
        .limit(limit)
        .all(db)
        .await?;
    let mut reconciled = 0;
    for row in rows {
        match reconcile_one(db, &row, now_ms).await {
            Ok(true) => reconciled += 1,
            Ok(false) => stamp_attempt(db, &row, now_ms).await?,
            Err(error) => {
                log::warn!(
                    "[approval] usage reconciliation failed for row {}: {error}",
                    row.id
                );
                stamp_attempt(db, &row, now_ms).await?;
            }
        }
    }
    Ok(reconciled)
}
