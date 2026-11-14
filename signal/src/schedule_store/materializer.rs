//! Central calendar pump. Queuing is not authorization to execute a model or tool.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_schedule_run as run;
pub use desk_assistant_data_store::schedule::ScheduleScanReport;
use std::time::Duration;

#[async_trait::async_trait]
impl desk_assistant_data_store::schedule::ScheduleScanOps for ScheduleStore {
    type Conn = crate::config::connection::DatabaseConnection;

    fn connection(&self) -> &Self::Conn {
        &self.db
    }

    async fn propagate_task_cancellation(&self, run_id: &str) -> Result<usize, ScheduleStoreError> {
        ScheduleStore::propagate_task_cancellation(self, run_id).await
    }

    async fn reconcile_late_task_receipts(&self, run_id: &str) -> Result<bool, ScheduleStoreError> {
        ScheduleStore::reconcile_late_task_receipts(self, run_id).await
    }

    async fn recover_action_free_fresh_task(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        ScheduleStore::recover_action_free_fresh_task(self, run_id).await
    }

    async fn settle_fresh_children_wait(&self, run_id: &str) -> Result<bool, ScheduleStoreError> {
        ScheduleStore::settle_fresh_children_wait(self, run_id).await
    }

    async fn cancel_continuation_wait(&self, run_id: &str) -> Result<bool, ScheduleStoreError> {
        ScheduleStore::cancel_continuation_wait(self, run_id).await
    }

    async fn expire_fresh_approval_wait(&self, run_id: &str) -> Result<bool, ScheduleStoreError> {
        ScheduleStore::expire_fresh_approval_wait(self, run_id).await
    }

    async fn recover_committed_continuation(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<Option<run::Model>, ScheduleStoreError> {
        ScheduleStore::recover_committed_continuation(self, owner, run_id).await
    }

    async fn materialize_due(
        &self,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<Option<run::Model>, ScheduleStoreError> {
        ScheduleStore::materialize_due(self, schedule_id, expected_revision).await
    }

    async fn expire_pending(&self, run_id: &str) -> Result<run::Model, ScheduleStoreError> {
        ScheduleStore::expire_pending(self, run_id).await
    }
}

impl ScheduleStore {
    /// One loop per central process. Manager nodes may all scan: database fences,
    /// not this timer, decide which node creates each occurrence. Only the local
    /// scan cursor is volatile; task calendars and catch-up decisions are durable.
    pub fn run_calendar_materializer(
        self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        desk_diagnose_core::future::send_phase(move || async move {
            let mut cursors = desk_assistant_data_store::schedule::ScanCursors::default();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                for warning in
                    desk_assistant_data_store::schedule::scan_tick(&self, &mut cursors).await
                {
                    log::warn!("{warning}");
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        entity,
        tests::{draft, store},
    };
    use super::*;
    use desk_agent_protocol::schedule::ScheduleRule;
    use desk_assistant_data_store::schedule::database_now;
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
            let task = store.create_pending_task(1, &draft, now).await.unwrap();
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
        let first = desk_assistant_data_store::schedule::scan_calendar_once(&store, 0)
            .await
            .unwrap();
        assert_eq!(
            (first.scanned, first.materialized, first.failed),
            (32, 31, 1)
        );
        let second = desk_assistant_data_store::schedule::scan_calendar_once(
            &store,
            first.next_cursor.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            (second.scanned, second.materialized, second.failed),
            (3, 3, 0)
        );
        assert_eq!(second.next_cursor, None);
        let restarted = ScheduleStore::new(store.db.clone());
        let replay = desk_assistant_data_store::schedule::scan_calendar_once(&restarted, 0)
            .await
            .unwrap();
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
        let first = desk_assistant_data_store::schedule::scan_pending_expiry_once(&restarted, 0)
            .await
            .unwrap();
        assert_eq!((first.scanned, first.expired, first.failed), (32, 32, 0));
        let second = desk_assistant_data_store::schedule::scan_pending_expiry_once(
            &restarted,
            first.next_cursor.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!((second.scanned, second.expired, second.failed), (1, 1, 0));
        assert_eq!(
            desk_assistant_data_store::schedule::scan_pending_expiry_once(&restarted, 0)
                .await
                .unwrap()
                .scanned,
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
