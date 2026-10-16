//! Resolve goal-owned work from immutable task origins and claimed-turn events.
use desk_diagnose_core::{
    goal::{GoalLedgerEvent, GoalRun},
    goal_continuation::GoalWorkWait,
    session::{ActionIdentity, PersistedAgentSession},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};

fn invalid() -> DbErr {
    DbErr::Custom("goal background task identity is inconsistent".into())
}

pub(crate) struct OriginalWork {
    pub turn_id: String,
    pub event_id: Option<String>,
    pub unresolved: bool,
}

pub(crate) async fn claim_for_turn<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    conversation_id: &str,
    turn_id: &str,
) -> Result<Option<GoalLedgerEvent>, DbErr> {
    let mut cursor = 0;
    loop {
        let rows = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq(conversation_id))
            .filter(agent_run_event::Column::Kind.eq("goal_slice_claimed"))
            .filter(agent_run_event::Column::Id.gt(cursor))
            .order_by_asc(agent_run_event::Column::Id)
            .limit(64)
            .all(db)
            .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        for row in rows {
            cursor = row.id;
            let event: GoalLedgerEvent =
                serde_json::from_str(&row.payload_json).map_err(|_| invalid())?;
            if event.event.event_id != row.event_id
                || event.event.input_revision != row.input_revision as u64
                || event.event.correlation_id.as_deref() != Some(event.goal_id.as_str())
            {
                return Err(invalid());
            }
            let claim = event.claim.as_ref().ok_or_else(invalid)?;
            {
                claim.validate().map_err(|_| invalid())?;
                if claim.segment.goal_id != event.goal_id
                    || claim.segment.goal_revision != event.goal_revision
                    || claim.segment.segment_seq != event.slice_seq
                    || claim.segment.lease_epoch != event.lease_epoch
                    || event.resulting_state != desk_diagnose_core::goal::GoalState::Running
                {
                    return Err(invalid());
                }
                if claim.turn_id == turn_id {
                    if event.event.run_id != conversation_id
                        || event.event.kind.as_str() != "goal_slice_claimed"
                    {
                        return Err(invalid());
                    }
                    return Ok(Some(event));
                }
            }
        }
    }
}

pub(crate) async fn owner_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
    action: &ActionIdentity,
) -> Result<Option<GoalRun>, DbErr> {
    let Some(original) = original_on(db, session, action).await? else {
        return Ok(None);
    };
    let Some(claim) = claim_for_turn(db, &session.conversation_id, &original.turn_id).await? else {
        return Ok(None);
    };
    let Some(row) = agent_goal_run::Entity::find()
        .filter(agent_goal_run::Column::GoalId.eq(&claim.goal_id))
        .filter(agent_goal_run::Column::ConversationId.eq(&session.conversation_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let goal = crate::agent_goal_store::decode(&row)?;
    if goal.owner_id != session.actor_id || goal.device_id != session.device_id {
        return Err(invalid());
    }
    Ok(Some(goal))
}

pub(crate) async fn prepare_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    goal: &GoalRun,
    session: &PersistedAgentSession,
    call_id: &str,
) -> Result<Option<GoalWorkWait>, DbErr> {
    if desk_diagnose_core::goal_continuation::unresolved_work_except(session, call_id) {
        return Ok(None);
    }
    let mut members = vec![];
    for action in session.execution_state.tasks() {
        let Some(original) = original_on(db, session, action).await? else {
            continue;
        };
        let Some(event) = claim_for_turn(db, &session.conversation_id, &original.turn_id).await?
        else {
            continue;
        };
        if event.goal_id == goal.goal_id
            && event.event.input_revision == session.input_revision
            && event.goal_revision == goal.goal_revision
            && event
                .claim
                .as_ref()
                .is_some_and(|claim| claim.control_revision <= session.control_revision)
        {
            members.push(action.clone());
        }
    }
    if members.is_empty() {
        return Ok(None);
    }
    GoalWorkWait::new(goal, session, call_id, members)
        .map(Some)
        .map_err(|_| invalid())
}

pub(crate) async fn delivered_event_ids_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    goal: &GoalRun,
    session: &PersistedAgentSession,
) -> Result<Vec<String>, DbErr> {
    let mut ready = vec![];
    let Some(wait) = &goal.work_wait else {
        return Ok(ready);
    };
    if !wait.matches(goal, session.control_revision) {
        return Ok(ready);
    }
    for action in &wait.members {
        let Some(original) = original_on(db, session, action).await? else {
            continue;
        };
        let Some(event_id) = original.event_id else {
            continue;
        };
        if session.execution_state.contains(action) {
            continue;
        }
        if session.conversation.iter().any(|message| {
            message.message_id == event_id
                && message.background_task_id.as_deref() == Some(&action.action_request_id)
                && matches!(
                    message.role,
                    desk_diagnose_core::chat::ChatRole::Tool
                        | desk_diagnose_core::chat::ChatRole::UntrustedOutput
                )
        }) {
            ready.push(event_id);
        }
    }
    Ok(ready)
}

pub(crate) async fn validate_wait_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    goal: &GoalRun,
    session: &PersistedAgentSession,
    wait: &GoalWorkWait,
) -> Result<(), DbErr> {
    if !wait.matches(goal, session.control_revision) {
        return Err(invalid());
    }
    for action in &wait.members {
        let original = original_on(db, session, action)
            .await?
            .ok_or_else(invalid)?;
        let claim = claim_for_turn(db, &session.conversation_id, &original.turn_id)
            .await?
            .ok_or_else(invalid)?;
        if claim.goal_id != goal.goal_id
            || claim.goal_revision != goal.goal_revision
            || claim.event.input_revision != session.input_revision
            || claim
                .claim
                .as_ref()
                .is_none_or(|claim| claim.control_revision > session.control_revision)
        {
            return Err(invalid());
        }
    }
    Ok(())
}

use crate::entity::{
    agent_action_item, agent_capability_dispatch_outbox, agent_exec_task, agent_goal_run,
    agent_run_event,
};

pub(crate) async fn original_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
    action: &ActionIdentity,
) -> Result<Option<OriginalWork>, DbErr> {
    use desk_diagnose_core::session::WorkKind;
    if action.kind == WorkKind::AgentExec {
        let Some(row) = agent_exec_task::Entity::find_by_id(action.work_id)
            .one(db)
            .await?
        else {
            return Ok(None);
        };
        if row.conversation_id != session.conversation_id
            || row.exec_request_id != action.action_request_id
            || row.execution_generation != action.execution_id
        {
            return Err(invalid());
        }
        let Some(dispatch) = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(&action.execution_id))
            .one(db)
            .await?
        else {
            return Ok(None);
        };
        let payload: crate::capability_grant_store::CapabilityDispatchPayload =
            serde_json::from_str(&dispatch.payload_json).map_err(|_| invalid())?;
        let Some(origin) = payload.command_origin else {
            return Ok(None);
        };
        origin.turn_fence.validate().map_err(|_| invalid())?;
        if origin.turn_fence.conversation_id != session.conversation_id
            || origin.turn_fence.actor_id != session.actor_id
            || origin.turn_fence.device_id != session.device_id
            || origin.tool_call_id != row.tool_call_id
        {
            return Err(invalid());
        }
        return Ok(Some(OriginalWork {
            turn_id: origin.turn_fence.turn_id,
            unresolved: row.status == crate::agent_exec_store::STATUS_UNKNOWN,
            event_id: (row.status == crate::agent_exec_store::STATUS_DONE
                && row.result_text.is_some())
            .then_some(row.event_id),
        }));
    }
    let Some(row) = agent_action_item::Entity::find_by_id(action.work_id)
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    if row.conversation_id != session.conversation_id
        || row.actor_id != session.actor_id
        || row.target_device_id != session.device_id
        || row.action_request_id != action.action_request_id
    {
        return Err(invalid());
    }
    if action.kind == WorkKind::ComputerAction
        && row.kind == crate::capability_grant_store::CAPABILITY_WORK_KIND
    {
        let (work, origin, result) =
            crate::capability_grant_store::SignalCapabilityGrantStore::computer_facts_on(
                db,
                &action.execution_id,
            )
            .await?;
        if work.id != action.work_id
            || result
                .as_ref()
                .is_some_and(|result| result.receipt.action != *action)
        {
            return Err(invalid());
        }
        return Ok(Some(OriginalWork {
            turn_id: origin.turn_fence.turn_id,
            unresolved: work.status
                == crate::capability_grant_store::CAPABILITY_WORK_OUTCOME_UNKNOWN,
            event_id: result.map(|result| result.work.completion_event_id),
        }));
    }
    if row.execution_id.as_deref() != Some(&action.execution_id) {
        return Err(invalid());
    }
    let terminal = if action.kind == WorkKind::CapabilityProvider {
        if row.kind != crate::agent_background_task_store::BACKGROUND_ACTION_KIND {
            return Err(invalid());
        }
        let record: desk_diagnose_core::dynamic_run::BackgroundTaskRecord =
            serde_json::from_str(&row.payload_json).map_err(|_| invalid())?;
        record.validate().map_err(|_| invalid())?;
        if record.turn_id != row.turn_id
            || record.task.task_id != action.action_request_id
            || format!("{}:{}", record.task.task_id, record.task.generation) != action.execution_id
        {
            return Err(invalid());
        }
        matches!(
            record.state,
            desk_diagnose_core::dynamic_run::BackgroundTaskState::Succeeded
                | desk_diagnose_core::dynamic_run::BackgroundTaskState::Failed
                | desk_diagnose_core::dynamic_run::BackgroundTaskState::Cancelled
        ) && row.result_json.is_some()
    } else {
        if row.kind != action.kind.as_str() {
            return Err(invalid());
        }
        row.status == "done" && row.result_json.is_some()
    };
    Ok(Some(OriginalWork {
        turn_id: row.turn_id,
        unresolved: matches!(
            row.status.as_str(),
            "unknown" | "background_outcome_unknown"
        ) && row.manual_resolved_at.is_none(),
        event_id: terminal.then_some(row.completion_event_id),
    }))
}

pub(crate) async fn all_tracked_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
) -> Result<bool, DbErr> {
    for action in session.execution_state.tasks() {
        if original_on(db, session, action)
            .await?
            .is_none_or(|work| work.unresolved)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// An unknown native outcome requires reconciliation even after metadata delivery.
pub(crate) async fn unresolved_wait_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    goal: &GoalRun,
    session: &PersistedAgentSession,
) -> Result<bool, DbErr> {
    if session.execution_state.has_unresolved_outcome() {
        return Ok(true);
    }
    if let Some(wait) = &goal.work_wait {
        for action in &wait.members {
            if original_on(db, session, action)
                .await?
                .is_none_or(|work| work.unresolved)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Merge only clock advances proven to come from independent background events.
/// Input, control, lease and other session writes still invalidate the held turn.
pub(crate) async fn rebase_background_events_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    current: &PersistedAgentSession,
    held: &mut PersistedAgentSession,
) -> Result<bool, DbErr> {
    if current.version == held.version {
        return Ok(true);
    }
    if held.trigger_origin != desk_diagnose_core::session::TriggerOrigin::GoalContinuation
        || !current.turn_state.is_active()
        || current.conversation_id != held.conversation_id
        || current.actor_id != held.actor_id
        || current.device_id != held.device_id
        || current.lease_token != held.lease_token
        || current.current_turn_id != held.current_turn_id
        || current.input_revision != held.input_revision
        || current.control_revision != held.control_revision
        || current.focus_epoch.goal_segment != held.focus_epoch.goal_segment
        || current.delegation_group_id != held.delegation_group_id
    {
        return Ok(false);
    }
    let Some(versions) = current
        .version
        .checked_sub(held.version)
        .and_then(|n| u64::try_from(n).ok())
        .filter(|n| *n > 0)
    else {
        return Ok(false);
    };
    if current.last_event_seq.checked_sub(held.last_event_seq) != Some(versions) {
        return Ok(false);
    }
    let events = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::RunId.eq(&held.conversation_id))
        .filter(
            agent_run_event::Column::EventSeq
                .gt(i64::try_from(held.last_event_seq).map_err(|_| invalid())?),
        )
        .filter(
            agent_run_event::Column::EventSeq
                .lte(i64::try_from(current.last_event_seq).map_err(|_| invalid())?),
        )
        .order_by_asc(agent_run_event::Column::EventSeq)
        .all(db)
        .await?;
    if events.len() as u64 != versions
        || events.iter().enumerate().any(|(index, event)| {
            u64::try_from(event.event_seq).ok() != held.last_event_seq.checked_add(index as u64 + 1)
                || !matches!(
                    event.kind.as_str(),
                    "background_progress"
                        | "background_completion"
                        | "background_cancel_requested"
                        | "background_cancel_delivered"
                )
        })
    {
        return Ok(false);
    }
    held.last_event_seq = current.last_event_seq;
    held.version = current.version;
    Ok(true)
}
