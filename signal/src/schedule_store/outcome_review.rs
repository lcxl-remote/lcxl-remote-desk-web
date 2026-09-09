//! Owner acknowledgement of reconciled history, without resuming or dispatching work.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
mod manual;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeReview {
    schema_version: u16,
    owner: i32,
    run_id: String,
    client_request_key: String,
    note: String,
    reviewed_at: i64,
    receipts_reconciled_at: Option<i64>,
    manual_disposition: Option<desk_diagnose_core::session::ManualOutcomeDisposition>,
}

impl ScheduleStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn acknowledge_run_outcome(
        &self,
        owner: i32,
        schedule_id: &str,
        run_id: &str,
        expected_revision: i64,
        key: &str,
        note: &str,
    ) -> Result<(), ScheduleStoreError> {
        self.review_run_outcome(
            owner,
            schedule_id,
            run_id,
            expected_revision,
            key,
            note,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn dispose_run_outcome(
        &self,
        owner: i32,
        schedule_id: &str,
        run_id: &str,
        expected_revision: i64,
        key: &str,
        note: &str,
        work_id: i64,
        execution_id: &str,
    ) -> Result<(), ScheduleStoreError> {
        if work_id <= 0 || execution_id.is_empty() || execution_id.len() > 256 {
            return Err(ScheduleStoreError::Invalid);
        }
        self.review_run_outcome(
            owner,
            schedule_id,
            run_id,
            expected_revision,
            key,
            note,
            Some((work_id, execution_id)),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn review_run_outcome(
        &self,
        owner: i32,
        schedule_id: &str,
        run_id: &str,
        expected_revision: i64,
        key: &str,
        note: &str,
        disposition: Option<(i64, &str)>,
    ) -> Result<(), ScheduleStoreError> {
        if owner <= 0
            || key.is_empty()
            || key.len() > 128
            || key.chars().any(char::is_control)
            || note.trim().is_empty()
            || note.len() > 2048
            || note.chars().any(|c| c.is_control() && c != '\n')
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .filter(run::Column::ScheduleId.eq(schedule_id))
            .filter(run::Column::OwnerUserId.eq(owner))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if let Some(raw) = &work.outcome_review_json {
            let old: OutcomeReview =
                serde_json::from_str(raw).map_err(|_| ScheduleStoreError::Invalid)?;
            if old.schema_version != 1
                || old.owner != owner
                || old.run_id != run_id
                || old.client_request_key != key
                || old.note != note
            {
                return Err(ScheduleStoreError::Conflict);
            }
            if let Some((work_id, execution_id)) = disposition
                && !old.manual_disposition.as_ref().is_some_and(|value| {
                    value.action.work_id == work_id && value.action.execution_id == execution_id
                })
            {
                return Err(ScheduleStoreError::Conflict);
            }
            txn.commit().await?;
            return Ok(());
        }
        if task.kind != "fresh_task"
            || task.status != "paused"
            || task.active_run_id.is_some()
            || task.revision != expected_revision
            || work.status != "outcome_unknown"
            || !work.failure_accounted
            || work.finished_at.is_none()
            || work.lease_deadline.is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let reconciled = work.receipts_reconciled_at;
        let now = super::authority::authority_now(&txn).await?;
        if reconciled.is_some_and(|value| value > now) {
            return Err(ScheduleStoreError::Conflict);
        }
        if disposition.is_some() && reconciled.is_some() {
            return Err(ScheduleStoreError::Conflict);
        }
        let manual_disposition = if reconciled.is_none() {
            Some(manual::evidence(&txn, &task, &work, now, disposition).await?)
        } else {
            None
        };
        let review = OutcomeReview {
            schema_version: 1,
            owner,
            run_id: run_id.into(),
            client_request_key: key.into(),
            note: note.into(),
            reviewed_at: now,
            receipts_reconciled_at: reconciled,
            manual_disposition,
        };
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                outcome_review_json: Set(Some(
                    serde_json::to_string(&review).map_err(|_| ScheduleStoreError::Invalid)?,
                )),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::OutcomeReviewJson.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let unresolved = run::Entity::find()
            .filter(run::Column::ScheduleId.eq(schedule_id))
            .filter(run::Column::OwnerUserId.eq(owner))
            .filter(run::Column::Status.eq("outcome_unknown"))
            .filter(run::Column::OutcomeReviewJson.is_null())
            .one(&txn)
            .await?
            .is_some();
        let mut failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
        if !unresolved {
            failures
                .pause_reasons
                .remove(&desk_agent_protocol::schedule::SchedulePauseReason::UnknownSideEffect);
        }
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                failure_state_json: Set(
                    serde_json::to_string(&failures).map_err(|_| ScheduleStoreError::Invalid)?
                ),
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        txn.commit().await?;
        Ok(())
    }
}

pub(crate) fn reviewed_at(raw: Option<&str>) -> Result<Option<i64>, ScheduleStoreError> {
    raw.map(|raw| {
        let value: OutcomeReview =
            serde_json::from_str(raw).map_err(|_| ScheduleStoreError::Invalid)?;
        let evidence_at = match (&value.receipts_reconciled_at, &value.manual_disposition) {
            (Some(at), None) => *at,
            (None, Some(disposition)) => {
                chrono::DateTime::parse_from_rfc3339(&disposition.disposed_at)
                    .map_err(|_| ScheduleStoreError::Invalid)?
                    .timestamp_millis()
            }
            _ => return Err(ScheduleStoreError::Invalid),
        };
        if value.schema_version != 1 || value.reviewed_at < evidence_at {
            return Err(ScheduleStoreError::Invalid);
        }
        Ok(value.reviewed_at)
    })
    .transpose()
}
