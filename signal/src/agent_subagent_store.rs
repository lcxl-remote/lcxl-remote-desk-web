//! Durable delegation records and source controls, scoped to one owner and root.

mod history;
pub(crate) use history::group_children_on;
mod control;
mod facts;
mod lifecycle;
mod main_tools;
mod notification;
mod observation;
mod session;
pub(crate) use notification::{
    mark_notification_attempt_on, prepare_notification_on, ready_notification_source_on,
};
mod parent_wait;
pub(crate) use main_tools::required_children_complete_on;
pub(crate) use observation::{acknowledge_results_on, save_main_delegation_session};
pub use parent_wait::{ClaimedParentCompletion, ParentWaitResolution};
pub(crate) use parent_wait::{goal_wait_ready_on, ready_wait_source_on};
mod native_cancel;
mod retention;
mod task_control;
pub(crate) use retention::{
    close_root_on, deleted_on, pinned_conversation_ids, pinned_goal_ids, purge_authority,
    purge_groups, reclaim_condition, redact_deleted_content, tombstone_on,
};
mod main_stop;
pub use main_stop::MainStopOutcome;
pub(crate) use main_stop::resume_goal_parent_on;
mod owner_attention;
mod presentation;
pub(crate) use owner_attention::augment_owner_attention_on;
mod budget;
mod funding;
mod model_receipt;
mod review_budget;
mod runtime_budget;
mod ui_read;
pub(crate) use funding::{reserve_call_budget_on, settle_call_budget_on};
pub(crate) use review_budget::{
    mark_review_dispatch_on, reserve_review_call_on, settle_review_call_on,
};
mod candidates;
mod claim;
mod command_completion;
mod creation;
mod initialization;
mod recovery;
mod resources;
mod scheduled_source;
mod source_context;
pub(crate) use scheduled_source::lock_child_source_on;
mod scheduled_initialization;
#[cfg(test)]
pub(crate) use tests::scheduled::{
    answered_children_fixture as scheduled_test_answer_fixture,
    answered_children_fixture_at as scheduled_test_answer_fixture_at,
    awaiting_children_fixture as scheduled_test_wait_fixture,
    complete_scheduled_child as complete_scheduled_test_child, record_test_notice_answer,
};
mod scheduled_budget;
mod scheduled_native;
pub(crate) use scheduled_native::{ScheduledNativeDisposition, scheduled_native_on};
mod scheduled_control;
#[cfg(test)]
pub(crate) use budget::reserve_budget_on;
pub(crate) use budget::{BudgetAdmission, replace_group_on, settle_budget_on};
pub use claim::{ClaimedSubAgentTurn, SubAgentClaimBlock, SubAgentClaimOutcome};
pub(crate) use control::{
    append_state_event_on, check_child_permission_on, synchronize_control_on,
};
pub(crate) use creation::parent_planning_on;
pub(crate) use initialization::{
    decode_creation, initialize_goal_group_on, initialize_input_group_on,
};
pub(crate) use presentation::presentation_on;
pub(crate) use scheduled_control::close_scheduled_group_on;
pub(crate) use session::{save_child_session, write_child_session_on};
pub(crate) use source_context::{
    child_resume_admitted_on, record_child_resume_on, task_context_on,
};
pub use task_control::SubAgentControlOutcome;
#[cfg(test)]
pub(crate) use task_control::apply_task_control_on;

use crate::entity::{
    agent_delegation_group as group_row, agent_goal_run as goal_row, agent_session as session_row,
    agent_subagent_run as run_row,
};
use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantSubAgentPage, AiAssistantSubAgentResult, AiAssistantSubAgentSummary,
    MAX_SUBAGENT_PAGE_SIZE,
};
use desk_diagnose_core::{
    goal::GoalState,
    session::{AgentSessionSurface, PersistedAgentSession},
    subagent::{
        SubAgentState,
        group::{DelegationGroup, SourceAdmission},
        state::SubAgentRun,
    },
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait,
};

#[derive(Clone)]
pub struct SubAgentStore {
    db: DatabaseConnection,
}

fn invalid() -> DbErr {
    DbErr::Custom("delegated task is unavailable or its control state changed".into())
}

pub(crate) fn decode_group(row: &group_row::Model) -> Result<DelegationGroup, DbErr> {
    let group: DelegationGroup = serde_json::from_str(&row.state_json).map_err(|_| invalid())?;
    group.validate().map_err(|_| invalid())?;
    if group.group_id != row.group_id
        || group.root_conversation_id != row.root_conversation_id
        || group.actor_id != row.actor_id
        || group.device_id != row.device_id
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

pub(crate) fn decode_run(row: &run_row::Model) -> Result<SubAgentRun, DbErr> {
    let run: SubAgentRun = serde_json::from_str(&row.state_json).map_err(|_| invalid())?;
    run.validate().map_err(|_| invalid())?;
    if run.binding.task_id != row.task_id
        || run.binding.group_id != row.group_id
        || run.binding.root_conversation_id != row.root_conversation_id
        || run.child_conversation_id != row.child_conversation_id
        || run.actor_id != row.actor_id
        || run.device_id != row.device_id
        || run.state.as_str() != row.state
        || i64::try_from(run.binding.input_revision).ok() != Some(row.input_revision)
        || i64::try_from(run.binding.control_revision).ok() != Some(row.control_revision)
        || i64::try_from(run.binding.source_epoch).ok() != Some(row.source_epoch)
        || i64::try_from(run.state_revision).ok() != Some(row.state_revision)
        || run.binding.deadline_ms != row.deadline_ms
    {
        return Err(invalid());
    }
    Ok(run)
}

fn admission_label(admission: SourceAdmission) -> &'static str {
    match admission {
        SourceAdmission::Open => "open",
        SourceAdmission::Paused => "paused",
        SourceAdmission::Closed => "closed",
    }
}

async fn parent_on<C: ConnectionTrait>(
    db: &C,
    root: &str,
    actor: &str,
    device: &str,
) -> Result<PersistedAgentSession, DbErr> {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(root))
        .filter(session_row::Column::ActorId.eq(actor))
        .filter(session_row::Column::DeviceId.eq(device))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if !session.agent_role.is_main()
        || session.surface != AgentSessionSurface::AiAssistant
        || session.conversation_id != root
        || session.actor_id != actor
        || session.device_id != device
        || session.version != row.version
    {
        return Err(invalid());
    }
    Ok(session)
}

impl SubAgentStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn list_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<AiAssistantSubAgentPage, DbErr> {
        if !(1..=MAX_SUBAGENT_PAGE_SIZE).contains(&limit) {
            return Err(invalid());
        }
        let before = match cursor {
            Some(cursor) => cursor.parse::<i64>().map_err(|_| invalid())?,
            None => i64::MAX,
        };
        if before <= 0 {
            return Err(invalid());
        }
        let txn = self.db.begin().await?;
        parent_on(&txn, root, actor, device).await?;
        let query = run_row::Entity::find()
            .filter(run_row::Column::RootConversationId.eq(root))
            .filter(run_row::Column::ActorId.eq(actor))
            .filter(run_row::Column::DeviceId.eq(device));
        let total = query.clone().count(&txn).await?;
        let unfinished = query
            .clone()
            .filter(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
            .count(&txn)
            .await?;
        let rows = query
            .filter(run_row::Column::Id.lt(before))
            .order_by_desc(run_row::Column::Id)
            .limit(u64::from(limit) + 1)
            .all(&txn)
            .await?;
        let has_more = rows.len() > limit as usize;
        let visible: Vec<_> = rows.iter().take(limit as usize).collect();
        let items = visible
            .iter()
            .map(|row| decode_run(row).map(|run| run.summary()))
            .collect::<Result<_, _>>()?;
        let next_cursor = if has_more {
            visible.last().map(|row| row.id.to_string())
        } else {
            None
        };
        txn.commit().await?;
        Ok(AiAssistantSubAgentPage {
            items,
            total,
            unfinished,
            next_cursor,
            has_more,
        })
    }

    pub async fn status_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        task_id: &str,
    ) -> Result<AiAssistantSubAgentSummary, DbErr> {
        Ok(self
            .result_for_owner(root, actor, device, task_id)
            .await?
            .task)
    }

    pub async fn result_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        task_id: &str,
    ) -> Result<AiAssistantSubAgentResult, DbErr> {
        let txn = self.db.begin().await?;
        parent_on(&txn, root, actor, device).await?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::RootConversationId.eq(root))
            .filter(run_row::Column::ActorId.eq(actor))
            .filter(run_row::Column::DeviceId.eq(device))
            .filter(run_row::Column::TaskId.eq(task_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let result = decode_run(&row)?.result();
        txn.commit().await?;
        Ok(result)
    }
}

/// The caller holds owner/device and root conversation control. Acquire child
/// conversations before group/task/session/goal rows, matching every coordinator.
pub(crate) async fn lock_goal_source_on<C: ConnectionTrait>(
    db: &C,
    root: &str,
    actor: &str,
    device: &str,
    goal_id: &str,
) -> Result<(), DbErr> {
    let groups = group_row::Entity::find()
        .filter(group_row::Column::RootConversationId.eq(root))
        .filter(group_row::Column::ActorId.eq(actor))
        .filter(group_row::Column::DeviceId.eq(device))
        .filter(group_row::Column::SourceGoalId.eq(goal_id))
        .order_by_asc(group_row::Column::Id)
        .all(db)
        .await?;
    if groups.is_empty() {
        return Ok(());
    }
    let group_ids: Vec<_> = groups.iter().map(|group| group.group_id.clone()).collect();
    let runs = run_row::Entity::find()
        .filter(run_row::Column::GroupId.is_in(group_ids.clone()))
        .filter(run_row::Column::RootConversationId.eq(root))
        .filter(run_row::Column::ActorId.eq(actor))
        .filter(run_row::Column::DeviceId.eq(device))
        .order_by_asc(run_row::Column::ChildConversationId)
        .all(db)
        .await?;
    let mut session_ids: Vec<_> = runs
        .iter()
        .map(|run| run.child_conversation_id.clone())
        .collect();

    session_ids.push(root.into());
    session_ids.sort();

    Ok(())
}

/// Commit source epoch, child admission and task states with the main goal. A new
/// parent message never enters this path and cannot invalidate independent children.
pub(crate) async fn apply_goal_source_on<C: ConnectionTrait>(
    db: &C,
    root: &str,
    actor: &str,
    device: &str,
    goal_id: &str,
    goal_state: GoalState,
    now_ms: i64,
) -> Result<(), DbErr> {
    let admission = match goal_state {
        GoalState::Paused(_) => SourceAdmission::Paused,
        GoalState::Queued => SourceAdmission::Open,
        GoalState::Cancelled | GoalState::Failed => SourceAdmission::Closed,
        _ => return Ok(()),
    };
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(invalid)?
        .to_rfc3339();
    let groups = group_row::Entity::find()
        .filter(group_row::Column::RootConversationId.eq(root))
        .filter(group_row::Column::ActorId.eq(actor))
        .filter(group_row::Column::DeviceId.eq(device))
        .filter(group_row::Column::SourceGoalId.eq(goal_id))
        .order_by_asc(group_row::Column::Id)
        .all(db)
        .await?;
    for row in groups {
        let mut group = decode_group(&row)?;
        if group.source_admission == SourceAdmission::Closed {
            continue;
        }
        let epoch = group
            .set_source_admission(admission)
            .map_err(|_| invalid())?;
        let changed = group_row::Entity::update_many()
            .set(group_row::ActiveModel {
                source_admission: Set(admission_label(group.source_admission).into()),
                source_epoch: Set(i64::try_from(epoch).map_err(|_| invalid())?),
                state_json: Set(serde_json::to_string(&group).map_err(|_| invalid())?),
                version: Set(i64::try_from(group.version).map_err(|_| invalid())?),
                updated_at: Set(now_ms),
                ..Default::default()
            })
            .filter(group_row::Column::Id.eq(row.id))
            .filter(group_row::Column::Version.eq(row.version))
            .exec(db)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        let runs = run_row::Entity::find()
            .filter(run_row::Column::GroupId.eq(&group.group_id))
            .filter(run_row::Column::RootConversationId.eq(root))
            .filter(run_row::Column::ActorId.eq(actor))
            .filter(run_row::Column::DeviceId.eq(device))
            .order_by_asc(run_row::Column::Id)
            .all(db)
            .await?;
        for row in runs {
            let mut run = decode_run(&row)?;
            if run.state.is_terminal() || run.state == SubAgentState::Cancelling {
                continue;
            }
            match admission {
                SourceAdmission::Paused
                    if !run.source_paused || run.binding.source_epoch != epoch =>
                {
                    run.pause_source(epoch, &now).map_err(|_| invalid())?;
                }
                SourceAdmission::Open if now_ms >= run.binding.deadline_ms => {
                    run.fail("delegation_deadline_reached", &now)
                        .map_err(|_| invalid())?;
                }
                SourceAdmission::Open if run.source_paused => {
                    run.resume_source(epoch, &now).map_err(|_| invalid())?;
                }
                SourceAdmission::Closed => {
                    run.request_cancel(run.fence(), &now)
                        .map_err(|_| invalid())?;
                    run.binding.source_epoch = epoch;
                    run.settle_cancel(&now).map_err(|_| invalid())?;
                }
                _ => continue,
            }
            replace_run_on(db, &row, &run, now_ms).await?;
            synchronize_control_on(db, &run, now_ms).await?;
            append_state_event_on(db, &group, &run, now_ms).await?;
        }
    }
    Ok(())
}

pub(crate) async fn replace_run_on<C: ConnectionTrait>(
    db: &C,
    old: &run_row::Model,
    run: &SubAgentRun,
    now_ms: i64,
) -> Result<(), DbErr> {
    run.validate().map_err(|_| invalid())?;
    let changed = run_row::Entity::update_many()
        .set(run_row::ActiveModel {
            state: Set(run.state.as_str().into()),
            input_revision: Set(i64::try_from(run.binding.input_revision).map_err(|_| invalid())?),
            control_revision: Set(
                i64::try_from(run.binding.control_revision).map_err(|_| invalid())?
            ),
            source_epoch: Set(i64::try_from(run.binding.source_epoch).map_err(|_| invalid())?),
            state_revision: Set(i64::try_from(run.state_revision).map_err(|_| invalid())?),
            state_json: Set(serde_json::to_string(run).map_err(|_| invalid())?),
            next_attempt_at_ms: Set(matches!(
                run.state,
                SubAgentState::Queued | SubAgentState::WaitingResource
            )
            .then_some(now_ms)),
            updated_at: Set(now_ms),
            ..Default::default()
        })
        .filter(run_row::Column::Id.eq(old.id))
        .filter(run_row::Column::StateRevision.eq(old.state_revision))
        .filter(run_row::Column::ControlRevision.eq(old.control_revision))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

/// Final child action admission under the caller's owner/root control locks.
/// Main input/control changes are independent of this child's frozen input.
pub(crate) async fn check_child_action_on<C: ConnectionTrait>(
    db: &C,
    session: &PersistedAgentSession,
    fence: &desk_diagnose_core::action_turn_fence::AssistantTurnFence,
    now_ms: i64,
) -> Result<bool, DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(fence.delegation.is_none());
    };
    let Some(delegation) = &fence.delegation else {
        return Ok(false);
    };
    if delegation.root_conversation_id != binding.root_conversation_id
        || delegation.group_id != binding.group_id
        || delegation.task_id != binding.task_id
    {
        return Ok(false);
    }
    let Some(row) = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(run_row::Column::ActorId.eq(&session.actor_id))
        .filter(run_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
    else {
        return Ok(false);
    };
    let run = decode_run(&row)?;
    let Some(row) = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
    else {
        return Ok(false);
    };
    let group = decode_group(&row)?;
    if group.source_admission != SourceAdmission::Open
        || now_ms >= group.limits.deadline_ms
        || now_ms >= run.binding.deadline_ms
        || group.source_epoch != delegation.source_epoch
        || group.source != run.binding.source
        || run.validate_session(session).is_err()
        || !group
            .budget
            .child_charged
            .checked_add(group.budget.child_outstanding)
            .map_err(|_| invalid())?
            .fits(group.limits.child_ceiling())
        || !group
            .budget
            .charged
            .checked_add(group.budget.outstanding)
            .map_err(|_| invalid())?
            .fits(group.limits.total)
    {
        return Ok(false);
    }
    let expected = desk_diagnose_core::subagent::state::PlanningFence {
        input_revision: fence.input_revision,
        control_revision: fence.control_revision,
        source_epoch: delegation.source_epoch,
    };
    Ok(run.require_current(expected).is_ok())
}

#[cfg(test)]
mod tests;
