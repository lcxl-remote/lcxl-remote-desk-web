//! Owner task rows: reads, the paged list and the user pause.
use super::{ScheduleStoreError, json};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_diagnose_core::schedule::lifecycle::FailureState;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};

/// Pausing scheduling never pretends to cancel an already dispatched action.
pub async fn pause_on<C: ConnectionTrait>(
    txn: &C,
    owner: i32,
    schedule_id: &str,
    expected_revision: i64,
    now_ms: i64,
) -> Result<(), ScheduleStoreError> {
    let row = entity::Entity::find()
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if row.revision != expected_revision {
        return Err(ScheduleStoreError::Conflict);
    }
    if !matches!(row.status.as_str(), "active" | "triggered" | "paused") || now_ms < 0 {
        return Err(ScheduleStoreError::Invalid);
    }
    let mut failures: FailureState =
        serde_json::from_str(&row.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    failures
        .pause_reasons
        .insert(desk_agent_protocol::schedule::SchedulePauseReason::User);
    let revision = row
        .revision
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let pending = if let Some(run_id) = row.active_run_id.as_deref() {
        run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(txn)
            .await?
            .filter(|work| matches!(work.status.as_str(), "queued" | "waiting_device"))
    } else {
        None
    };
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            status: Set("paused".into()),
            active_run_id: Set(if pending.is_some() {
                None
            } else {
                row.active_run_id.clone()
            }),
            next_run_at: Set(None),
            revision: Set(revision),
            failure_state_json: Set(json(&failures)?),
            updated_at: Set(now_ms),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(row.id))
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::Revision.eq(expected_revision))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    if let Some(work) = pending {
        let cancelled = run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("cancelled".into()),
                cancel_requested_at: Set(Some(now_ms)),
                failure_accounted: Set(true),
                finished_at: Set(Some(now_ms)),
                updated_at: Set(now_ms),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::Status.eq(work.status))
            .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
            .exec(txn)
            .await?;
        if cancelled.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    Ok(())
}

pub async fn read<C: ConnectionTrait>(
    db: &C,
    owner: i32,
    schedule_id: &str,
) -> Result<entity::Model, ScheduleStoreError> {
    entity::Entity::find()
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)
}

/// The stable primary-key cursor prevents duplicate rows during concurrent inserts.
pub async fn list<C: ConnectionTrait>(
    db: &C,
    owner: i32,
    after_id: i64,
    limit: u64,
) -> Result<Vec<entity::Model>, ScheduleStoreError> {
    if owner <= 0 || after_id < 0 || limit == 0 || limit > 100 {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok(entity::Entity::find()
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::Id.gt(after_id))
        .filter(entity::Column::Status.ne("deleted"))
        .order_by_asc(entity::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}
