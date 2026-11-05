//! The Worker queue spans native works of one workbench. Each work still owns
//! its individual request authority and execution session.

use crate::entity::agent_subagent_run as run;
use desk_agent_protocol::ai_assistant::subagent::{SubAgentState, SubAgentWaitReason};
use desk_diagnose_core::subagent::{
    SUBAGENT_ROOT_CAPACITY,
    role::{DelegatedTaskBinding, DelegationSource},
    state::SubAgentRun,
};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait, Select,
    sea_query::{Expr, Func},
};
use serde::Serialize;

fn invalid() -> DbErr {
    DbErr::Custom("delegated task is unavailable or its control state changed".into())
}

#[inline(never)]
pub fn decode_run(row: &run::Model) -> Result<SubAgentRun, DbErr> {
    let run: SubAgentRun = serde_json::from_str(&row.state_json).map_err(|_| invalid())?;
    if run.worker_id != row.worker_id
        || run.binding.source.workbench_id() != row.workbench_id.as_deref()
    {
        return Err(invalid());
    }
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

pub fn in_scope(
    actor: &str,
    device: &str,
    native_parent: &str,
    workbench: Option<&str>,
) -> Select<run::Entity> {
    let query = run::Entity::find()
        .filter(run::Column::ActorId.eq(actor))
        .filter(run::Column::DeviceId.eq(device));
    if let Some(workbench) = workbench {
        query.filter(run::Column::WorkbenchId.eq(workbench))
    } else {
        query
            .filter(run::Column::WorkbenchId.is_null())
            .filter(run::Column::RootConversationId.eq(native_parent))
    }
}

fn unfinished(query: Select<run::Entity>) -> Select<run::Entity> {
    query.filter(run::Column::State.is_not_in(["completed", "failed", "cancelled"]))
}

pub fn unfinished_in_scope(
    actor: &str,
    device: &str,
    native_parent: &str,
    workbench: Option<&str>,
) -> Select<run::Entity> {
    unfinished(in_scope(actor, device, native_parent, workbench))
}

/// Called under the same owner/writer lock that guards the first native claim.
pub async fn workbench_has_slot_on(
    db: &impl ConnectionTrait,
    row: &run::Model,
    capacity: u32,
) -> Result<bool, DbErr> {
    let Some(workbench) = row.workbench_id.as_deref() else {
        return Ok(true);
    };
    if row.workbench_claimed_at_ms.is_some() {
        return Ok(true);
    }
    let started = unfinished_in_scope(&row.actor_id, &row.device_id, "", Some(workbench))
        .filter(run::Column::WorkbenchClaimedAtMs.is_not_null())
        .count(db)
        .await?;
    Ok(started < u64::from(capacity))
}

pub async fn recent_workbench_workers_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
) -> Result<Vec<String>, DbErr> {
    in_scope(actor, "", "", Some(workbench))
        .select_only()
        .column(run::Column::WorkerId)
        .group_by(run::Column::WorkerId)
        .order_by_desc(Expr::from(Func::max(Expr::col(run::Column::Id))))
        .limit(10)
        .into_tuple::<String>()
        .all(db)
        .await
}

#[derive(Serialize)]
pub struct WorkerPage {
    pub items: Vec<String>,
    pub total: u64,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

pub async fn worker_page_on(
    db: &impl ConnectionTrait,
    scope: Select<run::Entity>,
    after: Option<&str>,
    limit: u32,
) -> Result<Option<WorkerPage>, DbErr> {
    let workers = scope
        .select_only()
        .column(run::Column::WorkerId)
        .group_by(run::Column::WorkerId);
    let mut page = workers.clone();
    if let Some(after) = after {
        if workers
            .clone()
            .filter(run::Column::WorkerId.eq(after))
            .into_tuple::<String>()
            .one(db)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        page = page.filter(run::Column::WorkerId.gt(after));
    }
    let total = workers.count(db).await?;
    let mut items = page
        .order_by_asc(run::Column::WorkerId)
        .limit(u64::from(limit) + 1)
        .into_tuple::<String>()
        .all(db)
        .await?;
    let has_more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next_cursor = if has_more {
        items.last().cloned()
    } else {
        None
    };
    Ok(Some(WorkerPage {
        items,
        total,
        next_cursor,
        has_more,
    }))
}

#[derive(Serialize)]
pub struct WorkerStatus {
    pub worker_id: String,
    pub current_request: Option<RequestStatus>,
    pub queued_requests: Vec<RequestStatus>,
    pub latest_finished_request: Option<RequestStatus>,
}

#[derive(Serialize)]
pub struct RequestStatus {
    pub request_id: String,
    pub state: SubAgentState,
    pub wait_reason: Option<SubAgentWaitReason>,
    pub state_revision: u64,
}

/// Read the queue and latest finished request together. Only server-owned
/// metadata crosses native work boundaries through this snapshot.
pub async fn worker_status_on(
    db: &impl ConnectionTrait,
    scope: Select<run::Entity>,
    worker_id: &str,
) -> Result<Option<WorkerStatus>, DbErr> {
    let scope = scope.filter(run::Column::WorkerId.eq(worker_id));
    let latest_finished = scope
        .clone()
        .filter(run::Column::State.is_in(["completed", "failed", "cancelled"]))
        .select_only()
        .column(run::Column::Id)
        .order_by_desc(run::Column::Id)
        .limit(1)
        .into_query();
    let rows = scope
        .filter(
            Condition::any()
                .add(run::Column::State.is_not_in(["completed", "failed", "cancelled"]))
                .add(run::Column::Id.in_subquery(latest_finished)),
        )
        .order_by_asc(run::Column::Id)
        .limit(SUBAGENT_ROOT_CAPACITY as u64 + 2)
        .all(db)
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() > SUBAGENT_ROOT_CAPACITY + 1 {
        return Err(invalid());
    }
    let mut result = WorkerStatus {
        worker_id: worker_id.into(),
        current_request: None,
        queued_requests: Vec::new(),
        latest_finished_request: None,
    };
    for row in rows {
        let task = decode_run(&row)?;
        let status = RequestStatus {
            wait_reason: task.wait_reason(),
            state: task.state,
            state_revision: task.state_revision,
            request_id: task.binding.task_id,
        };
        if status.state.is_terminal() {
            result.latest_finished_request = Some(status);
        } else if result.current_request.is_none() {
            result.current_request = Some(status);
        } else {
            result.queued_requests.push(status);
        }
    }
    Ok(Some(result))
}

pub fn previous_requests(row: &run::Model) -> Select<run::Entity> {
    in_scope(
        &row.actor_id,
        &row.device_id,
        &row.root_conversation_id,
        row.workbench_id.as_deref(),
    )
    .filter(run::Column::WorkerId.eq(&row.worker_id))
    .filter(run::Column::Id.lt(row.id))
    .filter(run::Column::State.is_in(["completed", "failed"]))
    .order_by_desc(run::Column::Id)
    .limit(4)
}

/// A report can summarize any device the previous request could use, including
/// earlier reports it received. Preserve the whole scope when carrying it on.
pub fn shares_history_scope(
    previous: &DelegatedTaskBinding,
    current: &DelegatedTaskBinding,
) -> bool {
    let same_source = match (&previous.source, &current.source) {
        (DelegationSource::UserInput { .. }, DelegationSource::UserInput { .. }) => true,
        (
            DelegationSource::WorkbenchOwner {
                workbench_id: a, ..
            },
            DelegationSource::WorkbenchOwner {
                workbench_id: b, ..
            },
        ) => a == b,
        (DelegationSource::Goal { goal_id: a }, DelegationSource::Goal { goal_id: b }) => a == b,
        (a @ DelegationSource::ScheduledOccurrence { .. }, b) => a == b,
        _ => false,
    };
    same_source
        && previous.targets.iter().all(|old| {
            current.targets.iter().any(|target| {
                old.device_id == target.device_id
                    && old.member_generation == target.member_generation
            })
        })
}

/// Admissions and claims hold the authenticated owner lock (Manager) or
/// SQLite writer transaction (Signal) before checking the head.
pub async fn is_worker_head_on(db: &impl ConnectionTrait, row: &run::Model) -> Result<bool, DbErr> {
    Ok(unfinished_in_scope(
        &row.actor_id,
        &row.device_id,
        &row.root_conversation_id,
        row.workbench_id.as_deref(),
    )
    .filter(run::Column::WorkerId.eq(&row.worker_id))
    .filter(run::Column::Id.lt(row.id))
    .select_only()
    .column(run::Column::Id)
    .into_tuple::<i64>()
    .one(db)
    .await?
    .is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::dynamic_run::AssistantInputTarget;

    fn work(root: &str, targets: &[(&str, u64)]) -> DelegatedTaskBinding {
        DelegatedTaskBinding {
            root_conversation_id: root.into(),
            group_id: format!("group-{root}"),
            task_id: format!("request-{root}"),
            source: DelegationSource::WorkbenchOwner {
                workbench_id: "workbench".into(),
                input_revision: 1,
            },
            targets: targets
                .iter()
                .map(|(id, generation)| AssistantInputTarget {
                    reference: format!("{root}-{id}"),
                    device_id: (*id).into(),
                    member_generation: Some(*generation),
                })
                .collect(),
            objective: "Check the reported behavior".into(),
            acceptance_criteria: vec!["Summarize the findings".into()],
            input_revision: 1,
            control_revision: 1,
            source_epoch: 1,
            deadline_ms: 123,
        }
    }

    #[test]
    fn worker_history_requires_the_same_source_and_all_original_device_memberships() {
        let original = work("a", &[("mac", 2)]);
        let same_mac = work("b", &[("linux", 3), ("mac", 2)]);
        assert!(shares_history_scope(&original, &same_mac));
        assert!(!shares_history_scope(
            &original,
            &work("c", &[("linux", 3)])
        ));
        assert!(!shares_history_scope(&original, &work("d", &[("mac", 4)])));
        assert!(shares_history_scope(&work("e", &[]), &same_mac));

        let mut unrelated = same_mac;
        unrelated.source = DelegationSource::WorkbenchOwner {
            workbench_id: "other-workbench".into(),
            input_revision: 1,
        };
        assert!(!shares_history_scope(&original, &unrelated));
        unrelated.source = DelegationSource::Goal {
            goal_id: "goal".into(),
        };
        assert!(!shares_history_scope(&original, &unrelated));
    }
}
