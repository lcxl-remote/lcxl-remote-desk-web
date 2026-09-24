//! SQLite projection and fenced writes for an AI Assistant goal.
//!
//! Mutating helpers take an existing transaction so the caller can lock the
//! session first and commit both rows and the run event together.

use crate::entity::{
    agent_attachment as attachment_row, agent_goal_run as goal_row,
    agent_permission_resume as resume_row, agent_run_event, agent_session,
};
use chrono::{DateTime, Duration, Utc};
use desk_diagnose_core::dynamic_run::{AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEventKind};
use desk_diagnose_core::goal::{
    GoalControl, GoalLedgerEvent, GoalOwnerAction, GoalPauseReason, GoalPermissionWake, GoalRun,
    GoalSegmentEnd, GoalState, GoalUsage, GoalWaitReason,
};
use desk_diagnose_core::seam::ClaimTurnParams;
use desk_diagnose_core::session::{
    AgentSessionSurface, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, DatabaseTransaction, DbErr,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};

fn invalid() -> DbErr {
    DbErr::Custom("AI Assistant goal state is inconsistent".into())
}

fn as_i64(value: u64) -> Result<i64, DbErr> {
    value.try_into().map_err(|_| invalid())
}

async fn active_deadline_cutoff(db: &DatabaseConnection, now_ms: u64) -> Result<i64, DbErr> {
    let policy = crate::goal_budget_policy::read(db).await?;
    as_i64(
        policy
            .limits
            .deadline_ms
            .map_or(0, |duration| now_ms.saturating_sub(duration)),
    )
}

fn project(goal: &GoalRun) -> Result<goal_row::ActiveModel, DbErr> {
    goal.validate().map_err(|_| invalid())?;
    Ok(goal_row::ActiveModel {
        goal_id: Set(goal.goal_id.clone()),
        conversation_id: Set(goal.conversation_id.clone()),
        actor_id: Set(goal.owner_id.clone()),
        device_id: Set(goal.device_id.clone()),
        status: Set(goal.status_code().into()),
        state_json: Set(serde_json::to_string(goal).map_err(|_| invalid())?),
        state_version: Set(as_i64(goal.state_version)?),
        goal_revision: Set(as_i64(goal.goal_revision)?),
        input_revision: Set(as_i64(goal.input_revision)?),
        slice_seq: Set(i32::try_from(goal.slice_seq).map_err(|_| invalid())?),
        lease_epoch: Set(as_i64(goal.lease_epoch)?),
        lease_owner: Set(None),
        lease_deadline: Set(None),
        next_attempt_at: Set(goal.next_attempt_unix_ms.map(as_i64).transpose()?),
        absolute_deadline: Set(as_i64(goal.deadline_unix_ms)?),
        created_at: Set(as_i64(goal.created_at_unix_ms)?),
        updated_at: Set(as_i64(goal.updated_at_unix_ms)?),
        ..Default::default()
    })
}

pub(crate) fn decode(row: &goal_row::Model) -> Result<GoalRun, DbErr> {
    let goal: GoalRun = serde_json::from_str(&row.state_json).map_err(|_| invalid())?;
    goal.validate().map_err(|_| invalid())?;
    if row.goal_id != goal.goal_id
        || row.conversation_id != goal.conversation_id
        || row.actor_id != goal.owner_id
        || row.device_id != goal.device_id
        || row.status != goal.status_code()
        || row.state_version != as_i64(goal.state_version)?
        || row.goal_revision != as_i64(goal.goal_revision)?
        || row.input_revision != as_i64(goal.input_revision)?
        || row.slice_seq != i32::try_from(goal.slice_seq).map_err(|_| invalid())?
        || row.lease_epoch != as_i64(goal.lease_epoch)?
        || row.next_attempt_at != goal.next_attempt_unix_ms.map(as_i64).transpose()?
        || row.absolute_deadline != as_i64(goal.deadline_unix_ms)?
        || row.created_at != as_i64(goal.created_at_unix_ms)?
        || row.updated_at != as_i64(goal.updated_at_unix_ms)?
    {
        return Err(invalid());
    }
    Ok(goal)
}

pub(crate) async fn load_completed_on(
    txn: &DatabaseTransaction,
    goal_id: &str,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
) -> Result<GoalRun, DbErr> {
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(goal_id))
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(actor_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .filter(goal_row::Column::Status.eq("completed"))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    decode(&row)
}

pub(crate) async fn insert_on(txn: &DatabaseTransaction, goal: &GoalRun) -> Result<(), DbErr> {
    project(goal)?.insert(txn).await?;
    Ok(())
}

pub(crate) async fn replace_on(
    txn: &DatabaseTransaction,
    goal: &GoalRun,
    expected_state_version: u64,
    expected_lease_epoch: u64,
    lease_owner: Option<&str>,
    lease_deadline: Option<u64>,
) -> Result<bool, DbErr> {
    let mut next = project(goal)?;
    next.lease_owner = Set(lease_owner.map(str::to_owned));
    next.lease_deadline = Set(lease_deadline.map(as_i64).transpose()?);
    let result = goal_row::Entity::update_many()
        .set(next)
        .filter(goal_row::Column::GoalId.eq(&goal.goal_id))
        .filter(goal_row::Column::StateVersion.eq(as_i64(expected_state_version)?))
        .filter(goal_row::Column::LeaseEpoch.eq(as_i64(expected_lease_epoch)?))
        .exec(txn)
        .await?;
    Ok(result.rows_affected == 1)
}

pub async fn load_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
) -> Result<Option<GoalRun>, DbErr> {
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(actor_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(db)
        .await?;
    row.as_ref().map(decode).transpose()
}

/// Owner-visible latest goal, including a terminal goal that the user may
/// inspect after the assistant has stopped running.
pub async fn load_latest_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
) -> Result<Option<GoalRun>, DbErr> {
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(actor_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .order_by_desc(goal_row::Column::CreatedAt)
        .order_by_desc(goal_row::Column::Id)
        .one(db)
        .await?;
    row.as_ref().map(decode).transpose()
}

pub async fn load_latest_completed_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
) -> Result<Option<GoalRun>, DbErr> {
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(actor_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .filter(goal_row::Column::Status.eq("completed"))
        .order_by_desc(goal_row::Column::CreatedAt)
        .order_by_desc(goal_row::Column::Id)
        .one(db)
        .await?;
    row.as_ref().map(decode).transpose()
}

/// Apply an owner action only between segments. The session version and goal
/// state change together, so a concurrent coordinator claim cannot observe a
/// paused or cancelled goal with a still-claimable old conversation state.
pub async fn apply_owner_action(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
    goal_id: &str,
    expected_state_version: u64,
    action: GoalOwnerAction,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let Some(session_row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .filter(agent_session::Column::ActorId.eq(actor_id))
        .filter(agent_session::Column::DeviceId.eq(device_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut session =
        PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
    if session.surface != AgentSessionSurface::AiAssistant
        || !(session.turn_state.can_claim() || session.turn_state == TurnState::AwaitingApproval)
        || session_row
            .lease_deadline
            .as_ref()
            .is_some_and(|deadline| deadline >= &now)
    {
        return Ok(None);
    }
    let Some(row) = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(goal_id))
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(actor_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut goal = decode(&row)?;
    if goal.state_version != expected_state_version
        || goal.input_revision != session.input_revision
        || goal.state == GoalState::Running
    {
        return Ok(None);
    }
    if action == GoalOwnerAction::Resume
        && goal.state == GoalState::Paused(GoalPauseReason::Recovery)
        && (session.execution_state != desk_diagnose_core::session::ExecutionState::None
            || !session.unclosed_tool_call_ids().is_empty()
            || session.permission_requests.iter().any(|request| {
                request.state == desk_diagnose_core::dynamic_run::PermissionRequestState::Pending
            }))
    {
        return Ok(None);
    }
    let previous_lease_epoch = goal.lease_epoch;
    goal.apply_budget_policy(&crate::goal_budget_policy::read(&txn).await?)
        .map_err(|_| invalid())?;
    goal.apply_owner_action(action, now_ms)
        .map_err(|_| invalid())?;
    session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
    session.version = session_row.version.checked_add(1).ok_or_else(invalid)?;
    let ledger = GoalLedgerEvent::new(
        &goal,
        AgentRunEventKind::GoalRevised,
        session.last_event_seq,
        now.to_rfc3339(),
    )
    .map_err(|_| invalid())?;
    let changed = agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
            version: Set(session.version),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(agent_session::Column::Id.eq(session_row.id))
        .filter(agent_session::Column::Version.eq(session_row.version))
        .filter(agent_session::Column::LeaseToken.eq(session_row.lease_token))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1
        || !replace_on(
            &txn,
            &goal,
            expected_state_version,
            previous_lease_epoch,
            None,
            None,
        )
        .await?
    {
        return Err(invalid());
    }
    agent_run_event::ActiveModel {
        event_id: Set(ledger.event.event_id.clone()),
        run_id: Set(goal.conversation_id.clone()),
        event_seq: Set(as_i64(ledger.event.event_seq)?),
        input_revision: Set(as_i64(goal.input_revision)?),
        kind: Set(ledger.event.kind.as_str().into()),
        correlation_id: Set(Some(goal.goal_id.clone())),
        input_seq: Set(None),
        actor_id: Set(Some(goal.owner_id.clone())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&ledger).map_err(|_| invalid())?),
        payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(Some(goal))
}

/// Route a decided permission through its goal before the ordinary permission
/// resume scanner can claim it. The resume row, goal and session version are
/// fenced in one write transaction; a paused goal is never restarted here.
pub async fn wake_for_permission_decision(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
    request_id: &str,
    now: DateTime<Utc>,
) -> Result<GoalPermissionWake, DbErr> {
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let Some(session_row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .filter(agent_session::Column::ActorId.eq(actor_id))
        .filter(agent_session::Column::DeviceId.eq(device_id))
        .one(&txn)
        .await?
    else {
        return Ok(GoalPermissionWake::NoGoal);
    };
    let mut session =
        PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
    if session.surface != AgentSessionSurface::AiAssistant {
        return Ok(GoalPermissionWake::NoGoal);
    }
    let Some(goal_record) = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(actor_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .order_by_desc(goal_row::Column::CreatedAt)
        .order_by_desc(goal_row::Column::Id)
        .one(&txn)
        .await?
    else {
        return Ok(GoalPermissionWake::NoGoal);
    };
    let mut goal = decode(&goal_record)?;
    if goal.state.is_terminal()
        && !goal
            .checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.evidence_ids.iter().any(|id| id == request_id))
    {
        return Ok(GoalPermissionWake::NoGoal);
    }
    if goal.input_revision != session.input_revision
        || !session.permission_requests.iter().any(|request| {
            request.request_id == request_id && request.input_revision == goal.input_revision
        })
    {
        return Ok(GoalPermissionWake::NoGoal);
    }
    if !session
        .permission_requests
        .iter()
        .any(|request| request.request_id == request_id && request.state.is_terminal())
        || !session.permission_decisions.iter().any(|decision| {
            decision.request_id == request_id && decision.input_revision == goal.input_revision
        })
    {
        return Err(invalid());
    }
    let Some(resume) = resume_row::Entity::find()
        .filter(resume_row::Column::RunId.eq(conversation_id))
        .filter(resume_row::Column::ActorId.eq(actor_id))
        .filter(resume_row::Column::DeviceId.eq(device_id))
        .filter(resume_row::Column::RequestId.eq(request_id))
        .one(&txn)
        .await?
    else {
        return Err(invalid());
    };
    if resume.state == "goal_handled" {
        return Ok(GoalPermissionWake::Held);
    }
    if resume.state != "pending" || resume.input_revision != as_i64(goal.input_revision)? {
        return Ok(GoalPermissionWake::Held);
    }
    if goal.state == GoalState::Running {
        // The decision can arrive before the model segment has atomically
        // settled into waiting_approval. Keep the durable resume pending so
        // the scanner retries after that settlement instead of losing it.
        return Ok(GoalPermissionWake::Held);
    }
    let waiting_for_this_request = goal.state == GoalState::Waiting(GoalWaitReason::Approval)
        && goal.status_reason.as_deref() == Some(request_id);
    if waiting_for_this_request
        && (!session.turn_state.can_claim()
            || session_row
                .lease_deadline
                .is_some_and(|deadline| deadline >= now))
    {
        // The segment has not settled yet. Leave the durable resume pending so
        // its scanner can wake this goal after the lease is released.
        return Ok(GoalPermissionWake::Held);
    }
    let can_wake = waiting_for_this_request;
    let outcome = if can_wake {
        let previous_state_version = goal.state_version;
        let previous_lease_epoch = goal.lease_epoch;
        goal.wake_from(GoalWaitReason::Approval, now_ms)
            .map_err(|_| invalid())?;
        session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
        session.version = session_row.version.checked_add(1).ok_or_else(invalid)?;
        let ledger = GoalLedgerEvent::new(
            &goal,
            AgentRunEventKind::GoalRevised,
            session.last_event_seq,
            now.to_rfc3339(),
        )
        .map_err(|_| invalid())?;
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
                version: Set(session.version),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(session_row.id))
            .filter(agent_session::Column::Version.eq(session_row.version))
            .filter(agent_session::Column::LeaseToken.eq(session_row.lease_token))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1
            || !replace_on(
                &txn,
                &goal,
                previous_state_version,
                previous_lease_epoch,
                None,
                None,
            )
            .await?
        {
            return Err(invalid());
        }
        agent_run_event::ActiveModel {
            event_id: Set(ledger.event.event_id.clone()),
            run_id: Set(goal.conversation_id.clone()),
            event_seq: Set(as_i64(ledger.event.event_seq)?),
            input_revision: Set(as_i64(goal.input_revision)?),
            kind: Set(ledger.event.kind.as_str().into()),
            correlation_id: Set(Some(goal.goal_id.clone())),
            input_seq: Set(None),
            actor_id: Set(Some(goal.owner_id.clone())),
            source_envelope_ids_json: Set("[]".into()),
            result_envelope_ids_json: Set("[]".into()),
            payload_json: Set(serde_json::to_string(&ledger).map_err(|_| invalid())?),
            payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        GoalPermissionWake::Queued
    } else {
        GoalPermissionWake::Held
    };
    let changed = resume_row::Entity::update_many()
        .col_expr(resume_row::Column::State, Expr::value("goal_handled"))
        .col_expr(
            resume_row::Column::Version,
            Expr::value(resume.version.checked_add(1).ok_or_else(invalid)?),
        )
        .col_expr(resume_row::Column::UpdatedAt, Expr::value(now))
        .filter(resume_row::Column::Id.eq(resume.id))
        .filter(resume_row::Column::Version.eq(resume.version))
        .filter(resume_row::Column::State.eq("pending"))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    txn.commit().await?;
    Ok(outcome)
}

/// Bounded, durable FIFO scan. A candidate is rechecked and fenced by
/// `claim_slice`; this read by itself never owns a planning turn.
pub async fn queued_candidates(
    db: &DatabaseConnection,
    now_unix_ms: u64,
    limit: u64,
) -> Result<Vec<GoalRun>, DbErr> {
    let now = as_i64(now_unix_ms)?;
    let rows = goal_row::Entity::find()
        .filter(goal_row::Column::Status.eq("queued"))
        .filter(goal_row::Column::CreatedAt.gt(active_deadline_cutoff(db, now_unix_ms).await?))
        .filter(
            Condition::any()
                .add(goal_row::Column::NextAttemptAt.is_null())
                .add(goal_row::Column::NextAttemptAt.lte(now)),
        )
        .order_by_asc(goal_row::Column::UpdatedAt)
        .order_by_asc(goal_row::Column::Id)
        .limit(limit.min(128))
        .all(db)
        .await?;
    rows.iter().map(decode).collect()
}

/// Device waits have their own due-time queue so offline goals cannot occupy
/// the runnable FIFO indefinitely.
pub async fn waiting_device_candidates(
    db: &DatabaseConnection,
    now_unix_ms: u64,
    limit: u64,
) -> Result<Vec<GoalRun>, DbErr> {
    let now = as_i64(now_unix_ms)?;
    let rows = goal_row::Entity::find()
        .filter(goal_row::Column::Status.eq("waiting_device"))
        .filter(goal_row::Column::CreatedAt.gt(active_deadline_cutoff(db, now_unix_ms).await?))
        .filter(goal_row::Column::NextAttemptAt.lte(now))
        .order_by_asc(goal_row::Column::NextAttemptAt)
        .order_by_asc(goal_row::Column::Id)
        .limit(limit.min(128))
        .all(db)
        .await?;
    rows.iter().map(decode).collect()
}

pub async fn waiting_model_candidates(
    db: &DatabaseConnection,
    now_unix_ms: u64,
    limit: u64,
) -> Result<Vec<GoalRun>, DbErr> {
    let now = as_i64(now_unix_ms)?;
    let rows = goal_row::Entity::find()
        .filter(goal_row::Column::Status.eq("waiting_model"))
        .filter(goal_row::Column::CreatedAt.gt(active_deadline_cutoff(db, now_unix_ms).await?))
        .filter(goal_row::Column::NextAttemptAt.lte(now))
        .order_by_asc(goal_row::Column::NextAttemptAt)
        .order_by_asc(goal_row::Column::Id)
        .limit(limit.min(128))
        .all(db)
        .await?;
    rows.iter().map(decode).collect()
}

/// Completed background work is detected from the server-written completion
/// message, then fenced against the goal and session in `scheduler_transition`.
pub async fn waiting_work_candidates(
    db: &DatabaseConnection,
    now_unix_ms: u64,
    limit: u64,
) -> Result<Vec<GoalRun>, DbErr> {
    let rows = goal_row::Entity::find()
        .filter(goal_row::Column::Status.eq("waiting_work"))
        .filter(goal_row::Column::CreatedAt.gt(active_deadline_cutoff(db, now_unix_ms).await?))
        .filter(
            Condition::any()
                .add(goal_row::Column::NextAttemptAt.is_null())
                .add(goal_row::Column::NextAttemptAt.lte(as_i64(now_unix_ms)?)),
        )
        .order_by_asc(goal_row::Column::NextAttemptAt)
        .order_by_asc(goal_row::Column::UpdatedAt)
        .order_by_asc(goal_row::Column::Id)
        .limit(limit.min(128))
        .all(db)
        .await?;
    rows.iter().map(decode).collect()
}

/// Expired settled goals are terminalized independently of the runnable FIFO.
/// Running slices are left to the lease/orphan reconciler, which must first
/// fence their session and account for any dispatched or unknown work.
pub async fn expire_due(
    db: &DatabaseConnection,
    now: DateTime<Utc>,
    limit: u64,
) -> Result<(), DbErr> {
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let policy = crate::goal_budget_policy::read(db).await?;
    let Some(deadline_ms) = policy.limits.deadline_ms else {
        return Ok(());
    };
    let rows = goal_row::Entity::find()
        .filter(goal_row::Column::CreatedAt.lte(as_i64(now_ms.saturating_sub(deadline_ms))?))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled", "running"]))
        .order_by_asc(goal_row::Column::AbsoluteDeadline)
        .limit(limit.min(128))
        .all(db)
        .await?;
    for row in rows {
        let goal = decode(&row)?;
        if let Err(error) = expire_settled(db, &goal, now.clone()).await {
            log::warn!(
                "[ai-assistant-goal] failed to expire goal {}: {error}",
                goal.goal_id
            );
        }
    }
    Ok(())
}

async fn expire_settled(
    db: &DatabaseConnection,
    expected: &GoalRun,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let Some(row) = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(&expected.goal_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut goal = decode(&row)?;
    goal.apply_budget_policy(&crate::goal_budget_policy::read(&txn).await?)
        .map_err(|_| invalid())?;
    if goal.state_version != expected.state_version
        || goal.state.is_terminal()
        || goal.state == GoalState::Running
        || now_ms < goal.deadline_unix_ms
    {
        return Ok(None);
    }
    let Some(session_row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&goal.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&goal.owner_id))
        .filter(agent_session::Column::DeviceId.eq(&goal.device_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut session =
        PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
    if session.surface != AgentSessionSurface::AiAssistant
        || session.input_revision != goal.input_revision
        || !(session.turn_state.can_claim() || session.turn_state == TurnState::AwaitingApproval)
        || session_row
            .lease_deadline
            .as_ref()
            .is_some_and(|deadline| deadline >= &now)
    {
        return Ok(None);
    }
    let previous_version = goal.state_version;
    let previous_epoch = goal.lease_epoch;
    goal.fail_expired_settled(now_ms).map_err(|_| invalid())?;
    session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
    session.version = session_row.version.checked_add(1).ok_or_else(invalid)?;
    session.updated_at = now.to_rfc3339();
    let ledger = GoalLedgerEvent::new(
        &goal,
        AgentRunEventKind::GoalRevised,
        session.last_event_seq,
        now.to_rfc3339(),
    )
    .map_err(|_| invalid())?;
    let changed = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().map_err(|_| invalid())?),
        )
        .col_expr(agent_session::Column::Version, Expr::value(session.version))
        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
        .filter(agent_session::Column::Id.eq(session_row.id))
        .filter(agent_session::Column::Version.eq(session_row.version))
        .filter(agent_session::Column::LeaseToken.eq(session_row.lease_token))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1
        || !replace_on(&txn, &goal, previous_version, previous_epoch, None, None).await?
    {
        return Err(invalid());
    }
    agent_run_event::ActiveModel {
        event_id: Set(ledger.event.event_id.clone()),
        run_id: Set(goal.conversation_id.clone()),
        event_seq: Set(as_i64(ledger.event.event_seq)?),
        input_revision: Set(as_i64(goal.input_revision)?),
        kind: Set(ledger.event.kind.as_str().into()),
        correlation_id: Set(Some(goal.goal_id.clone())),
        input_seq: Set(None),
        actor_id: Set(Some(goal.owner_id.clone())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&ledger).map_err(|_| invalid())?),
        payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(Some(goal))
}

/// Change only the scheduler-visible device wait state. The goal, conversation
/// revision and event share one SQLite write transaction.
pub async fn update_device_availability(
    db: &DatabaseConnection,
    expected: &GoalRun,
    available: bool,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    scheduler_transition(db, expected, SchedulerTransition::Device(available), now).await
}

pub async fn wait_for_model(
    db: &DatabaseConnection,
    expected: &GoalRun,
    retry_after_unix_ms: Option<u64>,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    scheduler_transition(
        db,
        expected,
        SchedulerTransition::ModelWait(retry_after_unix_ms),
        now,
    )
    .await
}

pub async fn wake_model_due(
    db: &DatabaseConnection,
    expected: &GoalRun,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    scheduler_transition(db, expected, SchedulerTransition::ModelWake, now).await
}

pub async fn block_model_before_claim(
    db: &DatabaseConnection,
    expected: &GoalRun,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    scheduler_transition(db, expected, SchedulerTransition::ModelBlock, now).await
}

pub async fn wake_for_work_completion(
    db: &DatabaseConnection,
    expected: &GoalRun,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    scheduler_transition(db, expected, SchedulerTransition::WorkWake, now).await
}

pub async fn pause_if_unresolved_work(
    db: &DatabaseConnection,
    expected: &GoalRun,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    scheduler_transition(db, expected, SchedulerTransition::RecoveryPause, now).await
}

pub async fn pause_if_missing_attachments(
    db: &DatabaseConnection,
    expected: &GoalRun,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    scheduler_transition(db, expected, SchedulerTransition::AttachmentCheck, now).await
}

async fn attachments_available_on(
    txn: &DatabaseTransaction,
    goal: &GoalRun,
    session: &PersistedAgentSession,
) -> Result<bool, DbErr> {
    let mut required = std::collections::BTreeSet::new();
    required.extend(session.focus_epoch.selected_attachment_ids.iter().cloned());
    if let Some(checkpoint) = &goal.checkpoint {
        required.extend(checkpoint.protected_attachment_ids.iter().cloned());
    }
    if let Some(previous) = &goal.previous_completion {
        required.extend(previous.protected_attachment_ids.iter().cloned());
    }
    if required.is_empty() {
        return Ok(true);
    }
    let available = attachment_row::Entity::find()
        .select_only()
        .column(attachment_row::Column::Id)
        .filter(attachment_row::Column::ConversationId.eq(&goal.conversation_id))
        .filter(attachment_row::Column::ActorId.eq(&goal.owner_id))
        .filter(attachment_row::Column::DeviceId.eq(&goal.device_id))
        .filter(attachment_row::Column::Available.eq(true))
        .filter(attachment_row::Column::Id.is_in(required.iter().cloned()))
        .into_tuple::<String>()
        .all(txn)
        .await?;
    Ok(required.len() == available.len() && available.iter().all(|id| required.contains(id)))
}

#[derive(Clone, Copy)]
enum SchedulerTransition {
    BudgetPause,
    Device(bool),
    ModelWait(Option<u64>),
    ModelWake,
    ModelBlock,
    WorkWake,
    RecoveryPause,
    AttachmentCheck,
}

async fn scheduler_transition(
    db: &DatabaseConnection,
    expected: &GoalRun,
    transition: SchedulerTransition,
    now: DateTime<Utc>,
) -> Result<Option<GoalRun>, DbErr> {
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let Some(goal_record) = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(&expected.goal_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut goal = decode(&goal_record)?;
    goal.apply_budget_policy(&crate::goal_budget_policy::read(&txn).await?)
        .map_err(|_| invalid())?;
    if now_ms >= goal.deadline_unix_ms {
        return Ok(None);
    }
    if goal.state_version != expected.state_version
        || goal.input_revision != expected.input_revision
        || goal.goal_revision != expected.goal_revision
        || goal.owner_id != expected.owner_id
        || goal.device_id != expected.device_id
    {
        return Ok(None);
    }
    let Some(session_row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&goal.conversation_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut session =
        PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
    if session.actor_id != goal.owner_id
        || session.device_id != goal.device_id
        || session.input_revision != goal.input_revision
        || !session.turn_state.can_claim()
        || session_row
            .lease_deadline
            .is_some_and(|deadline| deadline >= now)
    {
        return Ok(None);
    }
    let previous_state_version = goal.state_version;
    let previous_lease_epoch = goal.lease_epoch;
    let completed_work_event_id = goal.completed_work_event_id(&session).map(str::to_owned);
    match (goal.state, transition) {
        (GoalState::Queued, SchedulerTransition::BudgetPause)
            if !goal.next_slice_budget_available() =>
        {
            goal.pause_settled(GoalPauseReason::Budget, now_ms)
                .and_then(|_| {
                    goal.status_reason = Some("budget_exhausted".into());
                    goal.validate()
                })
        }
        (GoalState::Queued, SchedulerTransition::Device(false)) => goal.wait_for_device(now_ms),
        (GoalState::Waiting(GoalWaitReason::Device), SchedulerTransition::Device(false)) => {
            goal.defer_device_recheck(now_ms)
        }
        (GoalState::Waiting(GoalWaitReason::Device), SchedulerTransition::Device(true)) => {
            goal.wake_from(GoalWaitReason::Device, now_ms)
        }
        (GoalState::Queued, SchedulerTransition::ModelWait(retry_after)) => {
            goal.wait_for_model(now_ms, retry_after)
        }
        (GoalState::Queued, SchedulerTransition::ModelBlock) => {
            goal.block_model_before_claim(now_ms)
        }
        (GoalState::Waiting(GoalWaitReason::Model), SchedulerTransition::ModelWake)
            if goal.next_attempt_unix_ms.is_some_and(|due| due <= now_ms) =>
        {
            goal.wake_from(GoalWaitReason::Model, now_ms)
        }
        (GoalState::Waiting(GoalWaitReason::Work), SchedulerTransition::WorkWake)
            if completed_work_event_id.is_some() =>
        {
            goal.wake_from(GoalWaitReason::Work, now_ms)
        }
        (GoalState::Waiting(GoalWaitReason::Work), SchedulerTransition::WorkWake) => {
            goal.defer_work_recheck(now_ms)
        }
        (GoalState::Queued, SchedulerTransition::RecoveryPause)
            if session.execution_state != desk_diagnose_core::session::ExecutionState::None
                || !session.unclosed_tool_call_ids().is_empty() =>
        {
            goal.pause_for_unresolved_work(now_ms)
        }
        (GoalState::Queued, SchedulerTransition::AttachmentCheck)
            if !attachments_available_on(&txn, &goal, &session).await? =>
        {
            goal.pause_settled(GoalPauseReason::AttachmentMissing, now_ms)
                .and_then(|_| {
                    goal.status_reason = Some("attachment_missing".into());
                    goal.validate()
                })
        }
        _ => return Ok(None),
    }
    .map_err(|_| invalid())?;
    if goal.state == GoalState::Queued
        && matches!(transition, SchedulerTransition::WorkWake)
        && let Some(event_id) = completed_work_event_id
    {
        session.remove_pending_auto_trigger(&event_id);
    }
    let event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
    session.last_event_seq = event_seq;
    session.version = session_row.version.checked_add(1).ok_or_else(invalid)?;
    let ledger = GoalLedgerEvent::new(
        &goal,
        AgentRunEventKind::GoalRevised,
        event_seq,
        now.to_rfc3339(),
    )
    .map_err(|_| invalid())?;
    let changed = agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
            version: Set(session.version),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(agent_session::Column::Id.eq(session_row.id))
        .filter(agent_session::Column::Version.eq(session_row.version))
        .filter(agent_session::Column::LeaseToken.eq(session_row.lease_token))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1
        || !replace_on(
            &txn,
            &goal,
            previous_state_version,
            previous_lease_epoch,
            None,
            None,
        )
        .await?
    {
        return Err(invalid());
    }
    agent_run_event::ActiveModel {
        event_id: Set(ledger.event.event_id.clone()),
        run_id: Set(goal.conversation_id.clone()),
        event_seq: Set(as_i64(event_seq)?),
        input_revision: Set(as_i64(goal.input_revision)?),
        kind: Set(ledger.event.kind.as_str().into()),
        correlation_id: Set(Some(goal.goal_id.clone())),
        input_seq: Set(None),
        actor_id: Set(Some(goal.owner_id.clone())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&ledger).map_err(|_| invalid())?),
        payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(Some(goal))
}

pub async fn load_claimed(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
) -> Result<GoalRun, DbErr> {
    let segment = session
        .focus_epoch
        .goal_segment
        .as_ref()
        .ok_or_else(invalid)?;
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(&segment.goal_id))
        .filter(goal_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(goal_row::Column::ActorId.eq(&session.actor_id))
        .filter(goal_row::Column::DeviceId.eq(&session.device_id))
        .filter(goal_row::Column::Status.eq("running"))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let mut goal = decode(&row)?;
    let now_ms = Utc::now().timestamp_millis();
    if session.trigger_origin != TriggerOrigin::GoalContinuation
        || goal.input_revision != session.input_revision
        || goal.slice_seq != segment.segment_seq
        || goal.source_message_id != segment.source_message_id
        || goal
            .require_fence(
                session.input_revision,
                segment.goal_revision,
                segment.lease_epoch,
            )
            .is_err()
        || row.lease_owner.is_none()
        || row.lease_deadline.is_none_or(|deadline| deadline < now_ms)
    {
        return Err(invalid());
    }
    goal.apply_budget_policy(&crate::goal_budget_policy::read(db).await?)
        .map_err(|_| invalid())?;
    Ok(goal)
}

enum BudgetMutation {
    Reserve(GoalUsage),
    Settle(GoalUsage),
}

async fn mutate_budget(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    identity: &str,
    mutation: BudgetMutation,
    now: DateTime<Utc>,
) -> Result<bool, DbErr> {
    let segment = session
        .focus_epoch
        .goal_segment
        .as_ref()
        .ok_or_else(invalid)?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let session_row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    if session_row.lease_token != as_i64(session.lease_token)?
        || session_row
            .lease_deadline
            .is_none_or(|deadline| deadline < now)
    {
        return Err(invalid());
    }
    let goal_record = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(&segment.goal_id))
        .filter(goal_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(goal_row::Column::ActorId.eq(&session.actor_id))
        .filter(goal_row::Column::DeviceId.eq(&session.device_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut goal = decode(&goal_record)?;
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    if goal
        .require_fence(
            session.input_revision,
            segment.goal_revision,
            segment.lease_epoch,
        )
        .is_err()
        || goal.slice_seq != segment.segment_seq
        || goal.source_message_id != segment.source_message_id
        || goal_record.lease_owner.is_none()
        || goal_record
            .lease_deadline
            .is_none_or(|deadline| deadline < now.timestamp_millis())
    {
        return Err(invalid());
    }
    let prior_version = goal.state_version;
    let prior_epoch = goal.lease_epoch;
    goal.apply_budget_policy(&crate::goal_budget_policy::read(&txn).await?)
        .map_err(|_| invalid())?;
    let changed = match mutation {
        BudgetMutation::Reserve(delta) => goal.reserve_with_id(identity, delta, now_ms),
        BudgetMutation::Settle(actual) => goal.settle_with_id(identity, actual, now_ms),
    }
    .map_err(|_| invalid())?;
    if !changed {
        return Ok(false);
    }
    if !replace_on(
        &txn,
        &goal,
        prior_version,
        prior_epoch,
        goal_record.lease_owner.as_deref(),
        goal_record
            .lease_deadline
            .map(|value| u64::try_from(value).map_err(|_| invalid()))
            .transpose()?,
    )
    .await?
    {
        return Err(invalid());
    }
    txn.commit().await?;
    Ok(true)
}

pub async fn reserve_budget(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    identity: &str,
    upper: GoalUsage,
    now: DateTime<Utc>,
) -> Result<bool, DbErr> {
    mutate_budget(db, session, identity, BudgetMutation::Reserve(upper), now).await
}

pub async fn settle_budget(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    identity: &str,
    actual: GoalUsage,
    now: DateTime<Utc>,
) -> Result<bool, DbErr> {
    mutate_budget(db, session, identity, BudgetMutation::Settle(actual), now).await
}

pub struct ClaimedGoalSlice {
    pub goal: GoalRun,
    pub session: PersistedAgentSession,
}

/// Claim both leases in one SQLite write transaction. A busy conversation is
/// left untouched; orphan recovery is performed by the existing session/work
/// reconciler before the coordinator tries another claim.
pub async fn claim_slice(
    db: &DatabaseConnection,
    params: &ClaimTurnParams,
    expected_goal_id: &str,
    expected_state_version: Option<u64>,
    coordinator_id: &str,
) -> Result<Option<ClaimedGoalSlice>, DbErr> {
    if params.trigger_origin != TriggerOrigin::GoalContinuation
        || expected_goal_id.is_empty()
        || coordinator_id.is_empty()
    {
        return Err(invalid());
    }
    let now = DateTime::parse_from_rfc3339(&params.now)
        .map_err(|_| invalid())?
        .with_timezone(&Utc);
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let Some(session_row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&params.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&params.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&params.device_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut session =
        PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
    if session.surface != AgentSessionSurface::AiAssistant
        || !session.turn_state.can_claim()
        || session.execution_state != desk_diagnose_core::session::ExecutionState::None
        || !session.unclosed_tool_call_ids().is_empty()
        || session_row
            .lease_deadline
            .as_ref()
            .is_some_and(|deadline| deadline >= &now)
    {
        return Ok(None);
    }
    desk_diagnose_core::assistant_policy::validate_claim(
        session.surface,
        Some(session.policy_revision),
        params,
    )
    .map_err(|_| invalid())?;
    let Some(row) = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(expected_goal_id))
        .filter(goal_row::Column::ConversationId.eq(&params.conversation_id))
        .filter(goal_row::Column::Status.eq("queued"))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut goal = decode(&row)?;
    if expected_state_version.is_some_and(|version| goal.state_version != version) {
        return Ok(None);
    }
    if goal.owner_id != params.actor_id
        || goal.device_id != params.device_id
        || goal.input_revision != session.input_revision
    {
        return Err(invalid());
    }
    if !attachments_available_on(&txn, &goal, &session).await? {
        return Ok(None);
    }
    if goal_row::Entity::find()
        .filter(goal_row::Column::Status.eq("running"))
        .filter(
            Condition::any()
                .add(goal_row::Column::ActorId.eq(&goal.owner_id))
                .add(goal_row::Column::DeviceId.eq(&goal.device_id)),
        )
        .one(&txn)
        .await?
        .is_some()
    {
        return Ok(None);
    }
    let previous_state_version = goal.state_version;
    let previous_lease_epoch = goal.lease_epoch;
    goal.apply_budget_policy(&crate::goal_budget_policy::read(&txn).await?)
        .map_err(|_| invalid())?;
    if now_ms >= goal.deadline_unix_ms {
        return Ok(None);
    }
    if !goal.next_slice_budget_available() {
        txn.rollback().await?;
        scheduler_transition(db, &goal, SchedulerTransition::BudgetPause, now).await?;
        return Ok(None);
    }
    for event_id in goal.owned_pending_work_event_ids(&session) {
        session.remove_pending_auto_trigger(&event_id);
    }
    let (slice_seq, _) = goal.claim_slice(now_ms).map_err(|_| invalid())?;
    session
        .begin_goal_segment(
            &goal.goal_id,
            &goal.source_message_id,
            slice_seq,
            goal.goal_revision,
            goal.lease_epoch,
        )
        .map_err(|_| invalid())?;
    session
        .begin_turn(
            params.turn_id.clone(),
            params.request_id.clone(),
            params.connection_id.clone(),
            params.policy_revision,
            params.current_pdp_scope.clone(),
            params.now.clone(),
        )
        .map_err(|_| invalid())?;
    session.adopt_trigger(TriggerOrigin::GoalContinuation, &params.turn_id);
    session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
    let ledger = GoalLedgerEvent::new(
        &goal,
        AgentRunEventKind::GoalSliceClaimed,
        session.last_event_seq,
        params.now.clone(),
    )
    .map_err(|_| invalid())?;
    session.version = session_row.version.checked_add(1).ok_or_else(invalid)?;
    let session_state = session.encode_json_for_storage().map_err(|_| invalid())?;
    let deadline = now + Duration::seconds(90);
    let changed = agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            state_json: Set(session_state),
            version: Set(session.version),
            lease_token: Set(as_i64(session.lease_token)?),
            lease_deadline: Set(Some(deadline)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(agent_session::Column::Id.eq(session_row.id))
        .filter(agent_session::Column::Version.eq(session_row.version))
        .filter(agent_session::Column::LeaseToken.eq(session_row.lease_token))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1
        || !replace_on(
            &txn,
            &goal,
            previous_state_version,
            previous_lease_epoch,
            Some(coordinator_id),
            Some(u64::try_from(deadline.timestamp_millis()).map_err(|_| invalid())?),
        )
        .await?
    {
        return Err(invalid());
    }
    agent_run_event::ActiveModel {
        event_id: Set(ledger.event.event_id.clone()),
        run_id: Set(goal.conversation_id.clone()),
        event_seq: Set(as_i64(ledger.event.event_seq)?),
        input_revision: Set(as_i64(goal.input_revision)?),
        kind: Set(ledger.event.kind.as_str().into()),
        correlation_id: Set(Some(goal.goal_id.clone())),
        input_seq: Set(None),
        actor_id: Set(Some(goal.owner_id.clone())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&ledger).map_err(|_| invalid())?),
        payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(Some(ClaimedGoalSlice { goal, session }))
}

/// Renew the goal and session deadlines together. `None` means this is an
/// ordinary turn; `Some(false)` means a goal turn lost its fence and the caller
/// must stop using the claimed session.
pub async fn heartbeat_goal_if_running(
    db: &DatabaseConnection,
    conversation_id: &str,
    session_lease_token: u64,
    now: DateTime<Utc>,
) -> Result<Option<bool>, DbErr> {
    let running = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::Status.eq("running"))
        .one(db)
        .await?;
    if running.is_none() {
        return Ok(None);
    }
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let Some(goal_record) = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::Status.eq("running"))
        .one(&txn)
        .await?
    else {
        return Ok(Some(false));
    };
    let goal = decode(&goal_record)?;
    let Some(session_row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .one(&txn)
        .await?
    else {
        return Ok(Some(false));
    };
    let session =
        PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
    if session_row.lease_token != as_i64(session_lease_token)?
        || !session.turn_state.is_active()
        || session
            .focus_epoch
            .goal_segment
            .as_ref()
            .is_none_or(|segment| {
                segment.goal_id != goal.goal_id
                    || segment.segment_seq != goal.slice_seq
                    || segment.source_message_id != goal.source_message_id
                    || segment.goal_revision != goal.goal_revision
                    || segment.lease_epoch != goal.lease_epoch
            })
        || goal_record.lease_owner.is_none()
        || goal_record.lease_deadline.is_none()
        || goal_record.lease_epoch != as_i64(goal.lease_epoch)?
    {
        return Ok(Some(false));
    }
    let deadline = now + Duration::seconds(90);
    let deadline_ms = as_i64(u64::try_from(deadline.timestamp_millis()).map_err(|_| invalid())?)?;
    let session_changed = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::LeaseDeadline,
            Expr::value(Some(deadline)),
        )
        .filter(agent_session::Column::Id.eq(session_row.id))
        .filter(agent_session::Column::LeaseToken.eq(as_i64(session_lease_token)?))
        .exec(&txn)
        .await?;
    let goal_changed = goal_row::Entity::update_many()
        .col_expr(
            goal_row::Column::LeaseDeadline,
            Expr::value(Some(deadline_ms)),
        )
        .filter(goal_row::Column::GoalId.eq(&goal.goal_id))
        .filter(goal_row::Column::StateVersion.eq(as_i64(goal.state_version)?))
        .filter(goal_row::Column::LeaseEpoch.eq(as_i64(goal.lease_epoch)?))
        .filter(
            goal_row::Column::LeaseOwner.eq(goal_record.lease_owner.as_deref().unwrap_or_default()),
        )
        .filter(goal_row::Column::Status.eq("running"))
        .exec(&txn)
        .await?;
    if session_changed.rows_affected != 1 || goal_changed.rows_affected != 1 {
        return Ok(Some(false));
    }
    txn.commit().await?;
    Ok(Some(true))
}

fn no_pending_work(session: &PersistedAgentSession) -> bool {
    use desk_diagnose_core::dynamic_run::PermissionRequestState;
    session.unclosed_tool_call_ids().is_empty()
        && session.execution_state.tasks().is_empty()
        && !session.execution_state.has_unresolved_outcome()
        && !session
            .permission_requests
            .iter()
            .any(|request| request.state == PermissionRequestState::Pending)
}

fn control_has_required_fact(session: &PersistedAgentSession, control: &GoalControl) -> bool {
    use desk_diagnose_core::dynamic_run::PermissionRequestState;
    match control {
        GoalControl::Wait {
            reason: GoalWaitReason::Approval,
            reference_id,
        } => session.permission_requests.iter().any(|request| {
            request.request_id == *reference_id && request.state == PermissionRequestState::Pending
        }),
        GoalControl::Wait {
            reason: GoalWaitReason::Work,
            reference_id,
        } => session
            .execution_state
            .tasks()
            .iter()
            .any(|task| task.action_request_id == *reference_id),
        GoalControl::Complete { .. } => no_pending_work(session),
        _ => true,
    }
}

/// Commit the end of one segment, its session transition, and its ledger event
/// in one SQLite transaction. The caller has already closed the in-memory turn
/// and passes server-derived progress, never the model's self-assessment.
pub async fn settle_slice(
    db: &DatabaseConnection,
    session: &mut PersistedAgentSession,
    end: &GoalSegmentEnd,
    result_fingerprints: &[String],
    now: DateTime<Utc>,
) -> Result<GoalRun, DbErr> {
    let segment = session
        .focus_epoch
        .goal_segment
        .as_ref()
        .ok_or_else(invalid)?
        .clone();
    if session.trigger_origin != TriggerOrigin::GoalContinuation
        || !session.turn_state.is_settled()
        || matches!(end, GoalSegmentEnd::Control(control) if !control_has_required_fact(session, control))
    {
        return Err(invalid());
    }
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let session_row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    if session_row.version != session.version
        || session_row.lease_token != as_i64(session.lease_token)?
        || session_row
            .lease_deadline
            .is_none_or(|deadline| deadline < now)
    {
        return Err(invalid());
    }
    let persisted =
        PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
    if !persisted.turn_state.is_active()
        || persisted.input_revision != session.input_revision
        || persisted.focus_epoch.goal_segment.as_ref() != Some(&segment)
    {
        return Err(invalid());
    }
    let goal_record = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq(&segment.goal_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut goal = decode(&goal_record)?;
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let now_ms_i64 = as_i64(now_ms)?;
    if goal.conversation_id != session.conversation_id
        || goal_record.lease_owner.is_none()
        || goal_record
            .lease_deadline
            .is_none_or(|deadline| deadline < now_ms_i64)
        || goal.slice_seq != segment.segment_seq
        || goal.source_message_id != segment.source_message_id
        || goal
            .require_fence(
                session.input_revision,
                segment.goal_revision,
                segment.lease_epoch,
            )
            .is_err()
    {
        return Err(invalid());
    }
    let previous_state_version = goal.state_version;
    let previous_lease_epoch = goal.lease_epoch;
    goal.apply_budget_policy(&crate::goal_budget_policy::read(&txn).await?)
        .map_err(|_| invalid())?;
    let progressed = goal
        .observe_result_fingerprints(result_fingerprints)
        .map_err(|_| invalid())?;
    let event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
    match end {
        GoalSegmentEnd::Control(control) => goal.finish_slice(
            session.input_revision,
            segment.goal_revision,
            segment.lease_epoch,
            control,
            progressed,
            no_pending_work(session),
            event_seq,
            session.focus_epoch.selected_attachment_ids.clone(),
            now_ms,
        ),
        GoalSegmentEnd::Pause(reason) => goal.finish_paused_slice(
            session.input_revision,
            segment.goal_revision,
            segment.lease_epoch,
            *reason,
            event_seq,
            session.focus_epoch.selected_attachment_ids.clone(),
            now_ms,
        ),
        GoalSegmentEnd::WaitForModel {
            retry_after_unix_ms,
        } => goal.finish_model_wait_slice(
            session.input_revision,
            segment.goal_revision,
            segment.lease_epoch,
            *retry_after_unix_ms,
            event_seq,
            session.focus_epoch.selected_attachment_ids.clone(),
            now_ms,
        ),
        GoalSegmentEnd::BlockModel => goal.finish_blocked_model_slice(
            session.input_revision,
            segment.goal_revision,
            segment.lease_epoch,
            event_seq,
            session.focus_epoch.selected_attachment_ids.clone(),
            now_ms,
        ),
    }
    .map_err(|_| invalid())?;
    session.last_event_seq = event_seq;
    let ledger = GoalLedgerEvent::new(
        &goal,
        AgentRunEventKind::GoalSliceSettled,
        event_seq,
        now.to_rfc3339(),
    )
    .map_err(|_| invalid())?;
    session.version = session_row.version.checked_add(1).ok_or_else(invalid)?;
    let changed = agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
            version: Set(session.version),
            lease_deadline: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(agent_session::Column::Id.eq(session_row.id))
        .filter(agent_session::Column::Version.eq(session_row.version))
        .filter(agent_session::Column::LeaseToken.eq(as_i64(session.lease_token)?))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1
        || !replace_on(
            &txn,
            &goal,
            previous_state_version,
            previous_lease_epoch,
            None,
            None,
        )
        .await?
    {
        return Err(invalid());
    }
    agent_run_event::ActiveModel {
        event_id: Set(ledger.event.event_id.clone()),
        run_id: Set(goal.conversation_id.clone()),
        event_seq: Set(as_i64(event_seq)?),
        input_revision: Set(as_i64(goal.input_revision)?),
        kind: Set(ledger.event.kind.as_str().into()),
        correlation_id: Set(Some(goal.goal_id.clone())),
        input_seq: Set(None),
        actor_id: Set(Some(goal.owner_id.clone())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&ledger).map_err(|_| invalid())?),
        payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(goal)
}
