//! Versioned task edits and tombstones preserve immutable running snapshots.
use super::queue::database_now;
use super::{ScheduleStore, ScheduleStoreError, entity, json};
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::schedule::ScheduleSpec;
use desk_diagnose_core::schedule::{normalize, validate_publication};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set, TransactionTrait};

/// Called only while the task's version CAS owns the transaction.
pub(super) async fn stop_pending(
    txn: &DatabaseTransaction,
    work: &run::Model,
    now: i64,
    request_running_cancel: bool,
) -> Result<bool, ScheduleStoreError> {
    let unstarted = matches!(work.status.as_str(), "queued" | "waiting_device");
    if !unstarted && !request_running_cancel {
        return Ok(false);
    }
    if work.failure_accounted {
        return Ok(false);
    }
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set(if unstarted {
                "cancelled".into()
            } else {
                work.status.clone()
            }),
            cancel_requested_at: Set(Some(now)),
            failure_accounted: Set(unstarted),
            finished_at: Set(if unstarted {
                Some(now)
            } else {
                work.finished_at
            }),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .filter(run::Column::Status.eq(&work.status))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(unstarted)
}

impl ScheduleStore {
    /// Change failure policy only between runs. This never clears pauses or
    /// changes the recovery epoch, and it cannot revive a completed occurrence.
    pub async fn set_failure_threshold(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        threshold: u32,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.revision != expected
            || task.active_run_id.is_some()
            || matches!(task.status.as_str(), "deleted" | "completed")
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let mut failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
        failures
            .set_threshold(threshold)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let paused = !failures.pause_reasons.is_empty();
        let now = database_now(&txn).await?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                failure_state_json: Set(json(&failures)?),
                revision: Set(expected.checked_add(1).ok_or(ScheduleStoreError::Invalid)?),
                next_run_at: Set(if paused { None } else { task.next_run_at }),
                status: Set(
                    if paused && matches!(task.status.as_str(), "active" | "triggered" | "paused") {
                        "paused".into()
                    } else {
                        task.status
                    },
                ),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(expected))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = entity::Entity::find_by_id(task.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(result)
    }

    pub async fn rename(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        title: &str,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if title.trim().is_empty() || title.len() > 240 {
            return Err(ScheduleStoreError::Invalid);
        }
        let revision = expected.checked_add(1).ok_or(ScheduleStoreError::Invalid)?;
        let now = database_now(&self.db).await?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                title: Set(title.into()),
                revision: Set(revision),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(id))
            .filter(entity::Column::Revision.eq(expected))
            .filter(entity::Column::Status.ne("deleted"))
            .exec(&self.db)
            .await?;
        if changed.rows_affected != 1 {
            self.read(owner, id).await?;
            return Err(ScheduleStoreError::Conflict);
        }
        self.read(owner, id).await
    }

    pub async fn change_time(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        spec: &ScheduleSpec,
    ) -> Result<entity::Model, ScheduleStoreError> {
        self.edit(owner, id, expected, Some(spec), None, false)
            .await
    }

    /// A changed requirement always needs a new rehearsal/authorization publication.
    pub async fn change_prompt(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        prompt: &str,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if prompt.trim().is_empty()
            || prompt.len() > desk_diagnose_core::schedule::MAX_SCHEDULE_PROMPT_BYTES
        {
            return Err(ScheduleStoreError::Invalid);
        }
        self.edit(owner, id, expected, None, Some(prompt), false)
            .await
    }

    pub async fn delete(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        self.edit(owner, id, expected, None, None, true).await
    }

    async fn edit(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        new_spec: Option<&ScheduleSpec>,
        prompt: Option<&str>,
        delete: bool,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let now = database_now(&txn).await?;
        let row = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.revision != expected {
            return Err(ScheduleStoreError::Conflict);
        }
        if row.status == "deleted" {
            if delete {
                return Ok(row);
            }
            return Err(ScheduleStoreError::Conflict);
        }
        if row.status == "completed" && !delete {
            return Err(ScheduleStoreError::Conflict);
        }
        if prompt == Some(row.prompt.as_str()) {
            return Ok(row);
        }
        if let Some(spec) = new_spec {
            let normalized = normalize(spec).map_err(|_| ScheduleStoreError::Invalid)?;
            if json(&normalized)? == row.spec_json {
                return Ok(row);
            }
        }
        let work = if let Some(run_id) = row.active_run_id.as_deref() {
            run::Entity::find()
                .filter(run::Column::RunId.eq(run_id))
                .one(&txn)
                .await?
        } else {
            None
        };
        let unstarted = work
            .as_ref()
            .is_some_and(|w| matches!(w.status.as_str(), "queued" | "waiting_device"));
        if prompt.is_some()
            && row.kind == "conversation_resume"
            && row.active_run_id.is_some()
            && !unstarted
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if row.status == "triggered" && new_spec.is_some() && !unstarted {
            return Err(ScheduleStoreError::Conflict);
        }
        let mut patch = entity::ActiveModel {
            revision: Set(expected.checked_add(1).ok_or(ScheduleStoreError::Invalid)?),
            updated_at: Set(now),
            active_run_id: Set(if unstarted {
                None
            } else {
                row.active_run_id.clone()
            }),
            ..Default::default()
        };
        if delete {
            patch.status = Set("deleted".into());
            patch.next_run_at = Set(None);
        }
        if let Some(prompt) = prompt {
            patch.prompt = Set(prompt.into());
            patch.task_revision = Set(row
                .task_revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?);
            let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
                serde_json::from_str(&row.failure_state_json)
                    .map_err(|_| ScheduleStoreError::Invalid)?;
            patch.status = Set(
                if row.kind == "conversation_resume" && !failures.pause_reasons.is_empty() {
                    "paused".into()
                } else {
                    "draft".into()
                },
            );
            patch.next_run_at = Set(None);
            patch.contract_revision = Set(None);
            patch.authorization_revision = Set(None);
        }
        if let Some(spec) = new_spec {
            let normalized = normalize(spec).map_err(|_| ScheduleStoreError::Invalid)?;
            if row.kind == "conversation_resume"
                && !matches!(
                    normalized.rule,
                    desk_agent_protocol::schedule::ScheduleRule::Once { .. }
                )
            {
                return Err(ScheduleStoreError::Invalid);
            }
            patch.spec_json = Set(json(&normalized)?);
            if matches!(row.status.as_str(), "active" | "triggered") {
                let normalized =
                    validate_publication(&normalized, now, row.kind == "conversation_resume")
                        .map_err(|_| ScheduleStoreError::Invalid)?;
                patch.next_run_at = Set(desk_diagnose_core::schedule::next_after(&normalized, now)
                    .map_err(|_| ScheduleStoreError::Invalid)?);
                patch.status = Set("active".into());
                patch.recurrence_cursor_at = Set(Some(now));
            }
        }
        let changed = entity::Entity::update_many()
            .set(patch)
            .filter(entity::Column::Id.eq(row.id))
            .filter(entity::Column::Revision.eq(expected))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        if prompt.is_some() && row.kind == "conversation_resume" {
            super::resume_activation::lock_original_requirement(&txn, owner, &row).await?;
        }
        if let Some(work) = work {
            stop_pending(&txn, &work, now, delete).await?;
        }
        let result = entity::Entity::find_by_id(row.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(result)
    }
}
