//! Revocation serializes against task publication/dispatch and retains original audit rows.
use super::queue::database_now;
use super::{ScheduleStore, ScheduleStoreError, entity, json};
use crate::entity::{agent_schedule_run as run, agent_task_authorization as authorization};
use desk_agent_protocol::schedule::SchedulePauseReason;
use desk_diagnose_core::schedule::lifecycle::FailureState;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};

impl ScheduleStore {
    pub async fn read_authorization(
        &self,
        owner: i32,
        authorization_id: &str,
    ) -> Result<authorization::Model, ScheduleStoreError> {
        authorization::Entity::find()
            .filter(authorization::Column::OwnerUserId.eq(owner))
            .filter(authorization::Column::AuthorizationId.eq(authorization_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)
    }

    pub async fn revoke_task_authorization(
        &self,
        owner: i32,
        authorization_id: &str,
        reason: &str,
    ) -> Result<authorization::Model, ScheduleStoreError> {
        self.revoke_authorization_checked(owner, authorization_id, reason, None)
            .await
    }

    /// Revoke only the current authorization at the task revision the owner saw.
    pub async fn revoke_current_task_authorization(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let task = self.read(owner, schedule_id).await?;
        if task.kind != "fresh_task"
            || task.revision != expected_revision
            || task.status == "deleted"
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let row = authorization::Entity::find()
            .filter(authorization::Column::OwnerUserId.eq(owner))
            .filter(authorization::Column::ScheduleId.eq(schedule_id))
            .filter(
                authorization::Column::AuthorizationRevision.eq(task
                    .authorization_revision
                    .ok_or(ScheduleStoreError::Conflict)?),
            )
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        self.revoke_authorization_checked(
            owner,
            &row.authorization_id,
            "owner revoked task authorization",
            Some(expected_revision),
        )
        .await?;
        self.read(owner, schedule_id).await
    }

    async fn revoke_authorization_checked(
        &self,
        owner: i32,
        authorization_id: &str,
        reason: &str,
        expected_revision: Option<i64>,
    ) -> Result<authorization::Model, ScheduleStoreError> {
        if reason.trim().is_empty() || reason.len() > 512 {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = self.db.begin().await?;
        let now = database_now(&txn).await?;
        let row = authorization::Entity::find()
            .filter(authorization::Column::OwnerUserId.eq(owner))
            .filter(authorization::Column::AuthorizationId.eq(authorization_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.revoked_at.is_some() && expected_revision.is_none() {
            return Ok(row);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&row.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if expected_revision.is_some_and(|revision| {
            task.revision != revision
                || task.authorization_revision != Some(row.authorization_revision)
                || task.task_revision != row.task_revision
                || task.kind != "fresh_task"
                || task.status == "deleted"
        }) {
            return Err(ScheduleStoreError::Conflict);
        }
        if row.revoked_at.is_some() {
            return Ok(row);
        }
        let work = if let Some(run_id) = &task.active_run_id {
            run::Entity::find()
                .filter(run::Column::RunId.eq(run_id))
                .one(&txn)
                .await?
        } else {
            None
        };
        let work_uses_authorization = if let Some(work) = &work {
            let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            snapshot.authorization_revision == Some(row.authorization_revision)
                && snapshot.task_revision == row.task_revision
        } else {
            false
        };
        let current = task.authorization_revision == Some(row.authorization_revision)
            && task.task_revision == row.task_revision;
        let unstarted = work_uses_authorization
            && work
                .as_ref()
                .is_some_and(|w| matches!(w.status.as_str(), "queued" | "waiting_device"));
        let mut patch = entity::ActiveModel {
            revision: Set(task
                .revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            updated_at: Set(now),
            ..Default::default()
        };
        if current {
            let mut failures: FailureState = serde_json::from_str(&task.failure_state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            failures
                .pause_reasons
                .insert(SchedulePauseReason::AuthorizationInvalid);
            patch.failure_state_json = Set(json(&failures)?);
            patch.next_run_at = Set(None);
            if matches!(task.status.as_str(), "active" | "triggered" | "paused") {
                patch.status = Set("paused".into());
            }
        }
        if unstarted {
            patch.active_run_id = Set(None);
        }
        let changed = entity::Entity::update_many()
            .set(patch)
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let changed = authorization::Entity::update_many()
            .set(authorization::ActiveModel {
                revoked_at: Set(Some(now)),
                revoked_reason: Set(Some(reason.into())),
                version: Set(row
                    .version
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                ..Default::default()
            })
            .filter(authorization::Column::Id.eq(row.id))
            .filter(authorization::Column::Version.eq(row.version))
            .filter(authorization::Column::RevokedAt.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        if work_uses_authorization && let Some(work) = &work {
            super::edit::stop_pending(&txn, work, now, true).await?;
        }
        let result = authorization::Entity::find_by_id(row.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(result)
    }
}
