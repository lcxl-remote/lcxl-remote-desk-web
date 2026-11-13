//! Task-first locks for actions originating in a scheduled continuation.
use super::{
    ScheduleStoreError,
    authority::{CurrentTaskAuthority, authority_now, lock_run_authority},
    session::{OrgMode, ScheduleHostStores, SessionKey, SessionRow},
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_agent_protocol::schedule::SchedulePauseReason;
use desk_diagnose_core::{
    schedule::lifecycle::FailureState,
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, sea_query::Expr,
};

/// A boxed fence future kept off its callers' stack frames.
pub type Fence<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, DbErr>> + Send + 'a>>;

/// A scheduled continuation acquires the SQLite write lock by touching its task.
/// Validation and the caller's action/outbox mutation share that transaction;
/// an intervening writer cannot turn a stale read snapshot into a valid dispatch.
// Keep child execution state on the heap instead of embedding it in each caller.
#[inline(never)]
pub fn lock_action_session<'a, C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &'a H,
    txn: &'a C,
    conversation_id: &'a str,
) -> Fence<'a, Option<H::Row>> {
    desk_diagnose_core::future::send_phase(|| async move {
        Ok(lock_action_session_row(host, txn, conversation_id)
            .await?
            .map(|row| row.raw))
    })
}

/// The session row of an action transaction, with every organization session
/// returned as before; scheduled sessions are joined to their task fences.
#[inline(never)]
pub(super) fn lock_action_session_row<'a, C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &'a H,
    txn: &'a C,
    conversation_id: &'a str,
) -> Fence<'a, Option<SessionRow<H::Row>>> {
    desk_diagnose_core::future::send_phase(|| lock_action_session_inner(host, txn, conversation_id))
}

async fn lock_action_session_inner<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    conversation_id: &str,
) -> Result<Option<SessionRow<H::Row>>, DbErr> {
    let peek = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: conversation_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            false,
        )
        .await?;
    let Some(peek) = peek else {
        return Ok(None);
    };
    let initial = decode(&peek)?;
    host.lock_child_source(txn, &initial).await?;
    if initial.trigger_origin == TriggerOrigin::ScheduledTask {
        return desk_diagnose_core::future::send_phase(|| {
            lock_fresh_action_session_on(host, txn, &peek, &initial)
        })
        .await
        .map(Some);
    }
    let binding = if initial.trigger_origin == TriggerOrigin::ScheduledContinuation {
        let run_id = initial.current_request_id.as_deref().ok_or_else(invalid)?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(work.owner_user_id))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(invalid());
        }
        Some((work.schedule_id, run_id.to_owned()))
    } else {
        None
    };
    let row = host
        .find_session(txn, SessionKey::Id(peek.id), OrgMode::Any, true)
        .await?
        .ok_or_else(invalid)?;
    let session = decode(&row)?;
    if (session.trigger_origin == TriggerOrigin::ScheduledContinuation) != binding.is_some() {
        return Err(invalid());
    }
    if let Some((schedule_id, run_id)) = binding {
        desk_diagnose_core::future::send_phase(|| {
            validate_continuation_action_on(
                host,
                txn,
                conversation_id,
                &row,
                &session,
                &schedule_id,
                &run_id,
            )
        })
        .await?;
    }
    Ok(Some(row))
}

async fn lock_fresh_action_session_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    peek: &SessionRow<H::Row>,
    initial: &PersistedAgentSession,
) -> Result<SessionRow<H::Row>, DbErr> {
    desk_diagnose_core::future::send_phase(|| fresh_action_authority_on(txn, initial)).await?;
    let row = host
        .find_session(txn, SessionKey::Id(peek.id), OrgMode::Any, true)
        .await?
        .ok_or_else(invalid)?;
    let current = decode(&row)?;
    if current.as_ref() != initial || !row.personal {
        return Err(invalid());
    }
    let authority =
        desk_diagnose_core::future::send_phase(|| fresh_action_authority_on(txn, &current)).await?;
    if row
        .lease_deadline_ms
        .is_none_or(|deadline| deadline <= authority.verified_at())
    {
        return Err(invalid());
    }
    Ok(row)
}

async fn validate_continuation_action_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    _host: &H,
    txn: &C,
    conversation_id: &str,
    row: &SessionRow<H::Row>,
    session: &PersistedAgentSession,
    schedule_id: &str,
    run_id: &str,
) -> Result<(), DbErr> {
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let failures: FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| invalid())?;
    let now = authority_now(txn).await.map_err(|error| match error {
        ScheduleStoreError::Backend(error) => error,
        _ => invalid(),
    })?;
    if task.kind != "conversation_resume"
        || task.calc_version != desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION
        || session.lease_token == 0
        || !matches!(task.status.as_str(), "active" | "triggered" | "paused")
        || task.active_run_id.as_deref() != Some(run_id)
        || task.owner_user_id != work.owner_user_id
        || task.owner_user_id.to_string() != session.actor_id
        || session
            .scheduled_request
            .as_ref()
            .is_none_or(|binding| binding.execution_target.device_id != task.target_device_id)
        || super::continuation_subject(&task).map_err(|_| invalid())? != session.device_id
        || task.source_conversation_id.as_deref() != Some(conversation_id)
        || task.requirement_revision != i64::try_from(session.input_revision).ok()
        || failures
            .pause_reasons
            .iter()
            .any(|reason| *reason != SchedulePauseReason::User)
        || i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch)
        || work.schedule_id != schedule_id
        || work.status != "running"
        || work.cancel_requested_at.is_some()
        || work.failure_accounted
        || work.lease_epoch <= 0
        || work.lease_owner.as_ref().is_none_or(|node| node.is_empty())
        || work.lease_deadline.is_none_or(|deadline| deadline <= now)
        || work.conversation_id != conversation_id
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.active_control_connection_id.is_some()
        || !session.turn_state.is_active()
        || session.surface != AgentSessionSurface::AiAssistant
        || !row.personal
        || row
            .turn_state
            .as_deref()
            .is_some_and(|state| !matches!(state, "running" | "awaiting_approval"))
        || row.lease_deadline_ms.is_none_or(|deadline| deadline <= now)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Join task authority to the action transaction before taking the session lock.
/// A task grant cannot substitute for the current run, cancellation or lease.
pub async fn fresh_action_authority_on<C: ConnectionTrait>(
    txn: &C,
    session: &PersistedAgentSession,
) -> Result<CurrentTaskAuthority, DbErr> {
    if !session.agent_role.is_main()
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.surface != AgentSessionSurface::AiAssistant
        || session.input_revision != 1
        || session.lease_token == 0
        || session.current_request_id.as_deref() != Some(session.conversation_id.as_str())
        || session.active_control_connection_id.is_some()
        || !matches!(
            session.turn_state,
            desk_diagnose_core::session::TurnState::Running
                | desk_diagnose_core::session::TurnState::AwaitingApproval
        )
    {
        return Err(invalid());
    }
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(&session.conversation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if work.owner_user_id.to_string() != session.actor_id
        || work.conversation_id != session.conversation_id
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
    {
        return Err(invalid());
    }
    lock_run_authority(
        txn,
        work.owner_user_id,
        &session.device_id,
        &work.run_id,
        work.lease_owner.as_deref().ok_or_else(invalid)?,
        work.lease_epoch,
    )
    .await
    .map_err(|error| match error {
        ScheduleStoreError::Backend(error) => error,
        _ => invalid(),
    })
}

fn decode<R>(row: &SessionRow<R>) -> Result<Box<PersistedAgentSession>, DbErr> {
    let session =
        Box::new(PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?);
    if matches!(
        session.trigger_origin,
        TriggerOrigin::ScheduledContinuation | TriggerOrigin::ScheduledTask
    ) && (session.conversation_id != row.conversation_id
        || session.actor_id != row.actor_id
        || session.device_id != row.device_id
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token))
    {
        return Err(invalid());
    }
    Ok(session)
}
/// Return only the immutable task input. Operation dispatch retains its
/// existing task, cancellation, lease and action checks.
pub async fn scheduled_context_on<C: ConnectionTrait>(
    txn: &C,
    session: &PersistedAgentSession,
) -> Result<
    Option<(
        Option<desk_diagnose_core::input_read_context::ReadContextSelection>,
        chrono::DateTime<chrono::Utc>,
    )>,
    DbErr,
> {
    let Some(binding) = &session.scheduled_request else {
        return Ok(None);
    };
    binding.validate(session).map_err(|_| invalid())?;
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(&binding.run_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let snapshot: entity::Model =
        serde_json::from_str(&work.task_snapshot_json).map_err(|_| invalid())?;
    let consent = super::retrieval_consent(&task)
        .map_err(|_| invalid())?
        .ok_or_else(invalid)?;
    if work.owner_user_id.to_string() != session.actor_id
        || work.conversation_id != session.conversation_id
        || task.source_conversation_id.as_deref() != Some(&session.conversation_id)
        || consent.source_session_device_id != session.device_id
        || snapshot.retrieval_consent_json != task.retrieval_consent_json
        || binding.as_ref() != &consent.request_binding(&work.run_id)
    {
        return Err(invalid());
    }
    let recorded = chrono::DateTime::from_timestamp_millis(
        binding
            .recorded_at_unix_ms
            .try_into()
            .map_err(|_| invalid())?,
    )
    .ok_or_else(invalid)?;
    Ok(Some((binding.original_read_context.clone(), recorded)))
}

/// Metadata consent never substitutes for the native task and both live
/// leases. Call before operation/quota locks, and repeat before dispatch.
pub async fn assistant_retrieval_deadline_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    expected: &PersistedAgentSession,
) -> Result<u64, DbErr> {
    use desk_diagnose_core::ai_assistant::scheduled_retrieval::{
        RetrievalOccurrence, ScheduledRetrievalKind,
    };
    if !matches!(
        expected.trigger_origin,
        TriggerOrigin::ScheduledTask | TriggerOrigin::ScheduledContinuation
    ) || !expected.agent_role.is_main()
    {
        return Err(invalid());
    }
    let data = expected.assistant_data.as_ref().ok_or_else(invalid)?;
    let run_id = data.retrieval_task_id.as_deref().ok_or_else(invalid)?;
    if expected.current_request_id.as_deref() != Some(run_id) {
        return Err(DbErr::Custom(
            "scheduled retrieval request differs from its occurrence".into(),
        ));
    }
    let row = desk_diagnose_core::future::send_phase(|| {
        lock_action_session_row(host, txn, &expected.conversation_id)
    })
    .await
    .map_err(|error| DbErr::Custom(format!("scheduled retrieval native fence: {error}")))?
    .ok_or_else(invalid)?;
    let mut current = decode(&row)?;
    // Model preparation computes this usage estimate in memory before its
    // next save. It grants no retrieval authority. All other original
    // session fields, including the spent ledger, still compare exactly.
    current.context_usage_basis = expected.context_usage_basis.clone();
    if current.as_ref() != expected {
        return Err(DbErr::Custom(
            "scheduled retrieval session differs from the original native row".into(),
        ));
    }
    // Repeat the original native SELECTs under the task/session fences;
    // transaction identity alone does not make earlier rows current.
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let snapshot: entity::Model =
        serde_json::from_str(&work.task_snapshot_json).map_err(|_| invalid())?;
    if work.finished_at.is_some()
        || work.owner_user_id != task.owner_user_id
        || work.conversation_id != expected.conversation_id
        || !same_retrieval_definition(&task, &snapshot)
    {
        return Err(DbErr::Custom(
            "scheduled retrieval task snapshot changed".into(),
        ));
    }
    let consent = super::retrieval_consent(&task)
        .map_err(|_| invalid())?
        .ok_or_else(invalid)?;
    let fresh = expected.trigger_origin == TriggerOrigin::ScheduledTask;
    let mut deadline = row.lease_deadline_ms.ok_or_else(invalid)?;
    let original_deadline = if fresh {
        let authority =
            desk_diagnose_core::future::send_phase(|| fresh_action_authority_on(txn, expected))
                .await?;
        deadline = deadline.min(authority.valid_until());
        Some(
            authority
                .delegation_source()
                .map_err(|_| invalid())?
                .deadline_ms()
                .map_err(|_| invalid())?
                .try_into()
                .map_err(|_| invalid())?,
        )
    } else {
        deadline = deadline.min(work.lease_deadline.ok_or_else(invalid)?);
        None
    };
    let occurrence = RetrievalOccurrence {
        schedule_id: &task.schedule_id,
        task_revision: task.task_revision.try_into().map_err(|_| invalid())?,
        run_id,
        kind: if fresh {
            ScheduledRetrievalKind::FreshTask
        } else {
            ScheduledRetrievalKind::ConversationResume
        },
        source_conversation_id: consent.source_conversation_id.as_deref(),
        root_conversation_id: &expected.conversation_id,
        actor_id: &expected.actor_id,
        device_id: &task.target_device_id,
        source_device_ref: &data.source_device_ref,
        source_instance_id: data.source_instance_id.as_deref(),
        started_at_unix_ms: work
            .started_at
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
        original_deadline_unix_ms: original_deadline,
    };
    consent
        .validate_occurrence_binding(expected, &occurrence)
        .map_err(|message| DbErr::Custom(message.into()))?;
    let now = authority_now(txn).await.map_err(|_| invalid())?;
    if deadline <= now {
        return Err(invalid());
    }
    deadline.try_into().map_err(|_| invalid())
}
/// Compare the frozen task definition; mutable occurrence bookkeeping is fenced separately.
pub fn same_retrieval_definition(task: &entity::Model, snapshot: &entity::Model) -> bool {
    task.schedule_id == snapshot.schedule_id
        && task.owner_user_id == snapshot.owner_user_id
        && task.target_device_id == snapshot.target_device_id
        && task.kind == snapshot.kind
        && task.task_revision == snapshot.task_revision
        && task.prompt == snapshot.prompt
        && task.model_id == snapshot.model_id
        && task.locale == snapshot.locale
        && task.spec_json == snapshot.spec_json
        && task.calc_version == snapshot.calc_version
        && task.source_conversation_id == snapshot.source_conversation_id
        && task.requirement_revision == snapshot.requirement_revision
        && task.retrieval_consent_json == snapshot.retrieval_consent_json
}

fn invalid() -> DbErr {
    DbErr::Custom("scheduled action is no longer bound to an active run".into())
}
