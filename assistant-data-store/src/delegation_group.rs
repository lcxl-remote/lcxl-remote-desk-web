//! Delegation group rows: decoded only when the stored columns agree with the
//! group state, with all of a group's child runs read in bounded pages.
use crate::entity::{agent_delegation_group as group_row, agent_subagent_run as run_row};
use desk_diagnose_core::subagent::group::{DelegationGroup, SourceAdmission};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
};

fn invalid() -> DbErr {
    DbErr::Custom("delegated task is unavailable or its control state changed".into())
}

#[inline(never)]
pub fn decode_group(row: &group_row::Model) -> Result<DelegationGroup, DbErr> {
    let group: DelegationGroup = serde_json::from_str(&row.state_json).map_err(|_| invalid())?;
    group.validate().map_err(|_| invalid())?;
    if group.group_id != row.group_id
        || group.root_conversation_id != row.root_conversation_id
        || group.actor_id != row.actor_id
        || group.device_id != row.device_id
        || group.source.workbench_id() != row.source_workbench_id.as_deref()
        || group.source.goal_id() != row.source_goal_id.as_deref()
        || match &group.source {
            desk_diagnose_core::subagent::DelegationSource::ScheduledOccurrence {
                schedule_id,
                occurrence_id,
            } => {
                row.source_schedule_id.as_deref() != Some(schedule_id.as_str())
                    || row.source_occurrence_id.as_deref() != Some(occurrence_id.as_str())
            }
            _ => row.source_schedule_id.is_some() || row.source_occurrence_id.is_some(),
        }
        || i64::try_from(group.source_epoch).ok() != Some(row.source_epoch)
        || i64::try_from(group.version).ok() != Some(row.version)
        || i64::try_from(group.parent_input_revision).ok() != Some(row.parent_input_revision)
        || i64::try_from(group.parent_control_revision).ok() != Some(row.parent_control_revision)
        || group.parent_active != row.parent_active
        || group.limits.deadline_ms != row.deadline_ms
        || admission_label(group.source_admission) != row.source_admission
    {
        return Err(invalid());
    }
    Ok(group)
}

pub fn admission_label(admission: SourceAdmission) -> &'static str {
    match admission {
        SourceAdmission::Open => "open",
        SourceAdmission::Paused => "paused",
        SourceAdmission::Closed => "closed",
    }
}

/// Every child run of a group, read in bounded pages without a cumulative limit.
pub async fn group_children_on<C: ConnectionTrait>(
    txn: &C,
    group: &DelegationGroup,
) -> Result<Vec<run_row::Model>, DbErr> {
    group.validate().map_err(|_| invalid())?;
    let mut pages = run_row::Entity::find()
        .filter(run_row::Column::GroupId.eq(&group.group_id))
        .filter(run_row::Column::RootConversationId.eq(&group.root_conversation_id))
        .filter(run_row::Column::ActorId.eq(&group.actor_id))
        .filter(run_row::Column::DeviceId.eq(&group.device_id))
        .order_by_asc(run_row::Column::ChildConversationId)
        .paginate(txn, 128);
    let mut children = Vec::new();
    while let Some(page) = pages.fetch_and_next().await? {
        children.extend(page);
    }
    Ok(children)
}
