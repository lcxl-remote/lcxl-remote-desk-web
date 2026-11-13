//! Owner-scoped, newest-first occurrence history for management projections.
use super::{ScheduleStore, ScheduleStoreError};
use desk_assistant_data_store::schedule::{self as shared, RunHistoryPage};

impl ScheduleStore {
    /// Resolve a public occurrence selector. The snapshot reader must still
    /// verify current account/device access and the stored session subject.
    pub async fn resolve_run_session(
        &self,
        owner: i32,
        schedule_id: &str,
        run_id: &str,
    ) -> Result<(String, String), ScheduleStoreError> {
        shared::resolve_run_session(&self.db, owner, schedule_id, run_id).await
    }

    pub async fn run_history(
        &self,
        owner: i32,
        schedule_id: &str,
        before_run_id: Option<&str>,
        limit: u32,
    ) -> Result<RunHistoryPage, ScheduleStoreError> {
        shared::run_history(&self.db, owner, schedule_id, before_run_id, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::enabled_fresh_task_on;
    use super::*;
    use crate::entity::agent_schedule_run as run;
    use sea_orm::{ActiveValue::NotSet, EntityTrait, IntoActiveModel, Set};

    #[tokio::test]
    async fn public_run_selector_requires_started_original_subject() {
        use sea_orm::ActiveModelTrait;
        let (store, task) = enabled_fresh_task_on(
            crate::config::test_support::Database::connect("sqlite::memory:")
                .await
                .unwrap(),
            None,
            1,
            None,
        )
        .await;
        let row = store
            .enqueue_manual(1, &task.schedule_id, "result-selector")
            .await
            .unwrap();
        assert!(matches!(
            store
                .resolve_run_session(1, &task.schedule_id, &row.run_id)
                .await,
            Err(ScheduleStoreError::NotFound)
        ));
        let mut started = row.clone().into_active_model();
        started.started_at = Set(Some(row.requested_at));
        started.status = Set("running".into());
        let started = started.update(&store.db).await.unwrap();
        assert_eq!(
            store
                .resolve_run_session(1, &task.schedule_id, &row.run_id)
                .await
                .unwrap(),
            (row.conversation_id.clone(), task.target_device_id.clone())
        );
        let mut resumed = started.clone().into_active_model();
        resumed.turn_id = Set("permission-resume-original-decision".into());
        resumed.update(&store.db).await.unwrap();
        assert_eq!(
            store
                .resolve_run_session(1, &task.schedule_id, &row.run_id)
                .await
                .unwrap(),
            (row.conversation_id.clone(), task.target_device_id.clone())
        );
        for (owner, task_id, run_id) in [
            (2, task.schedule_id.as_str(), row.run_id.as_str()),
            (1, "other-task", row.run_id.as_str()),
            (1, task.schedule_id.as_str(), "other-run"),
        ] {
            assert!(matches!(
                store.resolve_run_session(owner, task_id, run_id).await,
                Err(ScheduleStoreError::NotFound)
            ));
        }
        for field in ["session", "turn", "snapshot"] {
            let mut corrupt = started.clone().into_active_model();
            match field {
                "session" => corrupt.conversation_id = Set("foreign-session".into()),
                "turn" => corrupt.turn_id = Set(String::new()),
                _ => corrupt.task_snapshot_json = Set("{}".into()),
            }
            corrupt.update(&store.db).await.unwrap();
            assert!(matches!(
                store
                    .resolve_run_session(1, &task.schedule_id, &row.run_id)
                    .await,
                Err(ScheduleStoreError::NotFound)
            ));
            // Restore each changed field rather than relying on unchanged ActiveValues.
            let mut restore = started.clone().into_active_model();
            restore.conversation_id = Set(started.conversation_id.clone());
            restore.turn_id = Set(started.turn_id.clone());
            restore.task_snapshot_json = Set(started.task_snapshot_json.clone());
            restore.update(&store.db).await.unwrap();
        }
    }

    #[tokio::test]
    async fn history_is_task_scoped_and_new_inserts_do_not_shift_the_cursor() {
        let (store, task) = enabled_fresh_task_on(
            crate::config::test_support::Database::connect("sqlite::memory:")
                .await
                .unwrap(),
            None,
            1,
            None,
        )
        .await;
        let first = store
            .enqueue_manual(1, &task.schedule_id, "history-first")
            .await
            .unwrap();
        for (id, owner, schedule) in [
            ("second", 1, task.schedule_id.as_str()),
            ("third", 1, task.schedule_id.as_str()),
            ("foreign-owner", 2, task.schedule_id.as_str()),
            ("foreign-task", 1, "another-task"),
        ] {
            let mut row = first.clone().into_active_model();
            row.id = NotSet;
            row.run_id = Set(id.into());
            row.occurrence_identity = Set(id.into());
            row.owner_user_id = Set(owner);
            row.schedule_id = Set(schedule.into());
            run::Entity::insert(row).exec(&store.db).await.unwrap();
        }
        let page = store
            .run_history(1, &task.schedule_id, None, 2)
            .await
            .unwrap();
        assert_eq!(
            page.runs
                .iter()
                .map(|row| row.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["third", "second"]
        );
        assert_eq!(page.next_cursor.as_deref(), Some("second"));
        let mut newest = first.clone().into_active_model();
        newest.id = NotSet;
        newest.run_id = Set("newest".into());
        newest.occurrence_identity = Set("newest".into());
        run::Entity::insert(newest).exec(&store.db).await.unwrap();
        let next = store
            .run_history(1, &task.schedule_id, page.next_cursor.as_deref(), 2)
            .await
            .unwrap();
        assert_eq!(next.runs, vec![first]);
        assert!(next.next_cursor.is_none());
        for cursor in ["foreign-owner", "foreign-task", "missing"] {
            assert!(matches!(
                store
                    .run_history(1, &task.schedule_id, Some(cursor), 2)
                    .await,
                Err(ScheduleStoreError::NotFound)
            ));
        }
        assert!(matches!(
            store.run_history(2, &task.schedule_id, None, 2).await,
            Err(ScheduleStoreError::NotFound)
        ));
        for limit in [0, 101] {
            assert!(matches!(
                store.run_history(1, &task.schedule_id, None, limit).await,
                Err(ScheduleStoreError::Invalid)
            ));
        }
    }
}
