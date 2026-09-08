//! Central calendar pump. Queuing is not authorization to execute a model or tool.
use super::{ScheduleStore, ScheduleStoreError};
use std::time::Duration;

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

impl ScheduleStore {
    pub async fn scan_action_cancellations_once(
        &self,
        after_id: i64,
    ) -> Result<ScheduleScanReport, ScheduleStoreError> {
        let candidates = self.action_cancel_candidates(after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match self.propagate_task_cancellation(&candidate.run_id).await {
                Ok(count) if count > 0 => report.recovered += count,
                Ok(_) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                    report.deferred += 1
                }
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }

    pub async fn scan_late_receipts_once(
        &self,
        after_id: i64,
    ) -> Result<ScheduleScanReport, ScheduleStoreError> {
        let candidates = self.late_receipt_candidates(after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match self.reconcile_late_task_receipts(&candidate.run_id).await {
                Ok(true) => report.recovered += 1,
                Ok(false) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                    report.deferred += 1
                }
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }

    pub async fn scan_fresh_recovery_once(
        &self,
        after_id: i64,
    ) -> Result<ScheduleScanReport, ScheduleStoreError> {
        let candidates = self.expired_fresh_candidates(after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match self.recover_action_free_fresh_task(&candidate.run_id).await {
                Ok(true) => report.recovered += 1,
                Ok(false) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                    report.deferred += 1
                }
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }

    pub async fn scan_approval_expiry_once(
        &self,
        after_id: i64,
    ) -> Result<ScheduleScanReport, ScheduleStoreError> {
        let candidates = self.approval_wait_candidates(after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match self.expire_fresh_approval_wait(&candidate.run_id).await {
                Ok(true) => report.expired += 1,
                Ok(false) | Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                    report.deferred += 1
                }
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }

    /// Reconcile durable results and action-free interrupted turns. Unfinished
    /// actions remain for original-result reconciliation without another dispatch.
    pub async fn scan_committed_recovery_once(
        &self,
        after_id: i64,
    ) -> Result<ScheduleScanReport, ScheduleStoreError> {
        let candidates = self
            .expired_continuation_candidates(after_id, BATCH_SIZE)
            .await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match self
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
    pub async fn scan_calendar_once(
        &self,
        after_id: i64,
    ) -> Result<ScheduleScanReport, ScheduleStoreError> {
        let candidates = self.due_candidates(after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match self
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
    pub async fn scan_pending_expiry_once(
        &self,
        after_id: i64,
    ) -> Result<ScheduleScanReport, ScheduleStoreError> {
        let candidates = self.expired_pending(after_id, BATCH_SIZE).await?;
        let mut report = ScheduleScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            match self.expire_pending(&candidate.run_id).await {
                Ok(_) => report.expired += 1,
                Err(ScheduleStoreError::Conflict | ScheduleStoreError::NotFound) => {
                    report.deferred += 1
                }
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }

    /// One loop per central process. Manager nodes may all scan: database fences,
    /// not this timer, decide which node creates each occurrence. Only the local
    /// scan cursor is volatile; task calendars and catch-up decisions are durable.
    pub async fn run_calendar_materializer(self) {
        let mut cursor = 0;
        let mut expiry_cursor = 0;
        let mut approval_cursor = 0;
        let mut recovery_cursor = 0;
        let mut fresh_recovery_cursor = 0;
        let mut late_receipt_cursor = 0;
        let mut action_cancel_cursor = 0;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match self
                .scan_action_cancellations_once(action_cancel_cursor)
                .await
            {
                Ok(report) => {
                    action_cancel_cursor = report.next_cursor.unwrap_or(0);
                    if report.failed > 0 {
                        log::warn!(
                            "[schedule] {} action cancellations could not be recorded",
                            report.failed
                        );
                    }
                }
                Err(_) => {
                    action_cancel_cursor = 0;
                    log::warn!("[schedule] action cancellation scan unavailable; retrying");
                }
            }
            match self.scan_late_receipts_once(late_receipt_cursor).await {
                Ok(report) => {
                    late_receipt_cursor = report.next_cursor.unwrap_or(0);
                    if report.failed > 0 {
                        log::warn!(
                            "[schedule] {} late receipt reconciliations failed",
                            report.failed
                        );
                    }
                }
                Err(_) => {
                    late_receipt_cursor = 0;
                    log::warn!("[schedule] late receipt reconciliation unavailable; retrying");
                }
            }
            match self.scan_fresh_recovery_once(fresh_recovery_cursor).await {
                Ok(report) => {
                    fresh_recovery_cursor = report.next_cursor.unwrap_or(0);
                    if report.failed > 0 {
                        log::warn!(
                            "[schedule] {} fresh runs could not be recovered",
                            report.failed
                        );
                    }
                }
                Err(_) => {
                    fresh_recovery_cursor = 0;
                    log::warn!("[schedule] fresh recovery scan unavailable; retrying");
                }
            }
            match self.scan_approval_expiry_once(approval_cursor).await {
                Ok(report) => {
                    approval_cursor = report.next_cursor.unwrap_or(0);
                    if report.failed > 0 {
                        log::warn!(
                            "[schedule] {} approval waits could not be expired",
                            report.failed
                        );
                    }
                }
                Err(_) => {
                    approval_cursor = 0;
                    log::warn!("[schedule] approval expiry scan unavailable; retrying");
                }
            }
            match self.scan_committed_recovery_once(recovery_cursor).await {
                Ok(report) => {
                    recovery_cursor = report.next_cursor.unwrap_or(0);
                    if report.failed > 0 {
                        log::warn!(
                            "[schedule] {} committed runs could not be recovered",
                            report.failed
                        );
                    }
                }
                Err(_) => {
                    recovery_cursor = 0;
                    log::warn!("[schedule] committed recovery scan unavailable; retrying");
                }
            }

            match self.scan_pending_expiry_once(expiry_cursor).await {
                Ok(report) => {
                    expiry_cursor = report.next_cursor.unwrap_or(0);
                    if report.failed > 0 {
                        log::warn!(
                            "[schedule] {} pending runs could not be expired",
                            report.failed
                        );
                    }
                }
                Err(_) => {
                    expiry_cursor = 0;
                    log::warn!("[schedule] pending expiry scan unavailable; retrying");
                }
            }
            match self.scan_calendar_once(cursor).await {
                Ok(report) => {
                    cursor = report.next_cursor.unwrap_or(0);
                    if report.failed > 0 {
                        log::warn!(
                            "[schedule] {} calendar rows could not be materialized",
                            report.failed
                        );
                    }
                }
                Err(_) => {
                    cursor = 0;
                    log::warn!("[schedule] calendar scan unavailable; retrying");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::database_now;
    use super::super::{
        entity,
        tests::{draft, store},
    };
    use super::*;
    use desk_agent_protocol::schedule::ScheduleRule;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set};

    #[tokio::test]
    async fn calendar_pump_pages_past_bad_rows_and_restart_never_duplicates_or_claims_work() {
        let store = store().await;
        let now = database_now(&store.db).await.unwrap();
        let at = (now / 1000 - 1) * 1000;
        for index in 0..35 {
            let mut draft = draft();
            draft.client_create_key = format!("calendar-{index}");
            draft.spec.rule = ScheduleRule::Once {
                at: chrono::DateTime::from_timestamp_millis(at)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            };
            let task = store.create_draft(1, &draft, now).await.unwrap();
            let mut update = entity::ActiveModel {
                status: Set("active".into()),
                next_run_at: Set(Some(at)),
                ..Default::default()
            };
            if index == 0 {
                update.spec_json = Set("invalid-json".into());
            }
            entity::Entity::update_many()
                .set(update)
                .filter(entity::Column::Id.eq(task.id))
                .exec(&store.db)
                .await
                .unwrap();
        }
        let first = store.scan_calendar_once(0).await.unwrap();
        assert_eq!(
            (first.scanned, first.materialized, first.failed),
            (32, 31, 1)
        );
        let second = store
            .scan_calendar_once(first.next_cursor.unwrap())
            .await
            .unwrap();
        assert_eq!(
            (second.scanned, second.materialized, second.failed),
            (3, 3, 0)
        );
        assert_eq!(second.next_cursor, None);
        let restarted = ScheduleStore::new(store.db.clone());
        let replay = restarted.scan_calendar_once(0).await.unwrap();
        assert_eq!(
            (replay.scanned, replay.materialized, replay.failed),
            (1, 0, 1)
        );
        let runs = crate::entity::agent_schedule_run::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(runs.len(), 34);
        assert!(
            runs.iter()
                .all(|r| r.status == "queued" && r.lease_owner.is_none() && r.started_at.is_none())
        );
        assert_eq!(
            crate::entity::agent_schedule_run::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            34
        );
        let running = store
            .claim_queued(&runs[0].run_id, "calendar-test", 60)
            .await
            .unwrap();
        store.wait_for_device(&runs[1].run_id).await.unwrap();
        crate::entity::agent_schedule_run::Entity::update_many()
            .set(crate::entity::agent_schedule_run::ActiveModel {
                start_deadline: Set(at),
                ..Default::default()
            })
            .exec(&store.db)
            .await
            .unwrap();
        let first = restarted.scan_pending_expiry_once(0).await.unwrap();
        assert_eq!((first.scanned, first.expired, first.failed), (32, 32, 0));
        let second = restarted
            .scan_pending_expiry_once(first.next_cursor.unwrap())
            .await
            .unwrap();
        assert_eq!((second.scanned, second.expired, second.failed), (1, 1, 0));
        assert_eq!(
            restarted.scan_pending_expiry_once(0).await.unwrap().scanned,
            0
        );
        let preserved = crate::entity::agent_schedule_run::Entity::find_by_id(running.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preserved.status, "running");
        assert_eq!(preserved.lease_owner, running.lease_owner);
        assert_eq!(preserved.lease_epoch, running.lease_epoch);
        assert_eq!(
            store
                .read(1, &running.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(running.run_id.as_str())
        );
    }
}
