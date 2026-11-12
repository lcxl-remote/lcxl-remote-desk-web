//! Durable delegation records and source controls, scoped to one owner and root.

use crate::config::connection::DatabaseConnection;
mod history;
pub(crate) use history::group_children_on;
mod control;
mod issue_control;
pub(crate) use issue_control::apply_issue_changes_on;
mod facts;
mod lifecycle;
mod main_tools;
pub(crate) mod notification;
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
pub(crate) use native_cancel::cancel_native_actions_for_subject_on;
mod retention;
mod task_control;
pub(crate) use retention::{
    close_root_for_subject_on, close_root_on, deleted_on, pinned_conversation_ids, pinned_goal_ids,
    purge_authority, purge_groups, reclaim_condition, redact_deleted_content, tombstone_on,
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
mod validated_session;
use validated_session::{CallSession, SessionRead, TxnValidatedSession};
mod shared_approval;
pub(crate) use shared_approval::shared_approval_current_on;
mod runtime_budget;
mod ui_read;
pub(crate) use funding::{reserve_call_budget_on, settle_call_budget_on};
pub(crate) use review_budget::{
    check_goal_review_funding_on, mark_review_dispatch_on, reserve_review_attempt_on,
    reserve_review_call_on, settle_review_call_on,
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
mod source_timing;
pub(crate) use scheduled_source::lock_child_source_on;
use source_timing::current_group_on;
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
pub(crate) use creation::{first_dispatch_on, parent_planning_on};
pub(crate) use initialization::{
    decode_creation, initialize_continuation_group_on, initialize_goal_group_on,
    initialize_input_group_on,
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
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set, TransactionTrait,
};

#[derive(Clone)]
pub struct SubAgentStore {
    db: DatabaseConnection,
}

/// The text stays generic for callers; the log names the rejecting check.
#[track_caller]
fn invalid() -> DbErr {
    let location = std::panic::Location::caller();
    log::warn!(
        "[subagent-store] rejected at {}:{}",
        location.file(),
        location.line()
    );
    DbErr::Custom("delegated task is unavailable or its control state changed".into())
}

#[inline(never)]
pub(crate) fn decode_group(row: &group_row::Model) -> Result<DelegationGroup, DbErr> {
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

pub(crate) use desk_assistant_data_store::worker_request::decode_run;

fn admission_label(admission: SourceAdmission) -> &'static str {
    match admission {
        SourceAdmission::Open => "open",
        SourceAdmission::Paused => "paused",
        SourceAdmission::Closed => "closed",
    }
}

#[inline(never)]
fn parent_on<'a, C: ConnectionTrait + crate::config::ConfigConnection + 'a>(
    db: &'a C,
    root: &'a str,
    actor: &'a str,
    device: &'a str,
) -> std::pin::Pin<
    Box<impl std::future::Future<Output = Result<Box<PersistedAgentSession>, DbErr>> + 'a>,
> {
    Box::pin(parent_on_inner(db, root, actor, device))
}

async fn parent_on_inner<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    root: &str,
    actor: &str,
    device: &str,
) -> Result<Box<PersistedAgentSession>, DbErr> {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(root))
        .filter(session_row::Column::ActorId.eq(actor))
        .filter(session_row::Column::DeviceId.eq(device))
        .one(db)
        .await?
        .ok_or_else(|| invalid())?;
    let session =
        Box::new(PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?);
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
        let txn = self.db.begin().await?;
        let task = owned_task_on(&txn, root, actor, device, task_id)
            .await?
            .summary();
        txn.commit().await?;
        Ok(task)
    }

    pub async fn result_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        task_id: &str,
        before_report_seq: Option<u64>,
    ) -> Result<AiAssistantSubAgentResult, DbErr> {
        let txn = self.db.begin().await?;
        let task = owned_task_on(&txn, root, actor, device, task_id).await?;
        let result = desk_assistant_data_store::worker_report::owner_result_on(
            &txn,
            &task,
            before_report_seq,
            chrono::Utc::now().timestamp_millis(),
        )
        .await?;
        txn.commit().await?;
        Ok(result)
    }
}

async fn owned_task_on(
    txn: &crate::config::connection::DatabaseTransaction,
    root: &str,
    actor: &str,
    device: &str,
    task_id: &str,
) -> Result<SubAgentRun, DbErr> {
    parent_on(txn, root, actor, device).await?;
    let row = run_row::Entity::find()
        .filter(run_row::Column::RootConversationId.eq(root))
        .filter(run_row::Column::ActorId.eq(actor))
        .filter(run_row::Column::DeviceId.eq(device))
        .filter(run_row::Column::TaskId.eq(task_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    decode_run(&row)
}

/// The caller holds owner/device and root conversation control. Acquire child
/// conversations before group/task/session/goal rows, matching every coordinator.
pub(crate) async fn lock_goal_source_on<C: ConnectionTrait + crate::config::ConfigConnection>(
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
    // Requests of one Worker share its session.
    session_ids.dedup();

    session_ids.push(root.into());
    session_ids.sort();

    Ok(())
}

/// Commit source epoch, child admission and task states with the main goal. A new
/// parent message never enters this path and cannot invalidate independent children.
pub(crate) async fn apply_goal_source_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    root: &str,
    actor: &str,
    device: &str,
    goal_id: &str,
    goal_state: GoalState,
    now_ms: i64,
) -> Result<desk_diagnose_core::model_observability::permission::PendingEnds, DbErr> {
    let admission = match goal_state {
        GoalState::Paused(_) => SourceAdmission::Paused,
        GoalState::Queued => SourceAdmission::Open,
        GoalState::Cancelled | GoalState::Failed => SourceAdmission::Closed,
        _ => return Ok(Default::default()),
    };
    let mut permission_ends =
        desk_diagnose_core::model_observability::permission::PendingEnds::default();
    let groups = desk_diagnose_core::future::boxed(|| async {
        group_row::Entity::find()
            .filter(group_row::Column::RootConversationId.eq(root))
            .filter(group_row::Column::ActorId.eq(actor))
            .filter(group_row::Column::DeviceId.eq(device))
            .filter(group_row::Column::SourceGoalId.eq(goal_id))
            .order_by_asc(group_row::Column::Id)
            .all(db)
            .await
    })
    .await?;
    for row in groups {
        permission_ends.extend(apply_group_admission_on(db, &row, admission, now_ms).await?);
    }
    Ok(permission_ends)
}

async fn apply_group_admission_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    row: &group_row::Model,
    admission: SourceAdmission,
    now_ms: i64,
) -> Result<desk_diagnose_core::model_observability::permission::PendingEnds, DbErr> {
    let mut permission_ends =
        desk_diagnose_core::model_observability::permission::PendingEnds::default();
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(|| invalid())?
        .to_rfc3339();
    let mut group = current_group_on(db, row).await?;
    if group.source_admission == SourceAdmission::Closed {
        return Ok(permission_ends);
    }
    let epoch = group
        .set_source_admission(admission)
        .map_err(|_| invalid())?;
    if group.limits.deadline_ms != row.deadline_ms && group.version == row.version as u64 {
        group.version = group.version.checked_add(1).ok_or_else(|| invalid())?;
    }
    desk_diagnose_core::future::boxed(|| replace_group_on(db, row, &group, now_ms)).await?;
    let runs = desk_diagnose_core::future::boxed(|| async {
        run_row::Entity::find()
            .filter(run_row::Column::GroupId.eq(&group.group_id))
            .filter(run_row::Column::RootConversationId.eq(&group.root_conversation_id))
            .filter(run_row::Column::ActorId.eq(&group.actor_id))
            .filter(run_row::Column::DeviceId.eq(&group.device_id))
            .order_by_asc(run_row::Column::Id)
            .all(db)
            .await
    })
    .await?;
    for row in runs {
        let mut run = decode_run(&row)?;
        if run.state.is_terminal() || run.state == SubAgentState::Cancelling {
            continue;
        }
        match admission {
            SourceAdmission::Paused if !run.source_paused || run.binding.source_epoch != epoch => {
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
        desk_diagnose_core::future::boxed(|| async {
            replace_run_on(db, &row, &run, now_ms).await
        })
        .await?;
        permission_ends.extend(
            desk_diagnose_core::future::boxed(|| async {
                synchronize_control_on(
                    db,
                    &run,
                    now_ms,
                    admission == SourceAdmission::Open && now_ms >= run.binding.deadline_ms,
                )
                .await
            })
            .await?,
        );
        desk_diagnose_core::future::boxed(|| async {
            append_state_event_on(db, &group, &run, now_ms).await
        })
        .await?;
    }
    Ok(permission_ends)
}

pub(crate) async fn replace_run_on<C: ConnectionTrait + crate::config::ConfigConnection>(
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
            workbench_claimed_at_ms: Set(old.workbench_claimed_at_ms.or_else(|| {
                (old.workbench_id.is_some() && run.state == SubAgentState::Running)
                    .then_some(now_ms)
            })),
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
pub(crate) async fn check_child_action_on<C: ConnectionTrait + crate::config::ConfigConnection>(
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
    let Some(expected) = desk_diagnose_core::subagent::control::request_fence(session, fence)
    else {
        return Ok(false);
    };
    check_child_planning_on(db, session, expected, now_ms).await
}

/// A central action, such as a web read, has no device to fence; the held
/// session's current request revisions stand in for the device fence.
pub(crate) async fn check_child_central_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
    now_ms: i64,
) -> Result<bool, DbErr> {
    if session.agent_role.binding().is_none() {
        return Ok(true);
    }
    let Some(expected) = desk_diagnose_core::subagent::control::session_fence(session) else {
        return Ok(false);
    };
    check_child_planning_on(db, session, expected, now_ms).await
}

async fn check_child_planning_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
    expected: desk_diagnose_core::subagent::state::PlanningFence,
    now_ms: i64,
) -> Result<bool, DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(false);
    };
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
    let group = current_group_on(db, &row).await?;
    if group.source_admission != SourceAdmission::Open
        || now_ms >= group.limits.deadline_ms
        || now_ms >= run.binding.deadline_ms
        || group.source_epoch != expected.source_epoch
        || group.source != run.binding.source
        || run.validate_session(session).is_err()
        || !group
            .budget
            .child_charged
            .checked_add(group.budget.child_outstanding)
            .map_err(|_| invalid())?
            .fits_allowance(group.limits.child_ceiling())
        || !group
            .budget
            .charged
            .checked_add(group.budget.outstanding)
            .map_err(|_| invalid())?
            .fits_allowance(group.limits.total)
    {
        return Ok(false);
    }
    Ok(run.require_current(expected).is_ok())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::native_cancel::{assert_stop_only, seed_native as seed_native_stop_test};

#[inline(never)]
fn boxed_group(row: &group_row::Model) -> Result<Box<DelegationGroup>, DbErr> {
    Ok(Box::new(decode_group(row)?))
}
