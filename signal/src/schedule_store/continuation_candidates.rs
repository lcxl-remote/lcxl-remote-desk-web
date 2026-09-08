//! Advisory dispatcher candidates; only a paired claim can authorize execution.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter, QueryOrder, QuerySelect,
};

impl ScheduleStore {
    /// Recovery scans never requeue a started occurrence or grant a new lease.
    pub async fn expired_continuation_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        if after_id < 0 || !(1..=128).contains(&limit) {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = self.database_time().await?;
        let tasks = Query::select()
            .column(entity::Column::ActiveRunId)
            .from(entity::Entity)
            .and_where(entity::Column::Kind.eq("conversation_resume"))
            .and_where(
                Expr::col((entity::Entity, entity::Column::OwnerUserId))
                    .equals((run::Entity, run::Column::OwnerUserId)),
            )
            .to_owned();
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after_id))
            .filter(run::Column::RunId.in_subquery(tasks))
            .filter(run::Column::OwnerUserId.gt(0))
            .filter(run::Column::Status.eq("running"))
            .filter(run::Column::StartedAt.is_not_null())
            .filter(run::Column::LeaseOwner.is_not_null())
            .filter(run::Column::LeaseDeadline.lte(now))
            .filter(run::Column::LeaseEpoch.gt(0))
            .filter(run::Column::Attempt.eq(1))
            .filter(run::Column::FinishedAt.is_null())
            .filter(run::Column::FailureAccounted.eq(false))
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    /// Started tasks are eligible only through their dedicated approval claim.
    pub async fn fresh_task_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        if after_id < 0 || !(1..=128).contains(&limit) {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = self.database_time().await?;
        let tasks = Query::select()
            .column(entity::Column::ActiveRunId)
            .from(entity::Entity)
            .and_where(entity::Column::Kind.eq("fresh_task"))
            .and_where(entity::Column::Status.is_in(["active", "triggered", "paused"]))
            .and_where(
                Expr::col((entity::Entity, entity::Column::OwnerUserId))
                    .equals((run::Entity, run::Column::OwnerUserId)),
            )
            .to_owned();
        let initial = Condition::all()
            .add(run::Column::Status.is_in(["queued", "waiting_device"]))
            .add(run::Column::StartedAt.is_null())
            .add(run::Column::LeaseOwner.is_null())
            .add(run::Column::LeaseEpoch.eq(0))
            .add(run::Column::Attempt.eq(0))
            .add(run::Column::StartDeadline.gt(now));
        let approval = Condition::all()
            .add(run::Column::Status.eq("awaiting_permission"))
            .add(run::Column::StartedAt.is_not_null())
            .add(run::Column::LeaseEpoch.gt(0))
            .add(run::Column::Attempt.eq(1))
            .add(
                Condition::any()
                    .add(run::Column::ResultRef.starts_with("permission:"))
                    .add(run::Column::ResultRef.starts_with("directory:")),
            );
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after_id))
            .filter(run::Column::RunId.in_subquery(tasks))
            .filter(run::Column::OwnerUserId.gt(0))
            .filter(run::Column::LeaseDeadline.is_null())
            .filter(Condition::any().add(initial).add(approval))
            .filter(run::Column::CancelRequestedAt.is_null())
            .filter(run::Column::FinishedAt.is_null())
            .filter(run::Column::FailureAccounted.eq(false))
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    /// Scan all owners with a stable keyset cursor. Never reclaim started work:
    /// an approval wait is eligible only for its dedicated decision claim.
    /// This read neither takes a lease nor replaces live subject/policy checks.
    pub async fn continuation_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        if after_id < 0 || !(1..=128).contains(&limit) {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = self.database_time().await?;
        let tasks = Query::select()
            .column(entity::Column::ActiveRunId)
            .from(entity::Entity)
            .and_where(entity::Column::Kind.eq("conversation_resume"))
            .and_where(entity::Column::Status.is_in(["active", "triggered", "paused"]))
            .and_where(
                Expr::col((entity::Entity, entity::Column::OwnerUserId))
                    .equals((run::Entity, run::Column::OwnerUserId)),
            )
            .to_owned();
        let initial = Condition::all()
            .add(run::Column::Status.is_in(["queued", "waiting_device"]))
            .add(run::Column::StartedAt.is_null())
            .add(run::Column::LeaseOwner.is_null())
            .add(run::Column::LeaseEpoch.eq(0))
            .add(run::Column::Attempt.eq(0))
            .add(run::Column::StartDeadline.gt(now));
        let approval = Condition::all()
            .add(run::Column::Status.eq("awaiting_permission"))
            .add(run::Column::StartedAt.is_not_null())
            .add(run::Column::LeaseEpoch.gt(0))
            .add(run::Column::Attempt.eq(1))
            .add(run::Column::ResultRef.starts_with("permission:"));
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after_id))
            .filter(run::Column::OwnerUserId.gt(0))
            .filter(run::Column::RunId.in_subquery(tasks))
            .filter(run::Column::CancelRequestedAt.is_null())
            .filter(run::Column::FinishedAt.is_null())
            .filter(run::Column::FailureAccounted.eq(false))
            .filter(run::Column::LeaseDeadline.is_null())
            .filter(Condition::any().add(initial).add(approval))
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }
}

#[cfg(test)]
mod tests;
