//! Derive source timing under the caller's control fences without taking an early Goal lock.
use super::*;
use crate::entity::agent_goal_run as goal_row;

pub(super) fn current_group_on<'a, C: ConnectionTrait + crate::config::ConfigConnection + 'a>(
    db: &'a C,
    row: &'a group_row::Model,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Box<DelegationGroup>, DbErr>> + Send + 'a>,
> {
    let mut inputs = Some(Box::new((db, row)));
    desk_diagnose_core::future::send_phase(|| {
        current_group_inner(inputs.take().expect("immediate phase factory"))
    })
}

pub(super) async fn current_group_inner<C: ConnectionTrait + crate::config::ConfigConnection>(
    inputs: Box<(&C, &group_row::Model)>,
) -> Result<Box<DelegationGroup>, DbErr> {
    let (db, row) = *inputs;
    let mut group = Box::new(decode_group(row)?);
    let Some(goal_id) = group.source.goal_id() else {
        return Ok(group);
    };
    if group.source_admission == SourceAdmission::Closed {
        return Ok(group);
    }
    // This projection is not a new reservation or authority. The funding path
    // rechecks the source at its original locking point before committing I/O.
    let goal = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(goal_id))
        .filter(goal_row::Column::ConversationId.eq(&group.root_conversation_id))
        .filter(goal_row::Column::ActorId.eq(&group.actor_id))
        .filter(goal_row::Column::DeviceId.eq(&group.device_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let mut goal = crate::agent_goal_store::decode(&goal)?;
    goal.apply_budget_policy(&crate::goal_budget_policy::read(db).await?)
        .map_err(|_| invalid())?;
    group.limits.deadline_ms = i64::try_from(goal.deadline_unix_ms).map_err(|_| invalid())?;
    group.validate().map_err(|_| invalid())?;
    Ok(group)
}
