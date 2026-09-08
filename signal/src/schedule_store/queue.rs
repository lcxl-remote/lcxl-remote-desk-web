//! Transactional calendar materialization and lease claims.
use super::{ScheduleStore, ScheduleStoreError, digest, entity, json};

#[cfg(test)]
mod tests;
use crate::entity::agent_schedule_run as run;
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION,
    due::{due_window, start_deadline},
    lifecycle::FailureState,
    parse_json,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    TransactionTrait,
    sea_query::{Alias, Expr, Query},
};

pub(super) async fn database_now<C: ConnectionTrait>(db: &C) -> Result<i64, ScheduleStoreError> {
    let query = Query::select()
        .expr_as(Expr::current_timestamp(), Alias::new("now"))
        .to_owned();
    let row = db
        .query_one(&query)
        .await?
        .ok_or(ScheduleStoreError::Invalid)?;
    Ok(row
        .try_get::<chrono::DateTime<chrono::Utc>>("", "now")?
        .timestamp_millis())
}

impl ScheduleStore {
    /// Manual execution has its own idempotency identity and never moves the calendar.
    pub async fn enqueue_manual(
        &self,
        owner: i32,
        schedule_id: &str,
        client_key: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.enqueue_manual_checked(owner, schedule_id, client_key, None)
            .await
    }

    /// Bind a management request to the version the owner reviewed.
    pub async fn enqueue_manual_at_revision(
        &self,
        owner: i32,
        schedule_id: &str,
        client_key: &str,
        expected_revision: i64,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.enqueue_manual_checked(owner, schedule_id, client_key, Some(expected_revision))
            .await
    }

    async fn enqueue_manual_checked(
        &self,
        owner: i32,
        schedule_id: &str,
        client_key: &str,
        expected_revision: Option<i64>,
    ) -> Result<run::Model, ScheduleStoreError> {
        if owner <= 0
            || client_key.is_empty()
            || client_key.len() > 256
            || client_key.chars().any(char::is_control)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = self.db.begin().await?;
        let now = database_now(&txn).await?;
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .filter(entity::Column::OwnerUserId.eq(owner))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let identity = digest(&json(&(schedule_id, "manual", client_key))?);
        if let Some(existing) = run::Entity::find()
            .filter(run::Column::OccurrenceIdentity.eq(&identity))
            .one(&txn)
            .await?
        {
            if existing.owner_user_id != owner
                || existing.schedule_id != schedule_id
                || expected_revision.is_some_and(|revision| existing.schedule_revision != revision)
            {
                return Err(ScheduleStoreError::Conflict);
            }
            return Ok(existing);
        }
        if expected_revision.is_some_and(|revision| revision != task.revision) {
            return Err(ScheduleStoreError::Conflict);
        }
        let failures: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if task.kind != "fresh_task"
            || task.status != "active"
            || task.active_run_id.is_some()
            || !failures.pause_reasons.is_empty()
            || task.contract_revision.is_none()
            || task.authorization_revision.is_none()
            || task.calc_version != SCHEDULE_CALC_VERSION
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let run_id = format!("schedule-run-{identity}");
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                active_run_id: Set(Some(run_id.clone())),
                updated_at: Set(now),
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .filter(entity::Column::ActiveRunId.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let grace = u32::try_from(task.grace_seconds).map_err(|_| ScheduleStoreError::Invalid)?;
        run::Entity::insert(run::ActiveModel {
            run_id: Set(run_id.clone()),
            schedule_id: Set(task.schedule_id.clone()),
            owner_user_id: Set(owner),
            occurrence_identity: Set(identity),
            source: Set("manual".into()),
            scheduled_at: Set(None),
            requested_at: Set(now),
            schedule_revision: Set(task.revision),
            recovery_epoch: Set(
                i64::try_from(failures.recovery_epoch).map_err(|_| ScheduleStoreError::Invalid)?
            ),
            task_snapshot_json: Set(json(&task)?),
            conversation_id: Set(run_id.clone()),
            turn_id: Set(format!("{run_id}-turn")),
            status: Set("queued".into()),
            start_deadline: Set(
                start_deadline(now, grace).map_err(|_| ScheduleStoreError::Invalid)?
            ),
            lease_epoch: Set(0),
            lease_owner: Set(None),
            lease_deadline: Set(None),
            attempt: Set(0),
            failure_accounted: Set(false),
            error_kind: Set(None),
            result_ref: Set(None),
            missed_count: Set(0),
            started_at: Set(None),
            finished_at: Set(None),
            receipts_reconciled_at: Set(None),
            outcome_review_json: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec_without_returning(&txn)
        .await?;
        let result = run::Entity::find()
            .filter(run::Column::RunId.eq(&run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(result)
    }
    pub async fn due_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<entity::Model>, ScheduleStoreError> {
        if after_id < 0 || limit == 0 || limit > 100 {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = database_now(&self.db).await?;
        Ok(entity::Entity::find()
            .filter(entity::Column::Status.eq("active"))
            .filter(entity::Column::NextRunAt.lte(now))
            .filter(entity::Column::Id.gt(after_id))
            .order_by_asc(entity::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    /// Only the row CAS can create an occurrence and advance its calendar cursor.
    pub async fn materialize_due(
        &self,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<Option<run::Model>, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let now = database_now(&txn).await?;
        let row = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.revision != expected_revision {
            return Err(ScheduleStoreError::Conflict);
        }
        if row.status != "active" || row.next_run_at.is_none_or(|at| at > now) {
            return Ok(None);
        }
        if row.calc_version != SCHEDULE_CALC_VERSION {
            return Err(ScheduleStoreError::Invalid);
        }
        let failures: FailureState = serde_json::from_str(&row.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if !failures.pause_reasons.is_empty() {
            return Ok(None);
        }
        let spec = parse_json(&row.spec_json).map_err(|_| ScheduleStoreError::Invalid)?;
        let first = row.next_run_at.ok_or(ScheduleStoreError::Invalid)?;
        let window = due_window(
            &spec,
            first.checked_sub(1).ok_or(ScheduleStoreError::Invalid)?,
            now,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
        let Some(latest) = window.latest_at else {
            return Ok(None);
        };
        let grace = u32::try_from(row.grace_seconds).map_err(|_| ScheduleStoreError::Invalid)?;
        let deadline = start_deadline(latest, grace).map_err(|_| ScheduleStoreError::Invalid)?;
        let status = if row.active_run_id.is_some() {
            "skipped_overlap"
        } else if now >= deadline {
            "missed"
        } else {
            "queued"
        };
        let identity = digest(&json(&(&row.schedule_id, "calendar", latest))?);
        let run_id = format!("schedule-run-{identity}");
        let conversation_id = match row.kind.as_str() {
            "fresh_task" => run_id.clone(),
            "conversation_resume" => row
                .source_conversation_id
                .clone()
                .ok_or(ScheduleStoreError::Invalid)?,
            _ => return Err(ScheduleStoreError::Invalid),
        };
        let revision = row
            .revision
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let terminal = status != "queued";
        let next_status = if window.next_at.is_some() {
            "active"
        } else if terminal {
            "completed"
        } else {
            "triggered"
        };
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(revision),
                next_run_at: Set(window.next_at),
                recurrence_cursor_at: Set(Some(latest)),
                status: Set(next_status.into()),
                active_run_id: Set(if terminal {
                    row.active_run_id.clone()
                } else {
                    Some(run_id.clone())
                }),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(row.id))
            .filter(entity::Column::Revision.eq(expected_revision))
            .filter(entity::Column::Status.eq("active"))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        run::Entity::insert(run::ActiveModel {
            run_id: Set(run_id.clone()),
            schedule_id: Set(row.schedule_id.clone()),
            owner_user_id: Set(row.owner_user_id),
            occurrence_identity: Set(identity),
            source: Set("calendar".into()),
            scheduled_at: Set(Some(latest)),
            requested_at: Set(now),
            schedule_revision: Set(row.revision),
            recovery_epoch: Set(
                i64::try_from(failures.recovery_epoch).map_err(|_| ScheduleStoreError::Invalid)?
            ),
            task_snapshot_json: Set(json(&row)?),
            conversation_id: Set(conversation_id),
            turn_id: Set(format!("{run_id}-turn")),
            status: Set(status.into()),
            start_deadline: Set(deadline),
            lease_epoch: Set(0),
            lease_owner: Set(None),
            lease_deadline: Set(None),
            attempt: Set(0),
            failure_accounted: Set(terminal),
            error_kind: Set(if status == "missed" {
                Some("misfire".into())
            } else {
                None
            }),
            result_ref: Set(None),
            missed_count: Set(
                i64::try_from(window.due_count - 1).map_err(|_| ScheduleStoreError::Invalid)?
            ),
            started_at: Set(None),
            finished_at: Set(terminal.then_some(now)),
            receipts_reconciled_at: Set(None),
            outcome_review_json: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec_without_returning(&txn)
        .await?;
        let result = run::Entity::find()
            .filter(run::Column::RunId.eq(&run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(Some(result))
    }

    /// A running or uncertain lease is never reclaimed by this entry point.
    pub async fn claim_queued(
        &self,
        run_id: &str,
        node_id: &str,
        lease_seconds: u32,
    ) -> Result<run::Model, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let result = Self::claim_queued_on(&txn, run_id, node_id, lease_seconds).await?;
        txn.commit().await?;
        Ok(result)
    }

    /// The caller must roll back on any error and commit only after pairing the
    /// claimed occurrence with its fresh session and current authorization.
    /// This function never commits and grants no permission to execute tools.
    pub async fn claim_queued_on(
        txn: &sea_orm::DatabaseTransaction,
        run_id: &str,
        node_id: &str,
        lease_seconds: u32,
    ) -> Result<run::Model, ScheduleStoreError> {
        if node_id.is_empty() || node_id.len() > 256 || !(30..=300).contains(&lease_seconds) {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = database_now(txn).await?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if !matches!(work.status.as_str(), "queued" | "waiting_device")
            || now >= work.start_deadline
            || work.started_at.is_some()
            || work.finished_at.is_some()
            || work.cancel_requested_at.is_some()
            || work.failure_accounted
            || work.attempt != 0
            || work.lease_epoch != 0
            || work.lease_owner.is_some()
            || work.lease_deadline.is_some()
            || work.error_kind.is_some()
            || work.result_ref.is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let schedule = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if schedule.kind != "fresh_task" {
            return Err(ScheduleStoreError::Invalid);
        }
        if work.conversation_id != work.run_id || work.turn_id != format!("{}-turn", work.run_id) {
            return Err(ScheduleStoreError::Conflict);
        }
        let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if work.owner_user_id != schedule.owner_user_id
            || snapshot.schedule_id != schedule.schedule_id
            || snapshot.owner_user_id != schedule.owner_user_id
            || snapshot.kind != schedule.kind
            || snapshot.revision != work.schedule_revision
            || snapshot.task_revision != schedule.task_revision
            || snapshot.target_device_id != schedule.target_device_id
            || snapshot.prompt != schedule.prompt
            || snapshot.locale != schedule.locale
            || snapshot.model_id != schedule.model_id
            || snapshot.contract_revision != schedule.contract_revision
            || snapshot.authorization_revision != schedule.authorization_revision
            || schedule.calc_version != SCHEDULE_CALC_VERSION
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let failures: FailureState = serde_json::from_str(&schedule.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if !matches!(schedule.status.as_str(), "active" | "triggered")
            || !failures.pause_reasons.is_empty()
            || schedule.active_run_id.as_deref() != Some(run_id)
            || i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let touched = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(schedule
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(schedule.id))
            .filter(entity::Column::Revision.eq(schedule.revision))
            .exec(txn)
            .await?;
        if touched.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        // Waiting for the task fence can outlive the start window. Re-read
        // the occurrence as cancellation and settlement may have changed it.
        let now = super::authority::authority_now(txn).await?;
        if now >= work.start_deadline
            || run::Entity::find_by_id(work.id).one(txn).await?.as_ref() != Some(&work)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("running".into()),
                lease_owner: Set(Some(node_id.into())),
                lease_epoch: Set(work
                    .lease_epoch
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                lease_deadline: Set(Some(
                    start_deadline(now, lease_seconds).map_err(|_| ScheduleStoreError::Invalid)?,
                )),
                attempt: Set(work
                    .attempt
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                started_at: Set(Some(now)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::Status.eq(&work.status))
            .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = run::Entity::find_by_id(work.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        Ok(result)
    }

    pub async fn renew_run(
        &self,
        run_id: &str,
        node: &str,
        epoch: i64,
        lease_seconds: u32,
    ) -> Result<bool, ScheduleStoreError> {
        if !(30..=300).contains(&lease_seconds) {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = database_now(&self.db).await?;
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                lease_deadline: Set(Some(
                    start_deadline(now, lease_seconds).map_err(|_| ScheduleStoreError::Invalid)?,
                )),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::RunId.eq(run_id))
            .filter(run::Column::LeaseOwner.eq(node))
            .filter(run::Column::LeaseEpoch.eq(epoch))
            .filter(run::Column::Status.eq("running"))
            .filter(run::Column::LeaseDeadline.gt(now))
            .exec(&self.db)
            .await?;
        Ok(changed.rows_affected == 1)
    }
}
