//! SQLite append-only event ledger for the OSS dynamic AI Assistant run.

pub(crate) mod input_context;
pub use desk_diagnose_core::input_read_context::ReadContextSelection;
pub use input_context::InputSubject;

use chrono::{DateTime, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope};
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::dynamic_run::{
    AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind, UserFollowupEvent,
};
use desk_diagnose_core::goal::{
    GoalLedgerEvent, GoalLimits, GoalModelBinding, GoalOpenRequestEvent, GoalOpenRequestState,
    GoalOpening, GoalRun,
};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set, TransactionTrait,
};

use crate::entity::{agent_goal_open_request, agent_goal_run, agent_run_event, agent_session};

const APPEND_ATTEMPTS: usize = 5;

#[derive(Debug, Clone)]
pub struct AppendUserFollowupParams {
    pub event_id: String,
    pub run_id: String,
    pub client_conversation_id: Option<String>,
    pub actor_id: String,
    pub device_id: String,
    pub surface: AgentSessionSurface,
    pub policy_revision: i64,
    pub current_scope: AgentScope,
    pub read_context: Option<ReadContextSelection>,
    pub message: ChatMessage,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct StartUserGoal {
    pub goal_id: String,
    pub previous_completed_goal_id: Option<String>,
    pub model_binding: GoalModelBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFollowupAck {
    pub event_id: String,
    pub event_seq: u64,
    pub input_seq: u64,
    pub input_revision: u64,
    pub newly_appended: bool,
    pub already_handled: bool,
}

#[derive(Clone)]
pub struct SignalAgentRunEventStore {
    db: DatabaseConnection,
}

impl SignalAgentRunEventStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Append the user message and its ledger event in one SQLite transaction.
    /// Returning an ACK proves both are durable. Updating the session version
    /// also fences an already-running owner from committing a stale model result.
    pub async fn append_user_followup(
        &self,
        params: AppendUserFollowupParams,
    ) -> Result<UserFollowupAck, AgentError> {
        self.append_user_followup_inner(params, None).await
    }

    /// The first user message, queued goal, and both ledger events become
    /// visible together. The caller performs model/export preflight first.
    pub async fn append_user_goal(
        &self,
        params: AppendUserFollowupParams,
        goal: StartUserGoal,
    ) -> Result<UserFollowupAck, AgentError> {
        self.append_user_followup_inner(params, Some(goal)).await
    }

    async fn append_user_followup_inner(
        &self,
        params: AppendUserFollowupParams,
        goal_start: Option<StartUserGoal>,
    ) -> Result<UserFollowupAck, AgentError> {
        validate_append_params(&params)?;
        for _ in 0..APPEND_ATTEMPTS {
            let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity)
                .await
                .map_err(|error| internal(format!("begin user follow-up transaction: {error}")))?;

            crate::schedule_store::validate_rehearsal_input_on(
                &txn,
                &params.actor_id,
                &params.device_id,
                &params.run_id,
                params.client_conversation_id.as_deref(),
                &params.message,
            )
            .await
            .map_err(|_| internal("rehearsal input changed or not admitted"))?;

            if let Some(existing) = agent_run_event::Entity::find()
                .filter(agent_run_event::Column::EventId.eq(&params.event_id))
                .one(&txn)
                .await
                .map_err(|error| internal(format!("load user follow-up event: {error}")))?
            {
                let session_row = agent_session::Entity::find()
                    .filter(agent_session::Column::ConversationId.eq(&params.run_id))
                    .one(&txn)
                    .await
                    .map_err(|error| {
                        internal(format!("load idempotent user follow-up run: {error}"))
                    })?
                    .ok_or_else(|| internal("user follow-up event has no run"))?;
                let session =
                    input_context::decode_session(&session_row, InputSubject::from(&params))?;
                let ack = ack_from_existing(&existing, &params, &session)?;
                if let Some(start) = &goal_start {
                    let row = agent_goal_run::Entity::find()
                        .filter(agent_goal_run::Column::GoalId.eq(&start.goal_id))
                        .filter(agent_goal_run::Column::ConversationId.eq(&params.run_id))
                        .one(&txn)
                        .await
                        .map_err(|error| internal(format!("load idempotent goal: {error}")))?
                        .ok_or_else(|| internal("goal input event has no goal"))?;
                    let goal = crate::agent_goal_store::decode(&row)
                        .map_err(|error| internal(format!("decode idempotent goal: {error}")))?;
                    if goal.source_message_id != params.message.message_id
                        || goal.opening != GoalOpening::OwnerRequest
                        || goal
                            .previous_completion
                            .as_ref()
                            .map(|previous| previous.goal_id.as_str())
                            != start.previous_completed_goal_id.as_deref()
                        || goal.model_binding != start.model_binding
                    {
                        return Err(internal("idempotent goal input changed"));
                    }
                }
                txn.commit().await.map_err(|error| {
                    internal(format!("commit idempotent user follow-up: {error}"))
                })?;
                return Ok(ack);
            }

            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&params.run_id))
                .one(&txn)
                .await
                .map_err(|error| internal(format!("load user follow-up run: {error}")))?;
            let now = parse_time(&params.created_at)?;
            let (mut session, old_version, existing_row_id) = match row {
                Some(row) => {
                    let mut session =
                        input_context::decode_session(&row, InputSubject::from(&params))?;
                    session.version = row.version;
                    session
                        .check_subject(&params.actor_id, &params.device_id)
                        .map_err(|error| internal(format!("user follow-up subject: {error:?}")))?;
                    session
                        .check_surface(params.surface)
                        .map_err(|error| internal(format!("user follow-up surface: {error:?}")))?;
                    session.adopt_client_metadata(
                        params.client_conversation_id.as_deref(),
                        params.surface,
                    );
                    (session, row.version, Some(row.id))
                }
                None => {
                    let mut session = PersistedAgentSession::new(
                        params.run_id.clone(),
                        params.actor_id.clone(),
                        params.device_id.clone(),
                        params.policy_revision,
                        params.current_scope.clone(),
                        params.created_at.clone(),
                    );
                    session.adopt_client_metadata(
                        params.client_conversation_id.as_deref(),
                        params.surface,
                    );
                    (session, 0, None)
                }
            };

            if let Some(selection) = &params.read_context {
                input_context::validate_selection(&session, selection, &params.message, now)?;
            }
            session.latest_input_seq = session
                .latest_input_seq
                .checked_add(1)
                .ok_or_else(|| internal("user input sequence exhausted"))?;
            session.input_revision = session
                .input_revision
                .checked_add(1)
                .ok_or_else(|| internal("user input revision exhausted"))?;
            // The new message and the approval fence commit atomically. Once the
            // ACK is visible, no permission request proposed against an older
            // requirement remains user-approvable.
            session
                .begin_focus_epoch(
                    session.input_revision,
                    params
                        .read_context
                        .iter()
                        .flat_map(|selection| selection.object_attachments.iter())
                        .map(|attachment| attachment.attachment_id.clone()),
                )
                .map_err(|error| internal(format!("start focus epoch: {error}")))?;
            session.last_event_seq = session
                .last_event_seq
                .checked_add(1)
                .ok_or_else(|| internal("agent run event sequence exhausted"))?;
            session.conversation.push(params.message.clone());
            session.updated_at = params.created_at.clone();

            let envelope = params
                .message
                .data_envelope
                .clone()
                .expect("validated user follow-up has a DataEnvelope");
            let event = AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: params.event_id.clone(),
                run_id: params.run_id.clone(),
                event_seq: session.last_event_seq,
                input_revision: session.input_revision,
                kind: AgentRunEventKind::UserFollowup,
                correlation_id: Some(params.message.message_id.clone()),
                source_envelope_ids: vec![envelope.envelope_id.clone()],
                result_envelope_ids: Vec::new(),
                created_at: params.created_at.clone(),
            };
            let followup = UserFollowupEvent {
                event: event.clone(),
                actor_id: params.actor_id.clone(),
                input_seq: session.latest_input_seq,
                message_id: params.message.message_id.clone(),
                message_envelope: envelope.clone(),
            };
            followup
                .validate()
                .map_err(|error| internal(format!("validate user follow-up event: {error}")))?;

            let active_goal_row = agent_goal_run::Entity::find()
                .filter(agent_goal_run::Column::ConversationId.eq(&params.run_id))
                .filter(agent_goal_run::Column::Status.is_not_in([
                    "completed",
                    "failed",
                    "cancelled",
                ]))
                .one(&txn)
                .await
                .map_err(|error| internal(format!("load active goal: {error}")))?;
            let opened_goal = if let Some(start) = &goal_start {
                if params
                    .client_conversation_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("rehearsal_"))
                    || active_goal_row.is_some()
                {
                    return Err(internal("this conversation already has an active goal"));
                }
                let opened_at = u64::try_from(now.timestamp_millis())
                    .map_err(|_| internal("invalid goal opening time"))?;
                let mut goal = GoalRun::new(
                    start.goal_id.clone(),
                    params.run_id.clone(),
                    params.actor_id.clone(),
                    params.device_id.clone(),
                    params.message.text.clone(),
                    params.message.message_id.clone(),
                    GoalOpening::OwnerRequest,
                    start.model_binding.clone(),
                    session.input_revision,
                    opened_at,
                    GoalLimits::default(),
                )
                .map_err(|error| internal(format!("invalid goal opening: {error:?}")))?;
                goal.apply_budget_policy(
                    &crate::goal_budget_policy::read(&txn)
                        .await
                        .map_err(|error| internal(format!("read goal budget policy: {error}")))?,
                )
                .map_err(|error| internal(format!("apply goal budget policy: {error:?}")))?;
                if let Some(previous_id) = &start.previous_completed_goal_id {
                    let previous = crate::agent_goal_store::load_completed_on(
                        &txn,
                        previous_id,
                        &params.run_id,
                        &params.actor_id,
                        &params.device_id,
                    )
                    .await
                    .map_err(|_| internal("previous completed goal is unavailable"))?;
                    goal = goal
                        .with_previous_completed(&previous)
                        .map_err(|_| internal("previous completed goal is invalid"))?;
                }
                session.last_event_seq = session
                    .last_event_seq
                    .checked_add(1)
                    .ok_or_else(|| internal("goal opening event sequence exhausted"))?;
                Some(goal)
            } else {
                None
            };
            let revised_goal = if goal_start.is_none() {
                if let Some(row) = active_goal_row.as_ref() {
                    let mut goal = crate::agent_goal_store::decode(row)
                        .map_err(|error| internal(format!("decode active goal: {error}")))?;
                    if goal.owner_id != params.actor_id || goal.device_id != params.device_id {
                        return Err(internal("active goal subject mismatch"));
                    }
                    let prior = (goal.state_version, goal.lease_epoch);
                    let now_ms = u64::try_from(now.timestamp_millis())
                        .map_err(|_| internal("invalid goal revision time"))?
                        .max(goal.updated_at_unix_ms);
                    goal.revise_input(session.input_revision, now_ms)
                        .map_err(|error| {
                            internal(format!("revise goal for new input: {error:?}"))
                        })?;
                    session.last_event_seq = session
                        .last_event_seq
                        .checked_add(1)
                        .ok_or_else(|| internal("goal revision event sequence exhausted"))?;
                    Some((goal, prior))
                } else {
                    None
                }
            } else {
                None
            };

            let goal_event_seq = session.last_event_seq;
            let closed_request = if let Some(pending_row) = agent_goal_open_request::Entity::find()
                .filter(agent_goal_open_request::Column::ConversationId.eq(&params.run_id))
                .filter(agent_goal_open_request::Column::Status.eq("pending"))
                .one(&txn)
                .await
                .map_err(|error| internal(format!("load pending goal request: {error}")))?
            {
                let mut pending = crate::agent_goal_open_store::decode(&pending_row)
                    .map_err(|error| internal(format!("decode pending goal request: {error}")))?;
                if pending.owner_id != params.actor_id || pending.device_id != params.device_id {
                    return Err(internal("pending goal request subject mismatch"));
                }
                let now_ms = u64::try_from(now.timestamp_millis())
                    .map_err(|_| internal("invalid goal request clock"))?
                    .max(pending.created_at_unix_ms);
                let state = if now_ms >= pending.expires_at_unix_ms {
                    GoalOpenRequestState::Expired
                } else {
                    GoalOpenRequestState::Withdrawn
                };
                let decision_event_id = GoalOpenRequestEvent::id_for(
                    &pending.request_id,
                    state,
                    AgentRunEventKind::GoalOpenDecided,
                )
                .map_err(|error| internal(format!("identify closed goal request: {error:?}")))?;
                pending
                    .close(state, decision_event_id, now_ms)
                    .map_err(|error| {
                        internal(format!("close superseded goal request: {error:?}"))
                    })?;
                if !crate::agent_goal_open_store::replace_pending_on(&txn, &pending)
                    .await
                    .map_err(|error| internal(format!("save superseded goal request: {error}")))?
                {
                    return Err(internal(
                        "pending goal request changed while accepting input",
                    ));
                }
                session.last_event_seq = session
                    .last_event_seq
                    .checked_add(1)
                    .ok_or_else(|| internal("goal request decision event sequence exhausted"))?;
                Some(
                    GoalOpenRequestEvent::new(
                        &pending,
                        AgentRunEventKind::GoalOpenDecided,
                        session.last_event_seq,
                        params.created_at.clone(),
                    )
                    .map_err(|error| {
                        internal(format!("create goal request decision event: {error:?}"))
                    })?,
                )
            } else {
                None
            };

            session.version = if existing_row_id.is_some() {
                old_version
                    .checked_add(1)
                    .ok_or_else(|| internal("input session version exhausted"))?
            } else {
                0
            };
            let state_json = session
                .encode_json_for_storage()
                .map_err(|error| internal(format!("encode user follow-up run: {error}")))?;
            if let Some(row_id) = existing_row_id {
                let new_version = session.version;
                let result = agent_session::Entity::update_many()
                    .col_expr(
                        agent_session::Column::StateJson,
                        sea_orm::sea_query::Expr::value(state_json),
                    )
                    .col_expr(
                        agent_session::Column::Version,
                        sea_orm::sea_query::Expr::value(new_version),
                    )
                    .col_expr(
                        agent_session::Column::UpdatedAt,
                        sea_orm::sea_query::Expr::value(now),
                    )
                    .filter(agent_session::Column::Id.eq(row_id))
                    .filter(agent_session::Column::Version.eq(old_version))
                    .exec(&txn)
                    .await
                    .map_err(|error| internal(format!("save user follow-up run: {error}")))?;
                if result.rows_affected != 1 {
                    txn.rollback().await.ok();
                    continue;
                }
            } else {
                let inserted = agent_session::ActiveModel {
                    conversation_id: Set(params.run_id.clone()),
                    actor_id: Set(params.actor_id.clone()),
                    device_id: Set(params.device_id.clone()),
                    state_json: Set(state_json),
                    version: Set(0),
                    lease_token: Set(0),
                    lease_deadline: Set(None),
                    created_at: Set(now),
                    updated_at: Set(now),
                    ..Default::default()
                }
                .insert(&txn)
                .await;
                if inserted.is_err() {
                    txn.rollback().await.ok();
                    continue;
                }
            }

            let payload_json =
                input_context::encode_event(&followup, params.read_context.as_ref())?;
            let event_row = agent_run_event::ActiveModel {
                event_id: Set(event.event_id.clone()),
                run_id: Set(event.run_id.clone()),
                event_seq: Set(to_i64("event_seq", event.event_seq)?),
                input_revision: Set(to_i64("input_revision", event.input_revision)?),
                kind: Set(event.kind.as_str().into()),
                correlation_id: Set(event.correlation_id.clone()),
                input_seq: Set(Some(to_i64("input_seq", followup.input_seq)?)),
                actor_id: Set(Some(params.actor_id.clone())),
                source_envelope_ids_json: Set(serde_json::to_string(&event.source_envelope_ids)
                    .map_err(|error| internal(format!("encode source envelope ids: {error}")))?),
                result_envelope_ids_json: Set("[]".into()),
                payload_json: Set(payload_json),
                payload_schema_version: Set(input_context::payload_version(
                    params.read_context.as_ref(),
                )),
                created_at: Set(now),
                ..Default::default()
            };
            if event_row.insert(&txn).await.is_err() {
                txn.rollback().await.ok();
                continue;
            }
            if let Some(goal) = &opened_goal {
                crate::agent_goal_store::insert_on(&txn, goal)
                    .await
                    .map_err(|error| internal(format!("insert opened goal: {error}")))?;
                let mut opened = GoalLedgerEvent::new(
                    goal,
                    AgentRunEventKind::GoalOpened,
                    goal_event_seq,
                    params.created_at.clone(),
                )
                .map_err(|error| internal(format!("create goal opening event: {error:?}")))?;
                opened
                    .event
                    .source_envelope_ids
                    .push(envelope.envelope_id.clone());
                opened
                    .validate_for(goal)
                    .map_err(|error| internal(format!("validate goal opening event: {error:?}")))?;
                agent_run_event::ActiveModel {
                    event_id: Set(opened.event.event_id.clone()),
                    run_id: Set(opened.event.run_id.clone()),
                    event_seq: Set(to_i64("goal_event_seq", opened.event.event_seq)?),
                    input_revision: Set(to_i64(
                        "goal_input_revision",
                        opened.event.input_revision,
                    )?),
                    kind: Set(opened.event.kind.as_str().into()),
                    correlation_id: Set(opened.event.correlation_id.clone()),
                    input_seq: Set(None),
                    actor_id: Set(Some(params.actor_id.clone())),
                    source_envelope_ids_json: Set(serde_json::to_string(
                        &opened.event.source_envelope_ids,
                    )
                    .map_err(|error| internal(format!("encode goal source envelopes: {error}")))?),
                    result_envelope_ids_json: Set("[]".into()),
                    payload_json: Set(serde_json::to_string(&opened).map_err(|error| {
                        internal(format!("encode goal opening event: {error}"))
                    })?),
                    payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
                    created_at: Set(now),
                    ..Default::default()
                }
                .insert(&txn)
                .await
                .map_err(|error| internal(format!("append goal opening event: {error}")))?;
            }
            if let Some((goal, (prior_version, prior_epoch))) = &revised_goal {
                if !crate::agent_goal_store::replace_on(
                    &txn,
                    goal,
                    *prior_version,
                    *prior_epoch,
                    None,
                    None,
                )
                .await
                .map_err(|error| internal(format!("save revised goal: {error}")))?
                {
                    txn.rollback().await.ok();
                    continue;
                }
                let mut revised = GoalLedgerEvent::new(
                    goal,
                    AgentRunEventKind::GoalInputReceived,
                    goal_event_seq,
                    params.created_at.clone(),
                )
                .map_err(|error| internal(format!("create goal revision event: {error:?}")))?;
                revised
                    .event
                    .source_envelope_ids
                    .push(envelope.envelope_id.clone());
                revised.validate_for(goal).map_err(|error| {
                    internal(format!("validate goal revision event: {error:?}"))
                })?;
                agent_run_event::ActiveModel {
                    event_id: Set(revised.event.event_id.clone()),
                    run_id: Set(revised.event.run_id.clone()),
                    event_seq: Set(to_i64("goal_event_seq", revised.event.event_seq)?),
                    input_revision: Set(to_i64(
                        "goal_input_revision",
                        revised.event.input_revision,
                    )?),
                    kind: Set(revised.event.kind.as_str().into()),
                    correlation_id: Set(revised.event.correlation_id.clone()),
                    input_seq: Set(None),
                    actor_id: Set(Some(params.actor_id.clone())),
                    source_envelope_ids_json: Set(serde_json::to_string(
                        &revised.event.source_envelope_ids,
                    )
                    .map_err(|error| internal(format!("encode goal revision source: {error}")))?),
                    result_envelope_ids_json: Set("[]".into()),
                    payload_json: Set(serde_json::to_string(&revised).map_err(|error| {
                        internal(format!("encode goal revision event: {error}"))
                    })?),
                    payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
                    created_at: Set(now),
                    ..Default::default()
                }
                .insert(&txn)
                .await
                .map_err(|error| internal(format!("append goal revision event: {error}")))?;
            }
            if let Some(decision) = &closed_request {
                agent_run_event::ActiveModel {
                    event_id: Set(decision.event.event_id.clone()),
                    run_id: Set(decision.event.run_id.clone()),
                    event_seq: Set(to_i64("goal_open_decision_seq", decision.event.event_seq)?),
                    input_revision: Set(to_i64(
                        "goal_open_input_revision",
                        decision.event.input_revision,
                    )?),
                    kind: Set(decision.event.kind.as_str().into()),
                    correlation_id: Set(decision.event.correlation_id.clone()),
                    input_seq: Set(None),
                    actor_id: Set(Some(params.actor_id.clone())),
                    source_envelope_ids_json: Set("[]".into()),
                    result_envelope_ids_json: Set("[]".into()),
                    payload_json: Set(serde_json::to_string(decision).map_err(|error| {
                        internal(format!("encode goal request decision: {error}"))
                    })?),
                    payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
                    created_at: Set(now),
                    ..Default::default()
                }
                .insert(&txn)
                .await
                .map_err(|error| internal(format!("append goal request decision: {error}")))?;
            }
            txn.commit()
                .await
                .map_err(|error| internal(format!("commit user follow-up: {error}")))?;
            return Ok(UserFollowupAck {
                event_id: event.event_id,
                event_seq: event.event_seq,
                input_seq: followup.input_seq,
                input_revision: event.input_revision,
                newly_appended: true,
                already_handled: false,
            });
        }
        Err(transport(
            "user follow-up conflicted; retry with the same event id",
        ))
    }

    pub async fn user_followups_after(
        &self,
        run_id: &str,
        input_seq: u64,
        limit: u64,
    ) -> Result<Vec<UserFollowupEvent>, AgentError> {
        let rows = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq(run_id))
            .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::UserFollowup.as_str()))
            .filter(agent_run_event::Column::InputSeq.gt(to_i64("input_seq", input_seq)?))
            .order_by_asc(agent_run_event::Column::InputSeq)
            .limit(limit.min(128))
            .all(&self.db)
            .await
            .map_err(|error| internal(format!("load user follow-up events: {error}")))?;
        rows.into_iter()
            .map(|row| {
                let (event, _) = input_context::decode_event(&row)?;
                Ok(event)
            })
            .collect()
    }
}

fn validate_append_params(params: &AppendUserFollowupParams) -> Result<(), AgentError> {
    if let Some(selection) = &params.read_context {
        selection.validate()?;
    }
    if params.surface != AgentSessionSurface::AiAssistant {
        return Err(internal("user follow-up ledger is AI Assistant only"));
    }
    if params.message.role != ChatRole::User
        || params.message.text.trim().is_empty()
        || params.message.text.len() > 16 * 1024
        || !params.message.tool_calls.is_empty()
        || params.message.tool_call_id.is_some()
    {
        return Err(internal("invalid user follow-up message"));
    }
    let envelope = params
        .message
        .data_envelope
        .as_ref()
        .ok_or_else(|| internal("user follow-up message has no DataEnvelope"))?;
    envelope
        .validate()
        .map_err(|error| internal(format!("invalid user follow-up DataEnvelope: {error}")))?;
    for (name, value) in [
        ("event_id", params.event_id.as_str()),
        ("run_id", params.run_id.as_str()),
        ("actor_id", params.actor_id.as_str()),
        ("device_id", params.device_id.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > 256 {
            return Err(internal(format!("invalid {name}")));
        }
    }
    parse_time(&params.created_at)?;
    Ok(())
}

fn ack_from_existing(
    row: &agent_run_event::Model,
    params: &AppendUserFollowupParams,
    session: &PersistedAgentSession,
) -> Result<UserFollowupAck, AgentError> {
    input_context::validate_replay(row, params, session)?;
    if row.run_id != params.run_id
        || row.actor_id.as_deref() != Some(params.actor_id.as_str())
        || row.kind != AgentRunEventKind::UserFollowup.as_str()
    {
        return Err(internal("user follow-up event id collision"));
    }
    let input_seq = to_u64(
        "input_seq",
        row.input_seq
            .ok_or_else(|| internal("persisted user follow-up has no input_seq"))?,
    )?;
    Ok(UserFollowupAck {
        event_id: row.event_id.clone(),
        event_seq: to_u64("event_seq", row.event_seq)?,
        input_seq,
        input_revision: to_u64("input_revision", row.input_revision)?,
        newly_appended: false,
        already_handled: session.handled_input_seq >= input_seq,
    })
}

fn parse_time(raw: &str) -> Result<DateTime<Utc>, AgentError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| internal("invalid user follow-up timestamp"))
}

fn to_i64(field: &str, value: u64) -> Result<i64, AgentError> {
    i64::try_from(value).map_err(|_| internal(format!("{field} exhausted")))
}

fn to_u64(field: &str, value: i64) -> Result<u64, AgentError> {
    u64::try_from(value).map_err(|_| internal(format!("persisted {field} is negative")))
}

fn internal(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

fn transport(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: true,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
        Sensitivity,
    };
    use desk_agent_protocol::{ExecutionMode, data_lineage::DestinationIdentity};
    use sea_orm::{Database, EntityTrait};
    use sha2::{Digest, Sha256};

    fn scope() -> AgentScope {
        AgentScope {
            granted: Vec::new(),
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: Some("test-read-only".into()),
        }
    }

    fn params(event_id: &str, message_id: &str, text: &str) -> AppendUserFollowupParams {
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{message_id}"),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("session-message-{message_id}"),
                sha256: digest.clone(),
                size_bytes: text.len() as u64,
                media_type: "text/plain;charset=utf-8".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "ai-assistant-user".into(),
                source_tool_name: "send-message".into(),
                source_object_id: Some(message_id.into()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest,
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: vec![DestinationIdentity::LocalArtifact {
                workspace_id: "test-workspace".into(),
            }],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        };
        let mut message = ChatMessage::text(message_id, ChatRole::User, text);
        message.data_envelope = Some(envelope);
        AppendUserFollowupParams {
            event_id: event_id.into(),
            run_id: "run-1".into(),
            client_conversation_id: Some("client-run-1".into()),
            actor_id: "actor-1".into(),
            device_id: "device-1".into(),
            surface: AgentSessionSurface::AiAssistant,
            policy_revision: 0,
            current_scope: scope(),
            read_context: None,
            message,
            created_at: "2026-08-25T00:00:00Z".into(),
        }
    }

    async fn file_db(path: &std::path::Path) -> DatabaseConnection {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = Database::connect(&url).await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn durable_ack_is_idempotent_and_survives_file_reopen() {
        let path =
            std::env::temp_dir().join(format!("lrdm-agent-run-event-{}.db", uuid::Uuid::new_v4()));
        let db = file_db(&path).await;
        let store = SignalAgentRunEventStore::new(db.clone());
        let first = store
            .append_user_followup(params("event-1", "message-1", "first"))
            .await
            .unwrap();
        let duplicate = store
            .append_user_followup(params("event-1", "message-1", "first"))
            .await
            .unwrap();
        assert_eq!(duplicate.event_seq, first.event_seq);
        assert!(first.newly_appended);
        assert!(!duplicate.newly_appended);
        assert!(!duplicate.already_handled);
        let second = store
            .append_user_followup(params("event-2", "message-2", "second"))
            .await
            .unwrap();
        assert_eq!((first.input_seq, second.input_seq), (1, 2));
        assert_eq!((first.input_revision, second.input_revision), (1, 2));
        db.close().await.unwrap();

        let reopened = file_db(&path).await;
        let store = SignalAgentRunEventStore::new(reopened.clone());
        let events = store.user_followups_after("run-1", 0, 128).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].message_id, "message-1");
        assert_eq!(events[1].input_seq, 2);
        let row = agent_session::Entity::find()
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        assert_eq!(session.latest_input_seq, 2);
        assert_eq!(session.input_revision, 2);
        assert_eq!(session.handled_input_seq, 0);
        assert_eq!(session.conversation.len(), 2);
        reopened.close().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn direct_goal_input_and_opening_are_one_idempotent_transaction() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let mut configured = crate::goal_budget_policy::read(&db).await.unwrap().limits;
        configured.model_calls = Some(4);
        crate::goal_budget_policy::update(
            &db,
            &desk_agent_protocol::ai_assistant::goal_budget::UpdateGoalBudgetPolicy {
                expected_revision: 0,
                limits: configured,
            },
        )
        .await
        .unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        let start = StartUserGoal {
            goal_id: "goal-1".into(),
            previous_completed_goal_id: None,
            model_binding: GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
        };
        let first = store
            .append_user_goal(
                params("event-goal-input", "message-goal", "Finish the report"),
                start.clone(),
            )
            .await
            .unwrap();
        let retry = store
            .append_user_goal(
                params("event-goal-input", "message-goal", "Finish the report"),
                start.clone(),
            )
            .await
            .unwrap();
        assert!(first.newly_appended);
        assert!(!retry.newly_appended);
        assert_eq!(first.input_revision, retry.input_revision);
        let goal = crate::agent_goal_store::load_for_subject(&db, "run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal.source_message_id, "message-goal");
        assert_eq!(goal.state, desk_diagnose_core::goal::GoalState::Queued);
        assert_eq!(goal.limits.model_calls, 4);
        let mut extended = configured;
        extended.deadline_ms = Some(desk_diagnose_core::goal::DEFAULT_DEADLINE_MS + 86_400_000);
        extended.model_calls = Some(1);
        crate::goal_budget_policy::update(
            &db,
            &desk_agent_protocol::ai_assistant::goal_budget::UpdateGoalBudgetPolicy {
                expected_revision: 1,
                limits: extended,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            crate::agent_goal_store::queued_candidates(&db, goal.deadline_unix_ms + 1, 8,)
                .await
                .unwrap()
                .len(),
            1
        );
        let retry_after_policy_change = store
            .append_user_goal(
                params("event-goal-input", "message-goal", "Finish the report"),
                start,
            )
            .await
            .unwrap();
        assert!(!retry_after_policy_change.newly_appended);
        let events = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("run-1"))
            .order_by_asc(agent_run_event::Column::EventSeq)
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["user_followup", "goal_opened"]
        );
        let paused = crate::agent_goal_store::apply_owner_action(
            &db,
            "run-1",
            "actor-1",
            "device-1",
            &goal.goal_id,
            goal.state_version,
            desk_diagnose_core::goal::GoalOwnerAction::Pause,
            chrono::DateTime::from_timestamp_millis(
                i64::try_from(goal.created_at_unix_ms + 1_000).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(paused.limits.model_calls, 1);
    }

    #[tokio::test]
    async fn reopened_goal_links_only_to_a_completed_goal_in_the_same_conversation() {
        use desk_diagnose_core::goal::GoalControl;
        use sea_orm::TransactionTrait;

        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        store
            .append_user_followup(params("event-1", "message-1", "Original goal"))
            .await
            .unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-25T00:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        let model_binding = GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        };
        let mut previous = GoalRun::new(
            "old-goal".into(),
            "run-1".into(),
            "actor-1".into(),
            "device-1".into(),
            "Original goal".into(),
            "message-1".into(),
            GoalOpening::OwnerRequest,
            model_binding.clone(),
            1,
            now,
            GoalLimits::default(),
        )
        .unwrap();
        previous.claim_slice(now + 1).unwrap();
        previous
            .finish_slice(
                1,
                1,
                1,
                &GoalControl::Complete {
                    summary: "Partial result".into(),
                    evidence_ids: vec!["receipt-1".into()],
                },
                true,
                true,
                2,
                vec![],
                now + 2,
            )
            .unwrap();
        let txn = db.begin().await.unwrap();
        crate::agent_goal_store::insert_on(&txn, &previous)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let latest = crate::agent_goal_store::load_latest_completed_for_subject(
            &db, "run-1", "actor-1", "device-1",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(latest.goal_id, "old-goal");

        let start = StartUserGoal {
            goal_id: "new-goal".into(),
            previous_completed_goal_id: Some("old-goal".into()),
            model_binding,
        };
        let input = params("event-2", "message-2", "Finish the original goal");
        store
            .append_user_goal(input.clone(), start.clone())
            .await
            .unwrap();
        let new_goal =
            crate::agent_goal_store::load_for_subject(&db, "run-1", "actor-1", "device-1")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            new_goal.previous_completion.as_ref().unwrap().summary,
            "Partial result"
        );
        assert_eq!(
            new_goal.previous_completion.as_ref().unwrap().evidence_ids,
            vec!["receipt-1"]
        );
        assert_eq!(new_goal.used.slices, 0);
        let old = crate::agent_goal_store::load_completed_on(
            &db.begin().await.unwrap(),
            "old-goal",
            "run-1",
            "actor-1",
            "device-1",
        )
        .await
        .unwrap();
        assert_eq!(old.used.slices, 1);
        let mut altered = start;
        altered.previous_completed_goal_id = None;
        assert!(store.append_user_goal(input, altered).await.is_err());
    }

    #[tokio::test]
    async fn owner_goal_controls_are_fenced_and_durable() {
        use desk_diagnose_core::goal::{GoalOwnerAction, GoalState};
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        store
            .append_user_goal(
                params(
                    "event-control-input",
                    "message-control",
                    "Finish the report",
                ),
                StartUserGoal {
                    goal_id: "goal-control".into(),
                    previous_completed_goal_id: None,
                    model_binding: GoalModelBinding {
                        connection_id: "gateway".into(),
                        connection_revision: 1,
                        profile_revision: 1,
                        model_id: "model".into(),
                    },
                },
            )
            .await
            .unwrap();
        let opened = crate::agent_goal_store::load_for_subject(&db, "run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .unwrap();
        let at = |seconds: u32| {
            chrono::DateTime::parse_from_rfc3339(&format!("2026-08-25T00:00:{seconds:02}Z"))
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let paused = crate::agent_goal_store::apply_owner_action(
            &db,
            "run-1",
            "actor-1",
            "device-1",
            &opened.goal_id,
            opened.state_version,
            GoalOwnerAction::Pause,
            at(1),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(paused.state, GoalState::Paused(_)));
        assert!(
            crate::agent_goal_store::apply_owner_action(
                &db,
                "run-1",
                "actor-1",
                "device-1",
                &opened.goal_id,
                opened.state_version,
                GoalOwnerAction::Cancel,
                at(2),
            )
            .await
            .unwrap()
            .is_none()
        );
        let resumed = crate::agent_goal_store::apply_owner_action(
            &db,
            "run-1",
            "actor-1",
            "device-1",
            &opened.goal_id,
            paused.state_version,
            GoalOwnerAction::Resume,
            at(3),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(resumed.state, GoalState::Queued);
        let persisted =
            crate::agent_goal_store::load_for_subject(&db, "run-1", "actor-1", "device-1")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(persisted.state_version, resumed.state_version);
        assert_eq!(persisted.limits, opened.limits);
    }

    #[tokio::test]
    async fn later_input_revises_goal_and_original_goal_input_stays_idempotent() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        let start = StartUserGoal {
            goal_id: "goal-1".into(),
            previous_completed_goal_id: None,
            model_binding: GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
        };
        let original = params("event-goal", "message-goal", "Finish the report");
        let first = store
            .append_user_goal(original.clone(), start.clone())
            .await
            .unwrap();
        let mut next = params("event-revision", "message-revision", "Use the new figures");
        next.created_at = "2026-08-25T00:01:00Z".into();
        let revised = store.append_user_followup(next).await.unwrap();
        let replay = store.append_user_goal(original, start).await.unwrap();
        assert_eq!(first.input_revision, 1);
        assert_eq!(revised.input_revision, 2);
        assert!(!replay.newly_appended);
        let goal = crate::agent_goal_store::load_for_subject(&db, "run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal.input_revision, 2);
        assert_eq!(goal.goal_revision, 1);
        assert_eq!(
            goal.state,
            desk_diagnose_core::goal::GoalState::Waiting(
                desk_diagnose_core::goal::GoalWaitReason::User,
            )
        );
        let events = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("run-1"))
            .order_by_asc(agent_run_event::Column::EventSeq)
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            vec![
                "user_followup",
                "goal_opened",
                "user_followup",
                "goal_input_received"
            ]
        );
    }

    #[tokio::test]
    async fn newer_user_input_withdraws_an_ai_goal_proposal_atomically() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        store
            .append_user_followup(params("first-event", "first-message", "Work on a report"))
            .await
            .unwrap();
        let now_ms = chrono::DateTime::parse_from_rfc3339("2026-08-25T00:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        let pending = desk_diagnose_core::goal::GoalOpenRequest::new(
            "proposal".into(),
            "run-1".into(),
            "actor-1".into(),
            "device-1".into(),
            "first-message".into(),
            1,
            "Finish the report".into(),
            GoalLimits::default(),
            GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
            now_ms,
        )
        .unwrap();
        let txn = db.begin().await.unwrap();
        crate::agent_goal_open_store::insert_on(&txn, &pending)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let mut next = params("next-event", "next-message", "Changed requirements");
        next.created_at = "2026-08-25T00:01:00Z".into();
        let accepted = store.append_user_followup(next).await.unwrap();
        let row = crate::entity::agent_goal_open_request::Entity::find()
            .filter(crate::entity::agent_goal_open_request::Column::RequestId.eq("proposal"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let stored = crate::agent_goal_open_store::decode(&row).unwrap();
        assert_eq!(stored.state, GoalOpenRequestState::Withdrawn);
        assert_ne!(
            stored.decision_event_id.as_deref(),
            Some(accepted.event_id.as_str())
        );
        let decision = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::EventId.eq(stored.decision_event_id.clone().unwrap()))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decision.kind, "goal_open_decided");
    }
}
