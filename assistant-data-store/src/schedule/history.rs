//! Owner-scoped, newest-first occurrence history for management projections.
use super::{ScheduleStoreError, continuation_subject, task::read};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

/// Internal rows must be projected before transport; snapshots and leases are private.
pub struct RunHistoryPage {
    pub runs: Vec<run::Model>,
    pub next_cursor: Option<String>,
}

/// Resolve a public occurrence selector. The snapshot reader must still
/// verify current account/device access and the stored session subject.
/// The turn rotates after permission decisions; the original conversation does not.
pub async fn resolve_run_session<C: ConnectionTrait>(
    db: &C,
    owner: i32,
    schedule_id: &str,
    run_id: &str,
) -> Result<(String, String), ScheduleStoreError> {
    let task = read(db, owner, schedule_id).await?;
    let work = run::Entity::find()
        .filter(run::Column::OwnerUserId.eq(owner))
        .filter(run::Column::ScheduleId.eq(schedule_id))
        .filter(run::Column::RunId.eq(run_id))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let frozen: entity::Model =
        serde_json::from_str(&work.task_snapshot_json).map_err(|_| ScheduleStoreError::NotFound)?;
    if owner <= 0
        || work.started_at.is_none()
        || frozen.owner_user_id != owner
        || frozen.schedule_id != schedule_id
        || frozen.target_device_id != task.target_device_id
        || frozen.kind != task.kind
        || frozen.source_conversation_id != task.source_conversation_id
        || work.turn_id.is_empty()
        || match frozen.kind.as_str() {
            "fresh_task" => work.conversation_id != run_id,
            "conversation_resume" => {
                frozen.source_conversation_id.as_deref() != Some(work.conversation_id.as_str())
            }
            _ => true,
        }
    {
        return Err(ScheduleStoreError::NotFound);
    }
    let subject = if frozen.kind == "conversation_resume" {
        continuation_subject(&frozen)?
    } else {
        frozen.target_device_id
    };
    Ok((work.conversation_id, subject))
}

pub async fn run_history<C: ConnectionTrait>(
    db: &C,
    owner: i32,
    schedule_id: &str,
    before_run_id: Option<&str>,
    limit: u32,
) -> Result<RunHistoryPage, ScheduleStoreError> {
    if owner <= 0 || !(1..=100).contains(&limit) {
        return Err(ScheduleStoreError::Invalid);
    }
    read(db, owner, schedule_id).await?;
    let base = run::Entity::find()
        .filter(run::Column::OwnerUserId.eq(owner))
        .filter(run::Column::ScheduleId.eq(schedule_id));
    let mut query = base.clone();
    if let Some(cursor) = before_run_id {
        let previous = base
            .filter(run::Column::RunId.eq(cursor))
            .one(db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        query = query.filter(run::Column::Id.lt(previous.id));
    }
    let mut rows = query
        .order_by_desc(run::Column::Id)
        .limit(u64::from(limit) + 1)
        .all(db)
        .await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = if more {
        rows.last().map(|row| row.run_id.clone())
    } else {
        None
    };
    Ok(RunHistoryPage {
        runs: rows,
        next_cursor,
    })
}
