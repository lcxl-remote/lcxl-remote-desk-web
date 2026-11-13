//! Calendar pump scans. Queuing is not authorization to execute a model or tool.
//! Hosts own the timer and logging; this module runs one bounded round.
use super::{
    ScheduleStoreError, database_now, due_candidates, expired_continuation_candidates,
    expired_pending, late_receipt_candidates, wait_expiry_candidates,
};
use crate::entity::agent_schedule_run as run;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

const BATCH_SIZE: u64 = 32;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScheduleScanReport {
    pub scanned: usize,
    pub materialized: usize,
    pub expired: usize,
    pub recovered: usize,
    pub deferred: usize,
    pub failed: usize,
    pub next_cursor: Option<i64>,
}

/// Per-row operations of the calendar pump. Each host forwards to its own
/// transactional store; candidate queries run on `connection`.
#[async_trait::async_trait]
pub trait ScheduleScanOps: Sync {
    type Conn: ConnectionTrait;
    fn connection(&self) -> &Self::Conn;
    async fn propagate_task_cancellation(&self, run_id: &str) -> Result<usize, ScheduleStoreError>;
    async fn reconcile_late_task_receipts(&self, run_id: &str) -> Result<bool, ScheduleStoreError>;
    async fn recover_action_free_fresh_task(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError>;
    async fn settle_fresh_children_wait(&self, run_id: &str) -> Result<bool, ScheduleStoreError>;
    async fn cancel_continuation_wait(&self, run_id: &str) -> Result<bool, ScheduleStoreError>;
    async fn expire_fresh_approval_wait(&self, run_id: &str) -> Result<bool, ScheduleStoreError>;
    async fn recover_committed_continuation(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<Option<run::Model>, ScheduleStoreError>;
    async fn materialize_due(
        &self,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<Option<run::Model>, ScheduleStoreError>;
    async fn expire_pending(&self, run_id: &str) -> Result<run::Model, ScheduleStoreError>;
}

pub async fn scan_action_cancellations_once<S: ScheduleScanOps>(
    store: &S,
    after_id: i64,
) -> Result<ScheduleScanReport, ScheduleStoreError> {
    let candidates = action_cancel_candidates(store.connection(), after_id, BATCH_SIZE).await?;
    let mut report = ScheduleScanReport {
        scanned: candidates.len(),
        next_cursor: (candidates.len() == BATCH_SIZE as usize)
            .then(|| candidates.last().unwrap().id),
        ..Default::default()
    };
    for candidate in candidates {
        match store.propagate_task_cancellation(&candidate.run_id).await {
            Ok(count) if count > 0 => report.recovered += count,
            Ok(_) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                report.deferred += 1
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

pub async fn scan_late_receipts_once<S: ScheduleScanOps>(
    store: &S,
    after_id: i64,
) -> Result<ScheduleScanReport, ScheduleStoreError> {
    let candidates = late_receipt_candidates(store.connection(), after_id, BATCH_SIZE).await?;
    let mut report = ScheduleScanReport {
        scanned: candidates.len(),
        next_cursor: (candidates.len() == BATCH_SIZE as usize)
            .then(|| candidates.last().unwrap().id),
        ..Default::default()
    };
    for candidate in candidates {
        match store.reconcile_late_task_receipts(&candidate.run_id).await {
            Ok(true) => report.recovered += 1,
            Ok(false) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                report.deferred += 1
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

pub async fn scan_fresh_recovery_once<S: ScheduleScanOps>(
    store: &S,
    after_id: i64,
) -> Result<ScheduleScanReport, ScheduleStoreError> {
    let candidates = expired_fresh_candidates(store.connection(), after_id, BATCH_SIZE).await?;
    let mut report = ScheduleScanReport {
        scanned: candidates.len(),
        next_cursor: (candidates.len() == BATCH_SIZE as usize)
            .then(|| candidates.last().unwrap().id),
        ..Default::default()
    };
    for candidate in candidates {
        match store
            .recover_action_free_fresh_task(&candidate.run_id)
            .await
        {
            Ok(true) => report.recovered += 1,
            Ok(false) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                report.deferred += 1
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

pub fn scan_children_wait_settlement_once<'a, S: ScheduleScanOps>(
    store: &'a S,
    after_id: i64,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<ScheduleScanReport, ScheduleStoreError>>
            + Send
            + 'a,
    >,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let candidates = children_wait_candidates(store.connection(), after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match store.settle_fresh_children_wait(&candidate.run_id).await {
                Ok(true) => report.expired += 1,
                Ok(false) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                    report.deferred += 1
                }
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    })
}

pub fn scan_wait_expiry_once<'a, S: ScheduleScanOps>(
    store: &'a S,
    after_id: i64,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<ScheduleScanReport, ScheduleStoreError>>
            + Send
            + 'a,
    >,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let candidates = wait_expiry_candidates(store.connection(), after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            let result = if candidate.cancel_requested_at.is_some() {
                match desk_diagnose_core::future::send_phase(|| {
                    store.cancel_continuation_wait(&candidate.run_id)
                })
                .await
                {
                    Ok(true) => Ok(true),
                    Ok(false)
                    | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                        desk_diagnose_core::future::send_phase(|| {
                            store.expire_fresh_approval_wait(&candidate.run_id)
                        })
                        .await
                    }
                    Err(error) => Err(error),
                }
            } else {
                desk_diagnose_core::future::send_phase(|| {
                    store.expire_fresh_approval_wait(&candidate.run_id)
                })
                .await
            };
            match result {
                Ok(true) => report.expired += 1,
                Ok(false) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                    report.deferred += 1
                }
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    })
}

/// Reconcile durable results and action-free interrupted turns. Unfinished
/// actions remain for original-result reconciliation without another dispatch.
pub async fn scan_committed_recovery_once<S: ScheduleScanOps>(
    store: &S,
    after_id: i64,
) -> Result<ScheduleScanReport, ScheduleStoreError> {
    let candidates =
        expired_continuation_candidates(store.connection(), after_id, BATCH_SIZE).await?;
    let mut report = ScheduleScanReport {
        scanned: candidates.len(),
        next_cursor: (candidates.len() == BATCH_SIZE as usize)
            .then(|| candidates.last().unwrap().id),
        ..Default::default()
    };
    for candidate in candidates {
        match store
            .recover_committed_continuation(candidate.owner_user_id, &candidate.run_id)
            .await
        {
            Ok(Some(_)) => report.recovered += 1,
            Ok(None) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                report.deferred += 1
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

/// Keyset pagination prevents one malformed or contended task from starving
/// later owners. Each occurrence still passes the database CAS and unique key.
pub async fn scan_calendar_once<S: ScheduleScanOps>(
    store: &S,
    after_id: i64,
) -> Result<ScheduleScanReport, ScheduleStoreError> {
    let candidates = due_candidates(store.connection(), after_id, BATCH_SIZE).await?;
    let mut report = ScheduleScanReport {
        scanned: candidates.len(),
        next_cursor: (candidates.len() == BATCH_SIZE as usize)
            .then(|| candidates.last().unwrap().id),
        ..Default::default()
    };
    for candidate in candidates {
        match store
            .materialize_due(&candidate.schedule_id, candidate.revision)
            .await
        {
            Ok(Some(_)) => report.materialized += 1,
            Ok(None) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                report.deferred += 1;
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

/// Release only work that never acquired an execution lease. Running work
/// needs outcome reconciliation and must never be recycled by a timer.
pub async fn scan_pending_expiry_once<S: ScheduleScanOps>(
    store: &S,
    after_id: i64,
) -> Result<ScheduleScanReport, ScheduleStoreError> {
    let candidates = expired_pending(store.connection(), after_id, BATCH_SIZE).await?;
    let mut report = ScheduleScanReport {
        scanned: candidates.len(),
        next_cursor: (candidates.len() == BATCH_SIZE as usize)
            .then(|| candidates.last().unwrap().id),
        ..Default::default()
    };
    for candidate in candidates {
        match store.expire_pending(&candidate.run_id).await {
            Ok(_) => report.expired += 1,
            Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                report.deferred += 1
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

async fn action_cancel_candidates<C: ConnectionTrait>(
    db: &C,
    after: i64,
    limit: u64,
) -> Result<Vec<run::Model>, ScheduleStoreError> {
    Ok(run::Entity::find()
        .filter(run::Column::Id.gt(after))
        .filter(run::Column::CancelRequestedAt.is_not_null())
        .order_by_asc(run::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

async fn expired_fresh_candidates<C: ConnectionTrait>(
    db: &C,
    after: i64,
    limit: u64,
) -> Result<Vec<run::Model>, ScheduleStoreError> {
    let now = database_now(db).await?;
    Ok(run::Entity::find()
        .filter(run::Column::Id.gt(after))
        .filter(run::Column::Status.eq("running"))
        .filter(run::Column::LeaseDeadline.lte(now))
        .filter(run::Column::FailureAccounted.eq(false))
        .filter(run::Column::FinishedAt.is_null())
        .order_by_asc(run::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

async fn children_wait_candidates<C: ConnectionTrait>(
    db: &C,
    after: i64,
    limit: u64,
) -> Result<Vec<run::Model>, ScheduleStoreError> {
    if after < 0 || !(1..=32).contains(&limit) {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok(run::Entity::find()
        .filter(run::Column::Id.gt(after))
        .filter(run::Column::Status.eq("awaiting_children"))
        .filter(run::Column::FailureAccounted.eq(false))
        .filter(run::Column::FinishedAt.is_null())
        .order_by_asc(run::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// Node-local scan positions. Only these cursors are volatile; task calendars
/// and catch-up decisions are durable.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanCursors {
    pub action_cancel: i64,
    pub late_receipt: i64,
    pub fresh_recovery: i64,
    pub children_wait: i64,
    pub wait_expiry: i64,
    pub committed_recovery: i64,
    pub pending_expiry: i64,
    pub calendar: i64,
}

/// A warning from one pump round, rendered with the host log prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanWarning {
    /// Rows of a scan that failed with a non-retryable error.
    Failed { count: usize, what: &'static str },
    /// The scan itself failed; its cursor restarts from the beginning.
    Unavailable(&'static str),
}

impl std::fmt::Display for ScanWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { count, what } => write!(f, "[schedule] {count} {what}"),
            Self::Unavailable(what) => write!(f, "[schedule] {what}; retrying"),
        }
    }
}

fn advance(
    cursor: &mut i64,
    result: Result<ScheduleScanReport, ScheduleStoreError>,
    failed: &'static str,
    unavailable: &'static str,
    warnings: &mut Vec<ScanWarning>,
) {
    match result {
        Ok(report) => {
            *cursor = report.next_cursor.unwrap_or(0);
            if report.failed > 0 {
                warnings.push(ScanWarning::Failed {
                    count: report.failed,
                    what: failed,
                });
            }
        }
        Err(_) => {
            *cursor = 0;
            warnings.push(ScanWarning::Unavailable(unavailable));
        }
    }
}

/// One pump round. Manager nodes may all scan: database fences, not this
/// round, decide which node creates or settles each occurrence.
pub async fn scan_tick<S: ScheduleScanOps>(
    store: &S,
    cursors: &mut ScanCursors,
) -> Vec<ScanWarning> {
    let mut warnings = Vec::new();
    let result = scan_action_cancellations_once(store, cursors.action_cancel).await;
    advance(
        &mut cursors.action_cancel,
        result,
        "action cancellations could not be recorded",
        "action cancellation scan unavailable",
        &mut warnings,
    );
    let result = scan_late_receipts_once(store, cursors.late_receipt).await;
    advance(
        &mut cursors.late_receipt,
        result,
        "late receipt reconciliations failed",
        "late receipt reconciliation unavailable",
        &mut warnings,
    );
    let result = scan_fresh_recovery_once(store, cursors.fresh_recovery).await;
    advance(
        &mut cursors.fresh_recovery,
        result,
        "fresh runs could not be recovered",
        "fresh recovery scan unavailable",
        &mut warnings,
    );
    let result = scan_children_wait_settlement_once(store, cursors.children_wait).await;
    advance(
        &mut cursors.children_wait,
        result,
        "child waits could not be settled",
        "child wait settlement unavailable",
        &mut warnings,
    );
    let result = scan_wait_expiry_once(store, cursors.wait_expiry).await;
    advance(
        &mut cursors.wait_expiry,
        result,
        "waiting runs could not be expired",
        "wait expiry scan unavailable",
        &mut warnings,
    );
    let result = scan_committed_recovery_once(store, cursors.committed_recovery).await;
    advance(
        &mut cursors.committed_recovery,
        result,
        "committed runs could not be recovered",
        "committed recovery scan unavailable",
        &mut warnings,
    );
    let result = scan_pending_expiry_once(store, cursors.pending_expiry).await;
    advance(
        &mut cursors.pending_expiry,
        result,
        "pending runs could not be expired",
        "pending expiry scan unavailable",
        &mut warnings,
    );
    let result = scan_calendar_once(store, cursors.calendar).await;
    advance(
        &mut cursors.calendar,
        result,
        "calendar rows could not be materialized",
        "calendar scan unavailable",
        &mut warnings,
    );
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_advances_or_restarts_each_cursor_and_keeps_the_log_text() {
        let mut warnings = Vec::new();
        let mut cursor = 7;
        advance(
            &mut cursor,
            Ok(ScheduleScanReport {
                failed: 2,
                next_cursor: Some(40),
                ..Default::default()
            }),
            "pending runs could not be expired",
            "pending expiry scan unavailable",
            &mut warnings,
        );
        assert_eq!(cursor, 40);
        advance(
            &mut cursor,
            Ok(ScheduleScanReport::default()),
            "pending runs could not be expired",
            "pending expiry scan unavailable",
            &mut warnings,
        );
        assert_eq!(cursor, 0);
        cursor = 9;
        advance(
            &mut cursor,
            Err(ScheduleStoreError::Invalid),
            "calendar rows could not be materialized",
            "calendar scan unavailable",
            &mut warnings,
        );
        assert_eq!(cursor, 0);
        assert_eq!(
            warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "[schedule] 2 pending runs could not be expired",
                "[schedule] calendar scan unavailable; retrying",
            ]
        );
    }
}
