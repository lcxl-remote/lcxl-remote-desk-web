//! Atomic capacity reservations share the projection transaction.

use desk_diagnose_core::model_observability::capacity::{StorageKind, data_budget};

use super::*;
use crate::config::ConfigConnection;

pub(super) fn valid(row: &settings::Model) -> bool {
    [
        row.event_rows,
        row.compact_rows,
        row.detail_rows,
        row.rollup_rows,
        row.storage_used_bytes,
        row.trimmed_details,
        row.dropped_pending_events,
    ]
    .into_iter()
    .all(|value| value >= 0)
}

pub(super) async fn resize(
    txn: &DatabaseTransaction,
    config: &MetricsSettings,
    kind: StorageKind,
    rows: i64,
    bytes: i64,
) -> Result<bool, DbErr> {
    let (column, limit) = match kind {
        StorageKind::Event => (
            settings::Column::EventRows,
            i64::from(config.event_row_budget),
        ),
        StorageKind::Compact => (
            settings::Column::CompactRows,
            i64::from(config.compact_row_budget),
        ),
        StorageKind::Detail => (
            settings::Column::DetailRows,
            i64::from(config.detail_row_budget),
        ),
        StorageKind::Rollup => (
            settings::Column::RollupRows,
            i64::from(config.rollup_row_budget),
        ),
    };
    let row_release = rows
        .min(0)
        .checked_neg()
        .ok_or_else(|| DbErr::Custom("metrics row accounting unavailable".into()))?;
    let byte_release = bytes
        .min(0)
        .checked_neg()
        .ok_or_else(|| DbErr::Custom("metrics byte accounting unavailable".into()))?;
    let byte_limit = data_budget(&config.storage_budget_bytes)
        .ok_or_else(|| DbErr::Custom("metrics byte budget unavailable".into()))?;
    let mut selection = settings::Entity::update_many()
        .col_expr(column, sea_orm::ExprTrait::add(Expr::col(column), rows))
        .col_expr(
            settings::Column::StorageUsedBytes,
            sea_orm::ExprTrait::add(Expr::col(settings::Column::StorageUsedBytes), bytes),
        )
        .filter(settings::Column::Id.eq(1))
        .filter(column.gte(row_release))
        .filter(settings::Column::StorageUsedBytes.gte(byte_release));
    if rows > 0 {
        selection = selection.filter(column.lte(limit.saturating_sub(rows)));
    }
    if bytes > 0 {
        selection = selection
            .filter(settings::Column::StorageUsedBytes.lte(byte_limit.saturating_sub(bytes)));
    }
    Ok(selection.exec(txn).await?.rows_affected == 1)
}

pub(super) async fn release(
    txn: &DatabaseTransaction,
    config: &MetricsSettings,
    kind: StorageKind,
    rows: usize,
    bytes: i64,
) -> Result<(), DbErr> {
    if bytes < 0 {
        return Err(DbErr::Custom("metrics accounting unavailable".into()));
    }
    let rows = i64::try_from(rows)
        .map_err(|_| DbErr::Custom("metrics row accounting unavailable".into()))?;
    if !resize(txn, config, kind, -rows, -bytes).await? {
        return Err(DbErr::Custom("metrics capacity accounting conflict".into()));
    }
    Ok(())
}

pub(super) fn bytes(mut values: impl Iterator<Item = i64>) -> Result<i64, DbErr> {
    values
        .try_fold(0i64, |total, value| {
            if value < 0 {
                None
            } else {
                total.checked_add(value)
            }
        })
        .ok_or_else(|| DbErr::Custom("metrics byte accounting unavailable".into()))
}

pub(super) fn charge(columns: &[&str]) -> Result<i64, DbErr> {
    charged_bytes(columns)
        .ok_or_else(|| DbErr::Custom("metrics byte accounting unavailable".into()))
}

pub(super) async fn configuration(
    txn: &DatabaseTransaction,
) -> Result<(settings::Model, MetricsSettings), DbErr> {
    // Lock the shared resource row before touching event or projection rows.
    // A changed budget takes effect for all subsequent reservations.
    let locked = settings::Entity::update_many()
        .col_expr(
            settings::Column::StorageUsedBytes,
            sea_orm::ExprTrait::add(Expr::col(settings::Column::StorageUsedBytes), 0),
        )
        .filter(settings::Column::Id.eq(1))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(DbErr::Custom("metrics settings unavailable".into()));
    }
    let row = settings::Entity::find_by_id(1)
        .one(txn)
        .await?
        .ok_or_else(|| DbErr::Custom("metrics settings unavailable".into()))?;
    if row.schema_version != EVENT_SCHEMA_VERSION as i32 || !valid(&row) {
        return Err(DbErr::Custom("metrics schema unavailable".into()));
    }
    let config = txn.config_read().await.model_metrics.clone();
    config
        .validate()
        .map_err(|_| DbErr::Custom("metrics settings unavailable".into()))?;
    Ok((row, config))
}

pub(super) async fn partial(txn: &DatabaseTransaction) -> Result<(), DbErr> {
    settings::Entity::update_many()
        .col_expr(settings::Column::CoveragePartial, Expr::value(true))
        .filter(settings::Column::Id.eq(1))
        .exec(txn)
        .await?;
    Ok(())
}
