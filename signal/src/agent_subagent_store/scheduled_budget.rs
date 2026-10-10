//! One occurrence quota for parent, children, compression and safety calls.

use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::{agent_delegation_reservation as cost, agent_task_budget_reservation as quota};
use desk_diagnose_core::subagent::{
    budget::Usage,
    reservation::{DelegationCallKind, DelegationCallReservation},
};
use sea_orm::sea_query::Expr;
use sha2::{Digest, Sha256};

fn key(reservation: &DelegationCallReservation) -> String {
    format!("delegated-model:{}", reservation.reservation_id)
}

/// This is part of the allocation transaction; quota failure rolls back both
/// reservations. A replay only checks the original link and never allocates again.
pub(super) async fn reserve_on(
    txn: &DatabaseTransaction,
    source: Option<&crate::schedule_store::CurrentDelegationSourceAuthority>,
    reservation: &DelegationCallReservation,
    newly_reserved: bool,
) -> Result<bool, DbErr> {
    let Some(source) = source else {
        return Ok(true);
    };
    if reservation.kind == DelegationCallKind::Tool {
        return Ok(true);
    }
    let row = cost::Entity::find()
        .filter(cost::Column::ReservationId.eq(&reservation.reservation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if budget::decode_reservation(&row)? != *reservation {
        return Err(invalid());
    }
    if !newly_reserved {
        let group = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&reservation.group_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        linked_on(txn, &group, &row, reservation).await?;
        return Ok(true);
    }
    if row.source_schedule_budget_id.is_some() || row.version != 1 {
        return Err(invalid());
    }
    let allocation = crate::schedule_store::ScheduleStore::reserve_delegation_model_budget_on(
        txn,
        source,
        &key(reservation),
        &reservation.arguments_sha256,
        reservation.upper.tokens,
    )
    .await;
    let allocation = match allocation {
        Ok(value) => value,
        Err(crate::schedule_store::ScheduleStoreError::BudgetExceeded) => return Ok(false),
        Err(_) => return Err(invalid()),
    };
    let changed = cost::Entity::update_many()
        .set(cost::ActiveModel {
            source_schedule_budget_id: Set(Some(allocation.reservation_id)),
            version: Set(2),
            ..Default::default()
        })
        .filter(cost::Column::Id.eq(row.id))
        .filter(cost::Column::Version.eq(1))
        .filter(cost::Column::SourceScheduleBudgetId.is_null())
        .filter(cost::Column::State.eq("reserved"))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(true)
}

/// Minimal immutable links survive content redaction; historical accounting does
/// not need the original prompt or another execution authorization.
async fn linked_on(
    txn: &DatabaseTransaction,
    group: &group_row::Model,
    row: &cost::Model,
    reservation: &DelegationCallReservation,
) -> Result<Option<quota::Model>, DbErr> {
    let scheduled_model =
        group.source_schedule_id.is_some() && reservation.kind != DelegationCallKind::Tool;
    if !scheduled_model {
        if row.source_schedule_budget_id.is_some() {
            return Err(invalid());
        }
        return Ok(None);
    }
    let id = row
        .source_schedule_budget_id
        .as_deref()
        .ok_or_else(invalid)?;
    let allocation = quota::Entity::find()
        .filter(quota::Column::ReservationId.eq(id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if allocation.schedule_id != *group.source_schedule_id.as_ref().ok_or_else(invalid)?
        || allocation.run_id != *group.source_occurrence_id.as_ref().ok_or_else(invalid)?
        || allocation.run_id != reservation.root_conversation_id
        || allocation.owner_user_id.to_string() != group.actor_id
        || allocation.kind != "model_tokens"
        || allocation.rule_id.is_some()
        || allocation.exception_grant_id.is_some()
        || allocation.logical_key_sha256
            != format!("{:x}", Sha256::digest(key(reservation).as_bytes()))
        || allocation.input_sha256 != reservation.arguments_sha256
        || u64::try_from(allocation.reserved_units).ok() != Some(reservation.upper.tokens)
    {
        return Err(invalid());
    }
    Ok(Some(allocation))
}

pub(super) async fn validate_dispatch_on(
    txn: &DatabaseTransaction,
    row: &cost::Model,
    reservation: &DelegationCallReservation,
) -> Result<(), DbErr> {
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&reservation.group_id))
        .filter(group_row::Column::RootConversationId.eq(&reservation.root_conversation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if let Some(allocation) = linked_on(txn, &group, row, reservation).await?
        && (allocation.state != "reserved" || allocation.charged_units != allocation.reserved_units)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Acquire the publication write fence before cost/group settlement, even after
/// cancellation or expiry. This lock proves ordering, never renewed authority.
pub(super) async fn lock_historical_on(
    txn: &DatabaseTransaction,
    group: &group_row::Model,
) -> Result<(), DbErr> {
    if let Some(schedule_id) = &group.source_schedule_id {
        use crate::entity::agent_schedule as schedule;
        let changed = schedule::Entity::update_many()
            .col_expr(
                schedule::Column::Revision,
                Expr::col(schedule::Column::Revision),
            )
            .filter(schedule::Column::ScheduleId.eq(schedule_id))
            .filter(
                schedule::Column::OwnerUserId
                    .eq(group.actor_id.parse::<i32>().map_err(|_| invalid())?),
            )
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) async fn settle_on(
    txn: &DatabaseTransaction,
    group: &group_row::Model,
    row: &cost::Model,
    reservation: &DelegationCallReservation,
    actual: Option<Usage>,
) -> Result<(), DbErr> {
    let allocation = linked_on(txn, group, row, reservation).await?;
    if let (Some(allocation), Some(actual)) = (allocation, actual) {
        let receipt = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    "delegated-model-settlement",
                    &reservation.reservation_id,
                    &row.provider_receipt_kind,
                    &row.provider_receipt_id,
                    row.provider_started_at_ms,
                    actual,
                ))
                .map_err(|_| invalid())?
            )
        );
        crate::schedule_store::ScheduleStore::settle_task_model_budget(
            txn,
            allocation.owner_user_id,
            &allocation.run_id,
            &allocation.reservation_id,
            actual.tokens,
            &receipt,
        )
        .await
        .map_err(|_| invalid())?;
    }
    Ok(())
}
