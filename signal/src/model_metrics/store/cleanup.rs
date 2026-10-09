//! Bounded retention releases exactly the resources deleted in this transaction.

use super::*;

fn active(previous: bool, used: i64, limit: i64) -> bool {
    used >= limit.saturating_mul(9) / 10 || (previous && used > low_water(limit))
}

async fn pressure(
    txn: &DatabaseTransaction,
    config: &MetricsSettings,
    budget: i64,
) -> Result<settings::Model, DbErr> {
    let (mut row, _) = resources::configuration(txn).await?;
    row.event_cleanup_active = active(
        row.event_cleanup_active,
        row.event_rows,
        i64::from(config.event_row_budget),
    );
    row.compact_cleanup_active = active(
        row.compact_cleanup_active,
        row.compact_rows,
        i64::from(config.compact_row_budget),
    );
    row.detail_cleanup_active = active(
        row.detail_cleanup_active,
        row.detail_rows,
        i64::from(config.detail_row_budget),
    );
    row.rollup_cleanup_active = active(
        row.rollup_cleanup_active,
        row.rollup_rows,
        i64::from(config.rollup_row_budget),
    );
    row.storage_cleanup_active = active(row.storage_cleanup_active, row.storage_used_bytes, budget);
    settings::Entity::update_many()
        .set(settings::ActiveModel {
            event_cleanup_active: Set(row.event_cleanup_active),
            compact_cleanup_active: Set(row.compact_cleanup_active),
            detail_cleanup_active: Set(row.detail_cleanup_active),
            rollup_cleanup_active: Set(row.rollup_cleanup_active),
            storage_cleanup_active: Set(row.storage_cleanup_active),
            ..Default::default()
        })
        .filter(settings::Column::Id.eq(1))
        .exec(txn)
        .await?;
    Ok(row)
}

fn batch(rows: i64, limit: u32, storage: bool) -> u64 {
    if storage {
        CLEANUP_BATCH_ROWS
    } else {
        u64::try_from(rows.saturating_sub(low_water(i64::from(limit))).max(1))
            .unwrap_or(CLEANUP_BATCH_ROWS)
            .min(CLEANUP_BATCH_ROWS)
    }
}

impl Store {
    pub async fn cleanup(&self, now: i64) -> Result<(), DbErr> {
        let txn = self.db.begin().await?;
        if !self.claim(&txn, now).await? {
            txn.rollback().await?;
            return Ok(());
        }
        let (_, config) = resources::configuration(&txn).await?;
        let budget = data_budget(&config.storage_budget_bytes)
            .ok_or_else(|| DbErr::Custom("metrics capacity unavailable".into()))?;

        let row = pressure(&txn, &config, budget).await?;

        // Disposable successful details and already-applied events are released
        // before shortening the correction window or discarding pending facts.
        let under_pressure = row.detail_cleanup_active || row.storage_cleanup_active;
        let mut selection = detail::Entity::find();
        if !under_pressure {
            let cutoff = now.saturating_sub(i64::from(config.detail_days) * DAY);
            selection = selection.filter(
                Condition::any()
                    .add(detail::Column::StartedAtMs.lt(cutoff))
                    .add(
                        Condition::all()
                            .add(detail::Column::Kind.eq("unassociated"))
                            .add(detail::Column::ReceivedAtMs.lt(cutoff)),
                    ),
            );
        }
        let details = selection
            .order_by_asc(detail::Column::RetentionPriority)
            .order_by_asc(detail::Column::StartedAtMs)
            .order_by_asc(detail::Column::ObjectId)
            .limit(if under_pressure {
                batch(
                    row.detail_rows,
                    config.detail_row_budget,
                    row.storage_cleanup_active,
                )
            } else {
                CLEANUP_BATCH_ROWS
            })
            .all(&txn)
            .await?;
        if !details.is_empty() {
            let bytes = resources::bytes(details.iter().map(|row| row.storage_bytes))?;
            let ids: Vec<_> = details.iter().map(|row| row.object_id.clone()).collect();
            compact::Entity::update_many()
                .col_expr(compact::Column::DetailTrimmed, Expr::value(true))
                .filter(compact::Column::ObjectId.is_in(ids.clone()))
                .exec(&txn)
                .await?;
            let removed = detail::Entity::delete_many()
                .filter(detail::Column::ObjectId.is_in(ids))
                .exec(&txn)
                .await?;
            if removed.rows_affected != details.len() as u64 {
                return Err(DbErr::Custom("metrics cleanup conflict".into()));
            }
            resources::release(&txn, &config, StorageKind::Detail, details.len(), bytes).await?;
            settings::Entity::update_many()
                .col_expr(
                    settings::Column::TrimmedDetails,
                    sea_orm::ExprTrait::add(
                        Expr::col(settings::Column::TrimmedDetails),
                        details.len() as i64,
                    ),
                )
                .filter(settings::Column::Id.eq(1))
                .exec(&txn)
                .await?;
        }

        let row = pressure(&txn, &config, budget).await?;
        let under_pressure = row.event_cleanup_active || row.storage_cleanup_active;
        let mut selection = event::Entity::find()
            .filter(event::Column::Applied.eq(true))
            .filter(event::Column::AssociationPending.eq(false));
        if !under_pressure {
            selection = selection.filter(event::Column::ReceivedAtMs.lt(now.saturating_sub(DAY)));
        }
        let applied = selection
            .order_by_asc(event::Column::Id)
            .limit(if under_pressure {
                batch(
                    row.event_rows,
                    config.event_row_budget,
                    row.storage_cleanup_active,
                )
            } else {
                CLEANUP_BATCH_ROWS
            })
            .all(&txn)
            .await?;
        remove_events(&txn, &config, &applied).await?;

        let row = pressure(&txn, &config, budget).await?;
        let under_pressure = row.event_cleanup_active || row.storage_cleanup_active;
        let eligible = if under_pressure {
            Condition::all()
                .add(
                    Condition::any()
                        .add(event::Column::Applied.eq(false))
                        .add(event::Column::AssociationPending.eq(true)),
                )
                .add(event::Column::ReceivedAtMs.lt(now.saturating_sub(30_000)))
        } else {
            Condition::any()
                .add(
                    Condition::all()
                        .add(event::Column::Applied.eq(false))
                        .add(event::Column::ReceivedAtMs.lt(now.saturating_sub(DAY))),
                )
                .add(
                    Condition::all()
                        .add(event::Column::AssociationPending.eq(true))
                        .add(
                            event::Column::ReceivedAtMs
                                .lt(now.saturating_sub(i64::from(config.mutable_days) * DAY)),
                        ),
                )
        };
        let pending = event::Entity::find()
            .filter(eligible)
            .order_by_asc(event::Column::Id)
            .limit(if under_pressure {
                batch(
                    row.event_rows,
                    config.event_row_budget,
                    row.storage_cleanup_active,
                )
            } else {
                CLEANUP_BATCH_ROWS
            })
            .all(&txn)
            .await?;
        if !pending.is_empty() {
            for fact in pending.iter().filter(|fact| fact.association_pending) {
                let state = if fact.received_at_ms
                    < now.saturating_sub(i64::from(config.mutable_days) * DAY)
                {
                    AssociationGapState::OutsideWindow
                } else {
                    AssociationGapState::Unavailable
                };
                self.end_unassociated(&txn, fact, now, state).await?;
            }
            remove_events(&txn, &config, &pending).await?;
            settings::Entity::update_many()
                .col_expr(
                    settings::Column::DroppedPendingEvents,
                    sea_orm::ExprTrait::add(
                        Expr::col(settings::Column::DroppedPendingEvents),
                        pending.len() as i64,
                    ),
                )
                .col_expr(settings::Column::CoveragePartial, Expr::value(true))
                .filter(settings::Column::Id.eq(1))
                .exec(&txn)
                .await?;
        }

        let row = pressure(&txn, &config, budget).await?;
        let under_pressure = row.compact_cleanup_active || row.storage_cleanup_active;
        let mut freeze = row.frozen_before_ms.max(
            now.saturating_sub(i64::from(config.mutable_days) * DAY)
                .div_euclid(HOUR)
                * HOUR,
        );
        if under_pressure {
            let oldest = compact::Entity::find()
                .order_by_asc(compact::Column::StartedAtMs)
                .order_by_asc(compact::Column::ObjectId)
                .limit(batch(
                    row.compact_rows,
                    config.compact_row_budget,
                    row.storage_cleanup_active,
                ))
                .all(&txn)
                .await?;
            if let Some(last) = oldest.last() {
                freeze = freeze.max(
                    last.started_at_ms
                        .div_euclid(HOUR)
                        .saturating_add(1)
                        .saturating_mul(HOUR),
                );
            }
        }
        // Freeze the whole cohort before removing its deduplication state.
        // The shared boundary is authoritative even while per-row marks catch up.
        settings::Entity::update_many()
            .col_expr(settings::Column::FrozenBeforeMs, Expr::value(freeze))
            .col_expr(
                settings::Column::CoveragePartial,
                Expr::value(row.coverage_partial || under_pressure),
            )
            .filter(settings::Column::Id.eq(1))
            .exec(&txn)
            .await?;
        let frozen = rollup::Entity::find()
            .filter(rollup::Column::BucketMs.lt(freeze))
            .filter(rollup::Column::Frozen.eq(false))
            .order_by_asc(rollup::Column::BucketMs)
            .order_by_asc(rollup::Column::Id)
            .limit(CLEANUP_BATCH_ROWS)
            .all(&txn)
            .await?;
        if !frozen.is_empty() {
            rollup::Entity::update_many()
                .col_expr(rollup::Column::Frozen, Expr::value(true))
                .filter(rollup::Column::Id.is_in(frozen.into_iter().map(|row| row.id)))
                .exec(&txn)
                .await?;
        }
        let compact = compact::Entity::find()
            .filter(compact::Column::StartedAtMs.lt(freeze))
            .order_by_asc(compact::Column::StartedAtMs)
            .order_by_asc(compact::Column::ObjectId)
            .limit(CLEANUP_BATCH_ROWS)
            .all(&txn)
            .await?;
        if !compact.is_empty() {
            let bytes = resources::bytes(compact.iter().map(|row| row.storage_bytes))?;
            let removed = compact::Entity::delete_many()
                .filter(
                    compact::Column::ObjectId
                        .is_in(compact.iter().map(|row| row.object_id.clone())),
                )
                .exec(&txn)
                .await?;
            if removed.rows_affected != compact.len() as u64 {
                return Err(DbErr::Custom("metrics cleanup conflict".into()));
            }
            resources::release(&txn, &config, StorageKind::Compact, compact.len(), bytes).await?;
        }

        let row = pressure(&txn, &config, budget).await?;
        let under_pressure = row.rollup_cleanup_active || row.storage_cleanup_active;
        let mut selection = rollup::Entity::find();
        if !under_pressure {
            selection = selection.filter(
                Condition::any()
                    .add(
                        Condition::all()
                            .add(rollup::Column::GranularityMs.eq(300_000))
                            .add(
                                rollup::Column::BucketMs
                                    .lt(now
                                        .saturating_sub(i64::from(config.five_minute_days) * DAY)),
                            ),
                    )
                    .add(
                        Condition::all()
                            .add(rollup::Column::GranularityMs.eq(HOUR))
                            .add(
                                rollup::Column::BucketMs
                                    .lt(now.saturating_sub(i64::from(config.hourly_days) * DAY)),
                            ),
                    ),
            );
        }
        let rollups = selection
            .order_by_asc(rollup::Column::BucketMs)
            .order_by_asc(rollup::Column::Id)
            .limit(if under_pressure {
                batch(
                    row.rollup_rows,
                    config.rollup_row_budget,
                    row.storage_cleanup_active,
                )
            } else {
                CLEANUP_BATCH_ROWS
            })
            .all(&txn)
            .await?;
        if !rollups.is_empty() {
            if under_pressure {
                let boundary = rollups
                    .last()
                    .unwrap()
                    .bucket_ms
                    .div_euclid(HOUR)
                    .saturating_add(1)
                    .saturating_mul(HOUR);
                settings::Entity::update_many()
                    .col_expr(
                        settings::Column::RollupTrimBeforeMs,
                        Expr::value(row.rollup_trim_before_ms.max(boundary)),
                    )
                    .col_expr(
                        settings::Column::FrozenBeforeMs,
                        Expr::value(freeze.max(boundary)),
                    )
                    .col_expr(settings::Column::CoveragePartial, Expr::value(true))
                    .filter(settings::Column::Id.eq(1))
                    .exec(&txn)
                    .await?;
            }
            let bytes = resources::bytes(rollups.iter().map(|row| row.storage_bytes))?;
            let removed = rollup::Entity::delete_many()
                .filter(rollup::Column::Id.is_in(rollups.iter().map(|row| row.id)))
                .exec(&txn)
                .await?;
            if removed.rows_affected != rollups.len() as u64 {
                return Err(DbErr::Custom("metrics cleanup conflict".into()));
            }
            resources::release(&txn, &config, StorageKind::Rollup, rollups.len(), bytes).await?;
        }
        let old_nodes = health::Entity::find()
            .filter(health::Column::ReportedAtMs.lt(now.saturating_sub(7 * DAY)))
            .order_by_asc(health::Column::ReportedAtMs)
            .limit(CLEANUP_BATCH_ROWS)
            .all(&txn)
            .await?;
        if !old_nodes.is_empty() {
            health::Entity::delete_many()
                .filter(health::Column::NodeId.is_in(old_nodes.into_iter().map(|row| row.node_id)))
                .exec(&txn)
                .await?;
        }
        pressure(&txn, &config, budget).await?;
        txn.commit().await?;
        Ok(())
    }
}

async fn remove_events(
    txn: &DatabaseTransaction,
    config: &MetricsSettings,
    rows: &[event::Model],
) -> Result<(), DbErr> {
    if rows.is_empty() {
        return Ok(());
    }
    let bytes = resources::bytes(rows.iter().map(|row| row.storage_bytes))?;
    let removed = event::Entity::delete_many()
        .filter(event::Column::Id.is_in(rows.iter().map(|row| row.id)))
        .exec(txn)
        .await?;
    if removed.rows_affected != rows.len() as u64 {
        return Err(DbErr::Custom("metrics cleanup conflict".into()));
    }
    resources::release(txn, config, StorageKind::Event, rows.len(), bytes).await
}
