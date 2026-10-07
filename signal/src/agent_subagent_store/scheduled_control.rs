//! Original occurrence termination closes children without deleting their evidence.
use super::*;

impl SubAgentStore {
    /// Select only withdrawn or expired original occurrences before LIMIT, so
    /// long-lived healthy sources cannot starve cleanup on another instance.
    pub(crate) async fn withdrawn_scheduled_source_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<group_row::Model>, DbErr> {
        use crate::entity::agent_schedule_run as occurrence;
        use sea_orm::{Condition, QueryTrait};
        if after_id < 0 || !(1..=32).contains(&limit) {
            return Err(invalid());
        }
        let withdrawn = occurrence::Entity::find()
            .select_only()
            .column(occurrence::Column::RunId)
            .filter(
                Condition::any()
                    .add(occurrence::Column::CancelRequestedAt.is_not_null())
                    .add(occurrence::Column::FinishedAt.is_not_null())
                    .add(occurrence::Column::FailureAccounted.eq(true)),
            )
            .into_query();
        group_row::Entity::find()
            .filter(group_row::Column::Id.gt(after_id))
            .filter(group_row::Column::SourceScheduleId.is_not_null())
            .filter(group_row::Column::SourceOccurrenceId.is_not_null())
            .filter(group_row::Column::SourceAdmission.ne("closed"))
            .filter(
                Condition::any()
                    .add(group_row::Column::SourceOccurrenceId.in_subquery(withdrawn))
                    .add(group_row::Column::DeadlineMs.lte(chrono::Utc::now().timestamp_millis())),
            )
            .order_by_asc(group_row::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
    }

    /// A scheduler invokes this after durably withdrawing the original source.
    /// Locks preserve native cancellation identity; no completion or exit is inferred.
    pub(crate) async fn close_scheduled_source(
        &self,
        root: &str,
        actor: &str,
        device: &str,
    ) -> Result<usize, DbErr> {
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        let rows = group_row::Entity::find()
            .filter(group_row::Column::RootConversationId.eq(root))
            .filter(group_row::Column::SourceOccurrenceId.eq(root))
            .filter(group_row::Column::ActorId.eq(actor))
            .filter(group_row::Column::DeviceId.eq(device))
            .order_by_asc(group_row::Column::Id)
            .limit(2)
            .all(&txn)
            .await?;
        // An isolated original occurrence opens one source group, never a new
        // allocation for each continuation or each child.
        if rows.len() > 1 {
            return Err(invalid());
        }
        let now = chrono::Utc::now();
        let mut closed = 0;
        for row in rows {
            super::scheduled_budget::lock_historical_on(&txn, &row).await?;
            let group = decode_group(&row)?;
            let work = crate::entity::agent_schedule_run::Entity::find()
                .filter(crate::entity::agent_schedule_run::Column::RunId.eq(root))
                .filter(
                    crate::entity::agent_schedule_run::Column::OwnerUserId
                        .eq(actor.parse::<i32>().map_err(|_| invalid())?),
                )
                .one(&txn)
                .await?
                .ok_or_else(invalid)?;
            let source_withdrawn = work.cancel_requested_at.is_some()
                || work.failure_accounted
                || work.finished_at.is_some()
                || now.timestamp_millis() >= group.limits.deadline_ms;
            if !source_withdrawn {
                return Err(invalid());
            }
            closed += close_scheduled_group_on(&txn, &row, now.timestamp_millis()).await?;
        }
        txn.commit().await?;
        Ok(closed)
    }
}

/// Trusted scheduler cleanup only. The caller holds owner/root/child control and
/// the original publication fence, and has independently established withdrawal.
/// No model-visible control tool can invoke this historical settlement helper.
pub(crate) async fn close_scheduled_group_on(
    txn: &sea_orm::DatabaseTransaction,
    row: &group_row::Model,
    now_ms: i64,
) -> Result<usize, DbErr> {
    let mut group = decode_group(row)?;
    if !matches!(&group.source, desk_diagnose_core::subagent::DelegationSource::ScheduledOccurrence {
        schedule_id, occurrence_id,
    } if row.source_schedule_id.as_deref() == Some(schedule_id.as_str())
        && occurrence_id == &group.root_conversation_id)
    {
        return Err(invalid());
    }
    let now = chrono::DateTime::from_timestamp_millis(now_ms).ok_or_else(invalid)?;
    if group.source_admission != SourceAdmission::Closed {
        group.stop_parent().map_err(|_| invalid())?;
        group
            .set_source_admission(SourceAdmission::Closed)
            .map_err(|_| invalid())?;
        replace_group_on(txn, row, &group, now_ms).await?;
    }
    let children = super::group_children_on(txn, &group).await?;
    let mut closed = 0;
    for child in children {
        let mut run = decode_run(&child)?;
        if run.state.is_terminal() {
            continue;
        }
        run.request_cancel(run.fence(), &now.to_rfc3339())
            .map_err(|_| invalid())?;
        run.binding.source_epoch = group.source_epoch;
        run.settle_cancel(&now.to_rfc3339())
            .map_err(|_| invalid())?;
        replace_run_on(txn, &child, &run, now_ms).await?;
        synchronize_control_on(txn, &run, now_ms).await?;
        append_state_event_on(txn, &group, &run, now_ms).await?;
        closed += 1;
    }
    Ok(closed)
}
