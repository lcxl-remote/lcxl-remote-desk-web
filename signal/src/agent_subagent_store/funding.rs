//! Reserve the source goal and allocation group in one host transaction.
use super::*;
use crate::entity::agent_goal_run as goal_row;
use desk_diagnose_core::{
    goal::{GoalRun, GoalUsage},
    subagent::{
        budget::Usage,
        reservation::{DelegationCallKind, DelegationCallReservation},
    },
};
use sea_orm::{DatabaseTransaction, QuerySelect};

async fn source_goal_on(
    txn: &DatabaseTransaction,
    group: &DelegationGroup,
) -> Result<goal_row::Model, DbErr> {
    let id = group.source.goal_id().ok_or_else(invalid)?;
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(id))
        .filter(goal_row::Column::ConversationId.eq(&group.root_conversation_id))
        .filter(goal_row::Column::DeviceId.eq(&group.device_id))
        .filter(goal_row::Column::ActorId.eq(&group.actor_id))
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let goal = crate::agent_goal_store::decode(&row)?;
    if goal.owner_id != group.actor_id
        || goal.device_id != group.device_id
        || goal.conversation_id != group.root_conversation_id
    {
        return Err(invalid());
    }
    Ok(row)
}

async fn save_source_goal_on(
    txn: &DatabaseTransaction,
    old: &goal_row::Model,
    goal: &GoalRun,
) -> Result<(), DbErr> {
    if !crate::agent_goal_store::replace_on(
        txn,
        goal,
        u64::try_from(old.state_version).map_err(|_| invalid())?,
        u64::try_from(old.lease_epoch).map_err(|_| invalid())?,
        old.lease_owner.as_deref(),
        old.lease_deadline
            .map(u64::try_from)
            .transpose()
            .map_err(|_| invalid())?,
    )
    .await?
    {
        return Err(invalid());
    }
    Ok(())
}

/// Errors and Exhausted roll back the caller's whole transaction. The owner/root/child locks
/// are already held; goal rows are acquired after group/task/session authority.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reserve_call_budget_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    logical_id: &str,
    kind: DelegationCallKind,
    arguments_sha256: &str,
    upper: Usage,
    goal_upper: Option<GoalUsage>,
    now_ms: i64,
) -> Result<BudgetAdmission, DbErr> {
    reserve_call_with_authority_on(
        txn,
        session,
        logical_id,
        kind,
        arguments_sha256,
        upper,
        goal_upper,
        None,
        now_ms,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn reserve_call_with_authority_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    logical_id: &str,
    kind: DelegationCallKind,
    arguments_sha256: &str,
    upper: Usage,
    goal_upper: Option<GoalUsage>,
    review_authority: Option<&desk_diagnose_core::subagent::reservation::ReviewCallAuthority>,
    now_ms: i64,
) -> Result<BudgetAdmission, DbErr> {
    let group_id = session.delegation_group_id.as_deref().ok_or_else(invalid)?;
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(group_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&row)?;
    let scheduled = super::scheduled_source::current_scheduled_source_on(txn, &row).await?;
    if group.source.goal_id().is_some() != goal_upper.is_some()
        || goal_upper.is_some_and(|goal| {
            goal.slices != 0
                || u64::from(goal.model_calls) != upper.model_calls
                || u64::from(goal.tool_calls) != upper.tool_calls
                || goal.total_tokens() != Some(upper.tokens)
        })
    {
        return Err(invalid());
    }
    let admitted = budget::reserve_budget_with_authority_on(
        txn,
        session,
        logical_id,
        kind,
        arguments_sha256,
        upper,
        review_authority,
        now_ms,
    )
    .await?;
    let (mut reservation, actual) = match &admitted {
        BudgetAdmission::Exhausted => return Ok(BudgetAdmission::Exhausted),
        BudgetAdmission::Reserved(reservation) | BudgetAdmission::AlreadyReserved(reservation) => {
            (reservation.clone(), None)
        }
        BudgetAdmission::Settled(reservation, actual) => (reservation.clone(), Some(*actual)),
    };
    if !super::scheduled_budget::reserve_on(
        txn,
        scheduled.as_ref(),
        &reservation,
        matches!(&admitted, BudgetAdmission::Reserved(_)),
    )
    .await?
    {
        return Ok(BudgetAdmission::Exhausted);
    }
    let Some(goal_upper) = goal_upper else {
        if reservation.source_goal_upper.is_some() {
            return Err(invalid());
        }
        return Ok(admitted);
    };
    let old = source_goal_on(txn, &group).await?;
    let mut goal = crate::agent_goal_store::decode(&old)?;
    if !matches!(&admitted, BudgetAdmission::Reserved(_)) {
        if reservation.source_goal_upper != Some(goal_upper) {
            return Err(invalid());
        }
        match actual {
            Some(actual) => {
                let recorded = goal
                    .delegation_settlements
                    .get(&reservation.reservation_id)
                    .ok_or_else(invalid)?;
                if recorded.total_tokens() != Some(actual.tokens)
                    || u64::from(recorded.model_calls) != actual.model_calls
                    || u64::from(recorded.tool_calls) != actual.tool_calls
                {
                    return Err(invalid());
                }
            }
            None if goal
                .delegation_reservations
                .get(&reservation.reservation_id)
                == Some(&goal_upper) => {}
            None => return Err(invalid()),
        }
        return Ok(admitted);
    }
    goal.apply_budget_policy(&crate::goal_budget_policy::read(txn).await?)
        .map_err(|_| invalid())?;
    let now = u64::try_from(now_ms)
        .map_err(|_| invalid())?
        .max(goal.updated_at_unix_ms);
    match goal.reserve_delegation_with_id(&reservation.reservation_id, goal_upper, now) {
        Ok(_) => {}
        Err(
            desk_diagnose_core::goal::GoalError::BudgetExceeded
            | desk_diagnose_core::goal::GoalError::DeadlineReached,
        ) => {
            // The caller rolls back the allocation reservation already written.
            return Ok(BudgetAdmission::Exhausted);
        }
        Err(_) => return Err(invalid()),
    }
    save_source_goal_on(txn, &old, &goal).await?;
    reservation.source_goal_upper = Some(goal_upper);
    reservation.validate().map_err(|_| invalid())?;
    let changed = crate::entity::agent_delegation_reservation::Entity::update_many()
        .set(crate::entity::agent_delegation_reservation::ActiveModel {
            reservation_json: Set(serde_json::to_string(&reservation).map_err(|_| invalid())?),
            version: Set(2),
            ..Default::default()
        })
        .filter(
            crate::entity::agent_delegation_reservation::Column::ReservationId
                .eq(&reservation.reservation_id),
        )
        .filter(crate::entity::agent_delegation_reservation::Column::Version.eq(1))
        .filter(crate::entity::agent_delegation_reservation::Column::State.eq("reserved"))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(BudgetAdmission::Reserved(reservation))
}

/// Settlement needs the original admitted identity, never the current planner
/// lease. Unknown usage leaves both constraints outstanding; known usage updates
/// both together and does not re-open a paused or cancelled source.
pub(crate) async fn settle_call_budget_on(
    txn: &DatabaseTransaction,
    reservation: &DelegationCallReservation,
    actual: Option<Usage>,
    actual_goal: Option<GoalUsage>,
    now_ms: i64,
) -> Result<(), DbErr> {
    let old = crate::entity::agent_delegation_reservation::Entity::find()
        .filter(
            crate::entity::agent_delegation_reservation::Column::ReservationId
                .eq(&reservation.reservation_id),
        )
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if budget::decode_reservation(&old)? != *reservation
        || actual.is_none() && actual_goal.is_some()
        || actual.is_some() && (reservation.source_goal_upper.is_some() != actual_goal.is_some())
    {
        return Err(invalid());
    }
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&reservation.group_id))
        .filter(group_row::Column::RootConversationId.eq(&reservation.root_conversation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    super::scheduled_budget::lock_historical_on(txn, &source).await?;
    super::scheduled_budget::settle_on(txn, &source, &old, reservation, actual).await?;
    if let (Some(actual), Some(goal_actual)) = (actual, actual_goal) {
        if goal_actual.slices != 0
            || goal_actual.total_tokens() != Some(actual.tokens)
            || u64::from(goal_actual.model_calls) != actual.model_calls
            || u64::from(goal_actual.tool_calls) != actual.tool_calls
        {
            return Err(invalid());
        }
        let row = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&reservation.group_id))
            .filter(group_row::Column::RootConversationId.eq(&reservation.root_conversation_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let group = decode_group(&row)?;
        let old_goal = source_goal_on(txn, &group).await?;
        let mut goal = crate::agent_goal_store::decode(&old_goal)?;
        let now = u64::try_from(now_ms)
            .map_err(|_| invalid())?
            .max(goal.updated_at_unix_ms);
        let changed = if actual == Usage::default()
            && reservation.kind != DelegationCallKind::Tool
            && old.provider_receipt_id.is_none()
        {
            goal.release_unstarted_delegation_model_with_id(&reservation.reservation_id, now)
        } else {
            goal.settle_delegation_with_id(&reservation.reservation_id, goal_actual, now)
        }
        .map_err(|_| invalid())?;
        if changed {
            save_source_goal_on(txn, &old_goal, &goal).await?;
        }
    }
    settle_budget_on(txn, reservation, actual, now_ms).await
}
