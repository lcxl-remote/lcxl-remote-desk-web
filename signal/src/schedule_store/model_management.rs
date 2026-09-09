//! Model management is confined to the current conversation and server origin.
use super::{ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use desk_diagnose_core::{
    schedule::management_tools::{Action, proposed_ids},
    session::PersistedAgentSession,
};
use sea_orm::{
    ColumnTrait, Condition, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set,
};
use serde_json::{Value, json};

fn scope(owner: i32, session: &PersistedAgentSession) -> Condition {
    let ids = proposed_ids(session);
    let mut origin =
        Condition::any().add(entity::Column::SourceConversationId.eq(&session.conversation_id));
    if !ids.is_empty() {
        origin = origin.add(
            Condition::all()
                .add(entity::Column::CreationSource.eq("ai_proposal"))
                .add(entity::Column::ScheduleId.is_in(ids)),
        );
    }
    Condition::all()
        .add(entity::Column::Status.ne("pending_review"))
        .add(entity::Column::OwnerUserId.eq(owner))
        .add(entity::Column::TargetDeviceId.eq(&session.device_id))
        .add(origin)
}

/// Acquire existing task before the conversation row, just like the dispatcher.
pub(super) async fn lock_target(
    txn: &DatabaseTransaction,
    owner: i32,
    session: &PersistedAgentSession,
    action: &Action,
) -> Result<Option<entity::Model>, ScheduleStoreError> {
    let Action::Cancel { schedule_id, .. } = action else {
        return Ok(None);
    };
    let task = entity::Entity::find()
        .filter(scope(owner, session))
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?;
    if let Some(task) = task
        .as_ref()
        .filter(|task| task.creation_source == "ai_proposal")
    {
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    Ok(task)
}

fn view(task: &entity::Model) -> Value {
    json!({"schedule_id":task.schedule_id,"title":task.title,"kind":task.kind,"state":task.status,
        "revision":task.revision,"creation_source":task.creation_source,
        "can_cancel":task.creation_source == "ai_proposal" && !matches!(task.status.as_str(),"deleted"|"completed"),
        "next_run_at_utc_ms":task.next_run_at,"active_run_id":task.active_run_id,
        "rule":serde_json::from_str::<Value>(&task.spec_json).ok().map(|spec| spec["rule"].clone())})
}

pub(super) async fn list(
    txn: &DatabaseTransaction,
    owner: i32,
    session: &PersistedAgentSession,
    after: i64,
    limit: u64,
    now: i64,
) -> Result<String, ScheduleStoreError> {
    let mut rows = entity::Entity::find()
        .filter(scope(owner, session))
        .filter(entity::Column::Id.gt(after))
        .order_by_asc(entity::Column::Id)
        .limit(limit + 1)
        .all(txn)
        .await?;
    let more = rows.len() > limit as usize;
    if more {
        rows.pop();
    }
    let next = more.then(|| rows.last().map(|row| row.id)).flatten();
    Ok(json!({"ok":true,"tasks":rows.iter().map(view).collect::<Vec<_>>(),"next_after":next,"observed_at_utc_ms":now}).to_string())
}

pub(super) async fn cancel(
    txn: &DatabaseTransaction,
    task: Option<entity::Model>,
    expected: i64,
    now: i64,
) -> Result<String, ScheduleStoreError> {
    let Some(task) = task else {
        return Ok(json!({"ok":false,"reason":"task_not_in_current_conversation"}).to_string());
    };
    if task.creation_source != "ai_proposal" {
        return Ok(json!({"ok":false,"reason":"manual_task_cannot_be_cancelled_by_ai","task":view(&task),"message":"Only the user can cancel this manually created task in the task manager. Do not request permission to bypass this restriction."}).to_string());
    }
    if task.status == "deleted" {
        return Ok(json!({"ok":true,"state":"already_cancelled","task":view(&task)}).to_string());
    }
    if task.status == "completed" {
        return Ok(
            json!({"ok":false,"reason":"task_already_completed","task":view(&task)}).to_string(),
        );
    }
    if task.revision != expected {
        return Ok(
            json!({"ok":false,"reason":"revision_changed_query_again","task":view(&task)})
                .to_string(),
        );
    }
    let work = if let Some(id) = task.active_run_id.as_deref() {
        Some(
            run::Entity::find()
                .filter(run::Column::RunId.eq(id))
                .filter(run::Column::ScheduleId.eq(&task.schedule_id))
                .filter(run::Column::OwnerUserId.eq(task.owner_user_id))
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?,
        )
    } else {
        None
    };
    let mut unstarted = false;
    if let Some(work) = &work {
        unstarted = super::edit::stop_pending(txn, work, now, true).await?;
    }
    let in_flight = work
        .as_ref()
        .is_some_and(|work| !work.failure_accounted && !unstarted);
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            revision: Set(task
                .revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            status: Set("deleted".into()),
            next_run_at: Set(None),
            active_run_id: Set(if unstarted {
                None
            } else {
                task.active_run_id.clone()
            }),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(expected))
        .filter(entity::Column::CreationSource.eq("ai_proposal"))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(json!({"ok":true,"schedule_id":task.schedule_id,"revision":expected+1,
        "state":if in_flight {"cancellation_requested"} else {"cancelled"},
        "future_scheduling_stopped":true,"effects_undone":false,
        "run_id":work.as_ref().map(|work| &work.run_id),
        "run_state":work.as_ref().map(|work| if unstarted { "cancelled" } else { work.status.as_str() }),"message":"No additional approval was required. Completed external effects are not undone."}).to_string())
}

#[cfg(test)]
mod tests;
