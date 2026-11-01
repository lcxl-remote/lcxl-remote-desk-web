//! Shared GC discovery, durable scan hints and final quota cursor writes.
//! Hosts retain the collector transaction, native reads and accounting locks.

use crate::{AssistantStoreError, entity::agent_assistant_quota as quota, signed};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, ExprTrait, Order, QueryFilter,
    QueryOrder, QuerySelect,
    sea_query::{Condition, Expr, NullOrdering},
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutputGcOutcome {
    pub scanned: u64,
    pub removed: u64,
    pub held: u64,
    pub invalid: u64,
    pub refunded_metadata_bytes: i64,
    pub cycle_finished: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutputGcSweep {
    pub claimed_actors: u64,
    pub failed_actors: u64,
    pub scanned: u64,
    pub removed: u64,
    pub held: u64,
    pub invalid: u64,
    pub refunded_metadata_bytes: i64,
}

impl OutputGcSweep {
    pub fn include(&mut self, outcome: OutputGcOutcome) -> Result<(), AssistantStoreError> {
        self.scanned += outcome.scanned;
        self.removed += outcome.removed;
        self.held += outcome.held;
        self.invalid += outcome.invalid;
        self.refunded_metadata_bytes = self
            .refunded_metadata_bytes
            .checked_add(outcome.refunded_metadata_bytes)
            .ok_or(AssistantStoreError::Capacity)?;
        Ok(())
    }
}

/// Discovery and cooldown claims do not grant source access or deletion authority.
pub fn due_actors_on<'a, C: ConnectionTrait>(
    db: &'a C,
    actor: Option<&'a str>,
    now: i64,
    interval_ms: u64,
    maximum: u32,
) -> impl std::future::Future<Output = Result<Vec<(i64, String, Option<i64>)>, AssistantStoreError>>
+ Send
+ 'a {
    desk_diagnose_core::future::send_phase(|| {
        let future = async move {
            let horizon = now
                .checked_add(signed(interval_ms)?)
                .ok_or(AssistantStoreError::Capacity)?;
            let mut query = quota::Entity::find()
                .filter(quota::Column::Id.gt(0))
                .filter(quota::Column::Revision.gte(0))
                .filter(quota::Column::Revision.lt(i64::MAX))
                .filter(
                    Condition::any()
                        .add(quota::Column::OutputGcNextScanAtMs.is_null())
                        .add(quota::Column::OutputGcNextScanAtMs.lte(now))
                        .add(quota::Column::OutputGcNextScanAtMs.gt(horizon)),
                );
            if let Some(actor) = actor {
                query = query.filter(quota::Column::ActorId.eq(actor));
            }
            Ok(query
                .select_only()
                .column(quota::Column::Id)
                .column(quota::Column::ActorId)
                .column(quota::Column::OutputGcNextScanAtMs)
                .order_by_with_nulls(
                    quota::Column::OutputGcNextScanAtMs,
                    Order::Asc,
                    NullOrdering::First,
                )
                .order_by_asc(quota::Column::Id)
                .limit(u64::from(maximum))
                .into_tuple::<(i64, String, Option<i64>)>()
                .all(db)
                .await?)
        };
        #[cfg(test)]
        assert!(std::mem::size_of_val(&future) <= 16 * 1024);
        future
    })
}

pub fn claim_on<'a, C: ConnectionTrait>(
    db: &'a C,
    id: i64,
    actor: &'a str,
    previous: Option<i64>,
    now: i64,
    interval_ms: u64,
) -> impl std::future::Future<Output = Result<bool, AssistantStoreError>> + Send + 'a {
    desk_diagnose_core::future::send_phase(|| {
        let future = async move {
            let next = now
                .checked_add(signed(interval_ms)?)
                .filter(|_| now > 0)
                .ok_or(AssistantStoreError::Capacity)?;
            if previous.is_some_and(|value| value > now && value <= next) {
                return Ok(false);
            }
            let mut update = quota::Entity::update_many()
                .col_expr(quota::Column::OutputGcNextScanAtMs, Expr::value(Some(next)))
                .col_expr(
                    quota::Column::Revision,
                    Expr::col(quota::Column::Revision).add(1_i64),
                )
                .filter(quota::Column::Id.eq(id))
                .filter(quota::Column::ActorId.eq(actor))
                .filter(quota::Column::Revision.gte(0))
                .filter(quota::Column::Revision.lt(i64::MAX));
            update = match previous {
                Some(value) => update.filter(quota::Column::OutputGcNextScanAtMs.eq(value)),
                None => update.filter(quota::Column::OutputGcNextScanAtMs.is_null()),
            };
            Ok(update.exec(db).await?.rows_affected == 1)
        };
        #[cfg(test)]
        assert!(std::mem::size_of_val(&future) <= 16 * 1024);
        future
    })
}

/// Raw scan fields captured before collection; unrelated quota fields stay current.
#[derive(Debug, Clone, Copy)]
pub struct GcCursor<'a> {
    pub id: i64,
    pub actor_id: &'a str,
    pub output: (Option<i64>, Option<i64>),
}

pub fn finish_quota_on<'a, C: ConnectionTrait>(
    db: &'a C,
    original: GcCursor<'a>,
    next: Option<(i64, i64)>,

    refund: i64,
) -> impl std::future::Future<Output = Result<(), AssistantStoreError>> + Send + 'a {
    desk_diagnose_core::future::send_phase(|| {
        let future = async move {
            if refund == 0
                && next.map(|(at, _)| at) == original.output.0
                && next.map(|(_, id)| id) == original.output.1
            {
                return Ok(());
            }
            let mut query =
                quota::Entity::find().filter(quota::Column::ActorId.eq(original.actor_id));
            if db.get_database_backend() == DatabaseBackend::Postgres {
                query = query.lock_exclusive();
            }
            let current = query
                .one(db)
                .await?
                .ok_or(AssistantStoreError::Inaccessible)?;
            if current.id != original.id
                || current.output_gc_completed_at_ms != original.output.0
                || current.output_gc_archive_id != original.output.1
                || refund < 0
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            let bytes = current
                .metadata_bytes
                .checked_sub(refund)
                .filter(|value| *value >= 8192)
                .ok_or(AssistantStoreError::Capacity)?;
            let mut update = quota::Entity::update_many()
                .col_expr(quota::Column::MetadataBytes, Expr::value(bytes))
                .col_expr(
                    quota::Column::OutputGcCompletedAtMs,
                    Expr::value(next.map(|(at, _)| at)),
                )
                .col_expr(
                    quota::Column::OutputGcArchiveId,
                    Expr::value(next.map(|(_, id)| id)),
                )
                .col_expr(
                    quota::Column::Revision,
                    Expr::value(crate::increment(current.revision)?),
                )
                .filter(quota::Column::Id.eq(current.id))
                .filter(quota::Column::ActorId.eq(&current.actor_id))
                .filter(quota::Column::Revision.eq(current.revision))
                .filter(quota::Column::MetadataBytes.eq(current.metadata_bytes));
            update = match current.output_gc_completed_at_ms {
                Some(value) => update.filter(quota::Column::OutputGcCompletedAtMs.eq(value)),
                None => update.filter(quota::Column::OutputGcCompletedAtMs.is_null()),
            };
            update = match current.output_gc_archive_id {
                Some(value) => update.filter(quota::Column::OutputGcArchiveId.eq(value)),
                None => update.filter(quota::Column::OutputGcArchiveId.is_null()),
            };
            if update.exec(db).await?.rows_affected != 1 {
                return Err(AssistantStoreError::Inaccessible);
            }
            Ok(())
        };
        #[cfg(test)]
        assert!(std::mem::size_of_val(&future) <= 16 * 1024);
        future
    })
}

/// Local wake-up bookkeeping only; account eligibility remains in shared storage.
#[derive(Debug)]
pub struct CleanupCadence {
    retention_interval: std::time::Duration,
    last_retention: Option<std::time::Instant>,
}

impl CleanupCadence {
    pub fn new(retention_interval: std::time::Duration) -> Self {
        Self {
            retention_interval,
            last_retention: None,
        }
    }

    pub fn take_retention(&mut self, now: std::time::Instant) -> bool {
        if self
            .last_retention
            .is_none_or(|last| now.saturating_duration_since(last) >= self.retention_interval)
        {
            self.last_retention = Some(now);
            true
        } else {
            false
        }
    }

    pub fn next_delay(
        &self,
        now: std::time::Instant,
        gc_interval: std::time::Duration,
    ) -> std::time::Duration {
        let retention_remaining = self
            .last_retention
            .map_or(std::time::Duration::ZERO, |last| {
                self.retention_interval
                    .saturating_sub(now.saturating_duration_since(last))
            });
        gc_interval.min(retention_remaining)
    }
}

#[cfg(test)]
mod tests;
