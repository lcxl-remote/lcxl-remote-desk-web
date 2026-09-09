//! Explicit recovery revalidates current authority before starting a new epoch.
use super::queue::database_now;
use super::{ScheduleStore, ScheduleStoreError, entity, json};
use desk_agent_protocol::schedule::SchedulePauseReason;
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION, lifecycle::FailureState, next_after, parse_json, validate_publication,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set};

/// Runtime policy adapter checks account, device, contract and session fences.
/// No wire payload or model response can implement this server-owned interface.
#[async_trait::async_trait(?Send)]
pub trait ScheduleAuthorizer {
    async fn authorize(
        &self,
        transaction: &DatabaseTransaction,
        task: &entity::Model,
    ) -> Result<(), ScheduleStoreError>;
}

impl ScheduleStore {
    pub async fn resume(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        authorizer: &dyn ScheduleAuthorizer,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let now = database_now(&txn).await?;
        let row = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.revision != expected || row.status != "paused" || row.active_run_id.is_some() {
            return Err(ScheduleStoreError::Conflict);
        }
        let mut failures: FailureState = serde_json::from_str(&row.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if failures
            .pause_reasons
            .contains(&SchedulePauseReason::UnknownSideEffect)
            || failures
                .pause_reasons
                .contains(&SchedulePauseReason::ScheduleUpgradeRequired)
            || row.calc_version != SCHEDULE_CALC_VERSION
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let spec = parse_json(&row.spec_json).map_err(|_| ScheduleStoreError::Invalid)?;
        let spec = validate_publication(&spec, now, row.kind == "conversation_resume")
            .map_err(|_| ScheduleStoreError::Invalid)?;
        authorizer.authorize(&txn, &row).await?;
        failures
            .pause_reasons
            .remove(&SchedulePauseReason::AuthorizationInvalid);
        failures.resume().map_err(|_| ScheduleStoreError::Invalid)?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                status: Set("active".into()),
                failure_state_json: Set(json(&failures)?),
                next_run_at: Set(next_after(&spec, now).map_err(|_| ScheduleStoreError::Invalid)?),
                recurrence_cursor_at: Set(Some(now)),
                revision: Set(expected.checked_add(1).ok_or(ScheduleStoreError::Invalid)?),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(row.id))
            .filter(entity::Column::Revision.eq(expected))
            .filter(entity::Column::ActiveRunId.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = entity::Entity::find_by_id(row.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(result)
    }
}
