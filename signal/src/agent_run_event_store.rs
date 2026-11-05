//! SQLite append-only event ledger for the OSS dynamic AI Assistant run.

use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
pub(crate) mod input_context;
pub(crate) mod main_requests;
pub use desk_diagnose_core::input_read_context::ReadContextSelection;
pub use input_context::InputSubject;

use chrono::{DateTime, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope};
use desk_assistant_data_store::main_request::{
    self, FrozenOwnerInput, OwnerAdmission, OwnerAdmissionResult,
};
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::dynamic_run::{
    AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind, AssistantInputTarget,
    UserFollowupEvent,
};
use desk_diagnose_core::goal::{
    GoalLedgerEvent, GoalLimits, GoalModelBinding, GoalOpenRequestEvent, GoalOpenRequestState,
    GoalOpening, GoalRun,
};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
#[cfg(test)]
use sea_orm::QueryOrder;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QuerySelect, Set, TransactionTrait,
};

use crate::entity::{agent_goal_open_request, agent_goal_run, agent_run_event, agent_session};

const APPEND_ATTEMPTS: usize = 5;

#[derive(Debug, Clone)]
pub struct AppendUserFollowupParams {
    pub client_timezone: String,
    /// Optional first-input choice; an existing conversation keeps its frozen mode.
    pub temporary_conversation: Option<bool>,
    pub event_id: String,
    pub run_id: String,
    pub client_conversation_id: Option<String>,
    pub actor_id: String,
    pub device_id: String,
    pub target_refs: Vec<String>,
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
    pub main_request_id: Option<String>,
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
        params: &AppendUserFollowupParams,
    ) -> Result<UserFollowupAck, AgentError> {
        self.admit_user_input(params, None, OwnerAdmission::Native)
            .await?
            .native()
            .map_err(|error| error.into_agent_error())
    }

    /// The first user message, queued goal, and both ledger events become
    /// visible together. The caller performs model/export preflight first.
    pub async fn append_user_goal(
        &self,
        params: &AppendUserFollowupParams,
        goal: StartUserGoal,
    ) -> Result<UserFollowupAck, AgentError> {
        self.admit_user_input(params, Some(goal), OwnerAdmission::Native)
            .await?
            .native()
            .map_err(|error| error.into_agent_error())
    }

    pub async fn admit_user_input(
        &self,
        params: &AppendUserFollowupParams,
        goal_start: Option<StartUserGoal>,
        admission: OwnerAdmission<'_>,
    ) -> Result<OwnerAdmissionResult<UserFollowupAck>, AgentError> {
        validate_append_params(params)?;
        if goal_start.is_some() && !matches!(admission, OwnerAdmission::Native) {
            return Err(internal("Goal input requires its native admission"));
        }
        let targetless = params.device_id.is_empty();
        if targetless && (goal_start.is_some() || params.temporary_conversation.is_some()) {
            return Err(internal(
                "targetless input requires the personal input path",
            ));
        }
        for _ in 0..APPEND_ATTEMPTS {
            let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity)
                .await
                .map_err(|error| internal(format!("begin user follow-up transaction: {error}")))?;

            if crate::agent_subagent_store::deleted_on(&txn, &params.run_id)
                .await
                .map_err(|_| internal("conversation deletion state unavailable"))?
            {
                return Err(internal("conversation was deleted"));
            }
            let workbench_id = main_request::workbench_for_activation(
                &txn,
                admission,
                &params.actor_id,
                &params.run_id,
            )
            .await
            .map_err(|error| error.into_agent_error())?;
            let queue_id = workbench_id.as_deref().unwrap_or(&params.run_id);
            if let Some(workbench_id) = &workbench_id {
                if !targetless {
                    return Err(internal(
                        "workbench work must use the personal assistant scope",
                    ));
                }

                if !main_requests::workbench_ready_on(
                    &txn,
                    &params.actor_id,
                    workbench_id,
                    parse_time(&params.created_at)?.timestamp_millis(),
                )
                .await?
                {
                    return Ok(OwnerAdmissionResult::Waiting);
                }
            }
            if !targetless {
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
            }
            if let OwnerAdmission::Enqueue { ask, .. } = admission {
                check_queue_intent(params, ask)?;
                if let Some(queued) = main_request::replay(
                    &txn,
                    &params.actor_id,
                    &params.run_id,
                    ask,
                    &params.device_id,
                )
                .await
                .map_err(|error| error.into_agent_error())?
                {
                    txn.commit()
                        .await
                        .map_err(|_| internal("commit queued input replay"))?;
                    return Ok(OwnerAdmissionResult::Queued(Box::new(queued)));
                }
            }

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
                    input_context::decode_session(&session_row, InputSubject::from(params))?;
                if session.agent_role.workbench_id() != workbench_id.as_deref() {
                    return Err(internal("workbench replay differs from its source"));
                }
                if params.temporary_conversation.is_some_and(|value| {
                    session
                        .assistant_data
                        .as_ref()
                        .is_none_or(|data| data.temporary != value)
                }) {
                    return Err(internal(
                        "temporary mode differs from the accepted conversation",
                    ));
                }
                let mut ack = ack_from_existing(&existing, params, &session)?;
                if let OwnerAdmission::Activate { request_id } = admission {
                    let queued = main_request::load(&txn, &params.actor_id, queue_id, request_id)
                        .await
                        .map_err(|error| error.into_agent_error())?
                        .filter(|queued| {
                            queued.client_message_id == params.message.message_id
                                && queued.input_revision == Some(ack.input_revision as i64)
                        })
                        .ok_or_else(|| {
                            internal("native input is not linked to this queue request")
                        })?;
                    if queued.first_worker_request_id.is_some() {
                        txn.commit()
                            .await
                            .map_err(|_| internal("commit dispatched input replay"))?;
                        return Ok(OwnerAdmissionResult::Dispatched);
                    }
                    ack.main_request_id = Some(request_id.into());
                }
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
                return Ok(OwnerAdmissionResult::Native(ack));
            }

            let mut targets = if params.target_refs.is_empty() {
                Vec::new()
            } else {
                desk_diagnose_core::future::send_phase(|| {
                    resolve_targets_on(&txn, &params.target_refs)
                })
                .await?
            };
            if !targets.is_empty()
                && let Some(roster) = desk_diagnose_core::future::boxed(|| {
                    desk_assistant_data_store::device_roster::lock(&txn, &params.actor_id, queue_id)
                })
                .await
                .map_err(|error| error.into_agent_error())?
            {
                roster
                    .freeze(&mut targets)
                    .map_err(|_| internal("selected device is outside the coordinator roster"))?;
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
                        input_context::decode_session(&row, InputSubject::from(params))?;
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
                    let mut session = if let Some(workbench_id) = &workbench_id {
                        let OwnerAdmission::Activate { request_id } = admission else {
                            unreachable!()
                        };
                        PersistedAgentSession::new_workbench_coordinator(
                            &params.actor_id,
                            workbench_id,
                            Some(request_id),
                            params.policy_revision,
                            params.current_scope.clone(),
                            &params.created_at,
                        )
                        .map_err(internal)?
                    } else {
                        PersistedAgentSession::new(
                            params.run_id.clone(),
                            params.actor_id.clone(),
                            params.device_id.clone(),
                            params.policy_revision,
                            params.current_scope.clone(),
                            params.created_at.clone(),
                        )
                    };
                    session.adopt_client_metadata(
                        params.client_conversation_id.as_deref(),
                        params.surface,
                    );
                    (session, 0, None)
                }
            };

            if (workbench_id.is_some()
                && session.agent_role.workbench_id() != workbench_id.as_deref())
                || (session.agent_role.workbench_id().is_some()
                    && workbench_id.is_none()
                    && !matches!(admission, OwnerAdmission::Enqueue { .. }))
            {
                return Err(internal(
                    "workbench input requires its exact queued request",
                ));
            }
            if session.agent_role.workbench_id().is_some()
                && targets
                    .iter()
                    .any(|target| target.member_generation.is_none())
            {
                return Err(internal("workbench targets require the coordinator roster"));
            }
            if let Some(selection) = &params.read_context {
                input_context::validate_selection(&session, selection, &params.message, now)?;
            }
            if !matches!(admission, OwnerAdmission::Enqueue { .. })
                && session.has_live_main_coordination(now.timestamp_millis())
            {
                return Ok(OwnerAdmissionResult::Waiting);
            }
            let activation = match admission {
                OwnerAdmission::Native => None,
                OwnerAdmission::Enqueue {
                    ask,
                    limit,
                    reply_connection_id,
                } => {
                    let frozen = FrozenOwnerInput::capture(
                        ask,
                        &params.device_id,
                        &targets,
                        params.read_context.as_ref(),
                        &params.message,
                    )
                    .map_err(|error| error.into_agent_error())?;
                    let queued = main_request::enqueue(
                        &txn,
                        &params.actor_id,
                        &params.run_id,
                        &frozen,
                        reply_connection_id,
                        limit,
                        now.timestamp_millis(),
                    )
                    .await
                    .map_err(|error| error.into_agent_error())?;
                    txn.commit()
                        .await
                        .map_err(|_| internal("commit queued assistant input"))?;
                    return Ok(OwnerAdmissionResult::Queued(Box::new(queued)));
                }
                OwnerAdmission::Activate { request_id } => {
                    let head = main_request::first_pending(&txn, &params.actor_id, queue_id)
                        .await
                        .map_err(|error| error.into_agent_error())?;
                    let Some(head) = head.filter(|row| row.request_id == request_id) else {
                        return Ok(OwnerAdmissionResult::Waiting);
                    };
                    if desk_diagnose_core::future::boxed(|| {
                        main_requests::slot_held(&txn, &session)
                    })
                    .await?
                    {
                        return Ok(OwnerAdmissionResult::Waiting);
                    }
                    let frozen = FrozenOwnerInput::decode(&head)
                        .map_err(|error| error.into_agent_error())?;
                    check_queue_intent(params, &frozen.ask)?;
                    if frozen
                        != FrozenOwnerInput::capture(
                            &frozen.ask,
                            &params.device_id,
                            &targets,
                            params.read_context.as_ref(),
                            &params.message,
                        )
                        .map_err(|error| error.into_agent_error())?
                    {
                        return Err(internal(
                            "queued assistant input scope or destination changed",
                        ));
                    }
                    Some(head)
                }
            };
            let prior_task_retrieval =
                desk_diagnose_core::ai_assistant::data_access::task_retrieval_snapshot(&session);
            desk_diagnose_core::future::send_phase(|| {
                crate::assistant_data_store::accept_input_on(
                    &txn,
                    &mut session,
                    &params.client_timezone,
                    params.temporary_conversation,
                    now.timestamp_millis().max(0) as u64,
                )
            })
            .await
            .map_err(|_| internal("assistant data input could not be committed"))?;
            session.latest_input_seq = session
                .latest_input_seq
                .checked_add(1)
                .ok_or_else(|| internal("user input sequence exhausted"))?;
            session.input_revision = session
                .input_revision
                .checked_add(1)
                .ok_or_else(|| internal("user input revision exhausted"))?;
            if targetless {
                session.bind_personal_assistant_request_targets(&targets)
            } else {
                session.bind_native_assistant_input_target()
            }
            .map_err(internal)?;
            if targetless {
                crate::assistant_data_store::bind_input_memory_devices(&mut session, &targets)
                    .map_err(internal)?;
            }
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
                client_timezone: params.client_timezone.clone(),
                actor_id: params.actor_id.clone(),
                input_seq: session.latest_input_seq,
                message_id: params.message.message_id.clone(),
                message_envelope: envelope.clone(),
                targets,
            };
            followup
                .validate()
                .map_err(|error| internal(format!("validate user follow-up event: {error}")))?;

            let goals = desk_diagnose_core::future::send_phase(|| {
                prepare_input_goals_on(
                    &txn,
                    params,
                    goal_start.as_ref(),
                    &mut session,
                    prior_task_retrieval,
                    now,
                )
            })
            .await?;
            session.version = if existing_row_id.is_some() {
                old_version
                    .checked_add(1)
                    .ok_or_else(|| internal("input session version exhausted"))?
            } else {
                0
            };
            crate::agent_subagent_store::initialize_input_group_on(
                &txn,
                &mut session,
                params.message.clone(),
                params.read_context.clone(),
                goals
                    .opened_goal
                    .as_ref()
                    .or_else(|| goals.revised_goal.as_ref().map(|(goal, _)| goal)),
                now.timestamp_millis(),
                desk_diagnose_core::subagent::creation::AcceptedOwnerSource {
                    targets: &followup.targets,
                    main_request_id: activation.as_ref().map(|row| row.request_id.as_str()),
                },
            )
            .await
            .map_err(|error| internal(format!("initialize delegation source: {error}")))?;
            let dispatched = match activation
                .as_ref()
                .filter(|row| row.first_dispatch_json.is_some())
            {
                Some(queued) => desk_diagnose_core::future::boxed(|| {
                    crate::agent_subagent_store::first_dispatch_on(&txn, &mut session, queued, now)
                })
                .await
                .map_err(|error| internal(format!("commit first Worker dispatch: {error}")))?,
                None => None,
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
                    conversation_schema_version: Set(Some(i32::from(
                        desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                    ))),
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
            if !desk_diagnose_core::future::send_phase(|| {
                write_input_goal_events_on(&txn, params, &goals, &envelope, now)
            })
            .await?
            {
                txn.rollback().await.ok();
                continue;
            }
            if let Some(queued) = &activation
                && !main_request::activate_with_dispatch(
                    &txn,
                    queued,
                    session.input_revision,
                    now.timestamp_millis(),
                    dispatched.as_deref(),
                )
                .await
                .map_err(|error| error.into_agent_error())?
            {
                return Err(internal("queued assistant input changed before activation"));
            }
            desk_diagnose_core::future::boxed(|| {
                crate::assistant_data_store::history::capture_user_input_on(
                    &txn,
                    &session,
                    &params.message,
                    now.timestamp_millis().max(0) as u64,
                )
            })
            .await
            .map_err(|_| internal("assistant input history could not be committed"))?;
            txn.commit()
                .await
                .map_err(|error| internal(format!("commit user follow-up: {error}")))?;
            desk_diagnose_core::model_observability::permission::input_superseded(
                &session,
                now.timestamp_millis(),
                crate::model_metrics::runtime::submit,
            );
            if let Some(decision) = &goals.closed_request {
                desk_diagnose_core::model_observability::permission::goal_open_closed(
                    &decision.request,
                    now.timestamp_millis(),
                    crate::model_metrics::runtime::submit,
                );
            }
            if dispatched.is_some() {
                return Ok(OwnerAdmissionResult::Dispatched);
            }
            return Ok(OwnerAdmissionResult::Native(UserFollowupAck {
                event_id: event.event_id,
                event_seq: event.event_seq,
                input_seq: followup.input_seq,
                input_revision: event.input_revision,
                newly_appended: true,
                already_handled: false,
                main_request_id: activation.map(|queued| queued.request_id),
            }));
        }
        Err(transport(
            "user follow-up conflicted; retry with the same event id",
        ))
    }

    #[cfg(test)]
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

// Owned Goal after-image for the original input transaction. Preparing this
// state neither commits the input nor changes its approval/budget fences.
struct PreparedInputGoals {
    opened_goal: Option<GoalRun>,
    revised_goal: Option<(GoalRun, (u64, u64))>,
    closed_request: Option<GoalOpenRequestEvent>,
    goal_event_seq: u64,
}

async fn prepare_input_goals_on(
    db: &DatabaseTransaction,
    params: &AppendUserFollowupParams,
    goal_start: Option<&StartUserGoal>,
    session: &mut PersistedAgentSession,
    prior_task_retrieval: Option<
        Box<desk_diagnose_core::ai_assistant::data_access::TaskRetrievalSnapshot>,
    >,
    now: DateTime<Utc>,
) -> Result<Box<PreparedInputGoals>, AgentError> {
    let mut goals = Box::new(PreparedInputGoals {
        opened_goal: None,
        revised_goal: None,
        closed_request: None,
        goal_event_seq: 0,
    });
    let active_goal_row = agent_goal_run::Entity::find()
        .filter(agent_goal_run::Column::ConversationId.eq(&params.run_id))
        .filter(agent_goal_run::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(db)
        .await
        .map_err(|error| internal(format!("load active goal: {error}")))?;
    goals.opened_goal = if let Some(start) = &goal_start {
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
            &crate::goal_budget_policy::read(db)
                .await
                .map_err(|error| internal(format!("read goal budget policy: {error}")))?,
        )
        .map_err(|error| internal(format!("apply goal budget policy: {error:?}")))?;
        if let Some(previous_id) = &start.previous_completed_goal_id {
            let previous = crate::agent_goal_store::load_completed_on(
                db,
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
    if let Some(goal) = &goals.opened_goal {
        desk_diagnose_core::ai_assistant::data_access::bind_opened_task(
            session,
            &goal.goal_id,
            goal.deadline_unix_ms,
        )
        .map_err(internal)?;
    }
    goals.revised_goal = if goal_start.is_none() {
        if let Some(row) = active_goal_row.as_ref() {
            let mut goal = crate::agent_goal_store::decode(row)
                .map_err(|error| internal(format!("decode active goal: {error}")))?;
            if goal.owner_id != params.actor_id || goal.device_id != params.device_id {
                return Err(internal("active goal subject mismatch"));
            }
            desk_diagnose_core::ai_assistant::data_access::preserve_active_task(
                session,
                prior_task_retrieval,
                &goal.goal_id,
                goal.deadline_unix_ms,
            )
            .map_err(internal)?;
            let prior = (goal.state_version, goal.lease_epoch);
            let now_ms = u64::try_from(now.timestamp_millis())
                .map_err(|_| internal("invalid goal revision time"))?
                .max(goal.updated_at_unix_ms);
            goal.revise_input(session.input_revision, now_ms)
                .map_err(|error| internal(format!("revise goal for new input: {error:?}")))?;
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

    goals.goal_event_seq = session.last_event_seq;
    goals.closed_request = if let Some(pending_row) = agent_goal_open_request::Entity::find()
        .filter(agent_goal_open_request::Column::ConversationId.eq(&params.run_id))
        .filter(agent_goal_open_request::Column::Status.eq("pending"))
        .one(db)
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
            .map_err(|error| internal(format!("close superseded goal request: {error:?}")))?;
        if !crate::agent_goal_open_store::replace_pending_on(db, &pending)
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
            .map_err(|error| internal(format!("create goal request decision event: {error:?}")))?,
        )
    } else {
        None
    };

    Ok(goals)
}

async fn write_input_goal_events_on(
    db: &DatabaseTransaction,
    params: &AppendUserFollowupParams,
    goals: &PreparedInputGoals,
    envelope: &desk_agent_protocol::data_lineage::DataEnvelope,
    now: DateTime<Utc>,
) -> Result<bool, AgentError> {
    if let Some(goal) = &goals.opened_goal {
        crate::agent_goal_store::insert_on(db, goal)
            .await
            .map_err(|error| internal(format!("insert opened goal: {error}")))?;
        let mut opened = GoalLedgerEvent::new(
            goal,
            AgentRunEventKind::GoalOpened,
            goals.goal_event_seq,
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
            input_revision: Set(to_i64("goal_input_revision", opened.event.input_revision)?),
            kind: Set(opened.event.kind.as_str().into()),
            correlation_id: Set(opened.event.correlation_id.clone()),
            input_seq: Set(None),
            actor_id: Set(Some(params.actor_id.clone())),
            source_envelope_ids_json: Set(serde_json::to_string(&opened.event.source_envelope_ids)
                .map_err(|error| internal(format!("encode goal source envelopes: {error}")))?),
            result_envelope_ids_json: Set("[]".into()),
            payload_json: Set(serde_json::to_string(&opened)
                .map_err(|error| internal(format!("encode goal opening event: {error}")))?),
            payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(|error| internal(format!("append goal opening event: {error}")))?;
    }
    if let Some((goal, (prior_version, prior_epoch))) = &goals.revised_goal {
        if !crate::agent_goal_store::replace_on(db, goal, *prior_version, *prior_epoch, None, None)
            .await
            .map_err(|error| internal(format!("save revised goal: {error}")))?
        {
            return Ok(false);
        }
        let mut revised = GoalLedgerEvent::new(
            goal,
            AgentRunEventKind::GoalInputReceived,
            goals.goal_event_seq,
            params.created_at.clone(),
        )
        .map_err(|error| internal(format!("create goal revision event: {error:?}")))?;
        revised
            .event
            .source_envelope_ids
            .push(envelope.envelope_id.clone());
        revised
            .validate_for(goal)
            .map_err(|error| internal(format!("validate goal revision event: {error:?}")))?;
        agent_run_event::ActiveModel {
            event_id: Set(revised.event.event_id.clone()),
            run_id: Set(revised.event.run_id.clone()),
            event_seq: Set(to_i64("goal_event_seq", revised.event.event_seq)?),
            input_revision: Set(to_i64("goal_input_revision", revised.event.input_revision)?),
            kind: Set(revised.event.kind.as_str().into()),
            correlation_id: Set(revised.event.correlation_id.clone()),
            input_seq: Set(None),
            actor_id: Set(Some(params.actor_id.clone())),
            source_envelope_ids_json: Set(serde_json::to_string(
                &revised.event.source_envelope_ids,
            )
            .map_err(|error| internal(format!("encode goal revision source: {error}")))?),
            result_envelope_ids_json: Set("[]".into()),
            payload_json: Set(serde_json::to_string(&revised)
                .map_err(|error| internal(format!("encode goal revision event: {error}")))?),
            payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(|error| internal(format!("append goal revision event: {error}")))?;
    }
    if let Some(decision) = &goals.closed_request {
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
            payload_json: Set(serde_json::to_string(decision)
                .map_err(|error| internal(format!("encode goal request decision: {error}")))?),
            payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(|error| internal(format!("append goal request decision: {error}")))?;
    }
    Ok(true)
}

fn check_queue_intent(
    params: &AppendUserFollowupParams,
    ask: &desk_agent_protocol::ai_assistant::AiAssistantAsk,
) -> Result<(), AgentError> {
    if ask.conversation_id.as_deref().map(str::trim) != params.client_conversation_id.as_deref()
        || ask.client_timezone != params.client_timezone
        || ask.temporary_conversation != params.temporary_conversation
    {
        return Err(internal("queued assistant conversation intent changed"));
    }
    Ok(())
}

fn validate_append_params(params: &AppendUserFollowupParams) -> Result<(), AgentError> {
    desk_agent_protocol::ai_assistant::validate_target_refs(
        params.target_refs.iter().map(String::as_str),
    )
    .map_err(internal)?;
    if !params.device_id.is_empty() && !params.target_refs.is_empty() {
        return Err(internal(
            "device-bound input cannot select personal conversation targets",
        ));
    }
    if params.device_id.is_empty() {
        if params.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
            || params
                .read_context
                .as_ref()
                .is_some_and(|selection| !selection.is_host_web_only())
        {
            return Err(internal(
                "targetless input requires an owner without device context",
            ));
        }
        if desk_diagnose_core::native_ids::is(
            desk_diagnose_core::native_ids::Kind::WorkbenchWorkConversation,
            &params.run_id,
        ) {
            desk_diagnose_core::assistant_policy::validate_personal_input_scope(
                params.client_conversation_id.as_deref(),
                &params.current_scope,
            )
            .map_err(internal)?;
        } else {
            desk_diagnose_core::assistant_policy::validate_targetless_input(
                &params.actor_id,
                &params.run_id,
                params.client_conversation_id.as_deref(),
                &params.current_scope,
            )
            .map_err(internal)?;
        }
        if let Some(selection) = &params.read_context {
            let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry_ref();
            if selection.expires_at != params.current_scope.expires_at
                || selection.tool_names.iter().any(|name| {
                    registry.capability_for_tool(name).is_none_or(|capability| {
                        !params
                            .current_scope
                            .granted
                            .contains(&capability.required_capability)
                    })
                })
            {
                return Err(internal(
                    "targetless read context exceeds its original scope",
                ));
            }
        }
    }
    if let Some(selection) = &params.read_context {
        selection.validate()?;
    }
    if params.surface != AgentSessionSurface::AiAssistant {
        return Err(internal("user follow-up ledger is AI Assistant only"));
    }
    if params.message.role != ChatRole::User
        || params.message.text.trim().is_empty()
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
    ]
    .into_iter()
    .chain((!params.device_id.is_empty()).then_some(("device_id", params.device_id.as_str())))
    {
        if value.trim().is_empty() || value.len() > 256 {
            return Err(internal(format!("invalid {name}")));
        }
    }
    parse_time(&params.created_at)?;
    Ok(())
}

pub(crate) async fn resolve_targets_on(
    txn: &DatabaseTransaction,
    references: &[String],
) -> Result<Vec<AssistantInputTarget>, AgentError> {
    let ids = eligible_request_device_ids(txn, references).await?;
    if ids.len() != references.len() {
        return Err(internal("selected assistant device is unavailable"));
    }
    Ok(references
        .iter()
        .map(|reference| AssistantInputTarget {
            reference: reference.clone(),
            device_id: reference.clone(),
            member_generation: None,
        })
        .collect())
}

pub(crate) async fn eligible_request_device_ids<C: sea_orm::ConnectionTrait>(
    db: &C,
    references: &[String],
) -> Result<std::collections::HashSet<String>, AgentError> {
    use crate::entity::{device_code, host_remote_access_state};
    use sea_orm::QueryTrait;
    let locked = host_remote_access_state::Entity::find()
        .select_only()
        .column(host_remote_access_state::Column::ClientId)
        .filter(host_remote_access_state::Column::Locked.eq(true));
    Ok(device_code::Entity::find()
        .select_only()
        .column(device_code::Column::ClientId)
        .filter(device_code::Column::ClientId.is_in(references.iter().cloned()))
        .filter(device_code::Column::ClientId.not_in_subquery(locked.into_query()))
        .into_tuple::<String>()
        .all(db)
        .await
        .map_err(|_| internal("assistant device directory unavailable"))?
        .into_iter()
        .collect())
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
        main_request_id: None,
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
    use sea_orm::EntityTrait;
    use sha2::{Digest, Sha256};

    fn owner_actor() -> String {
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
    }

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
            allowed_destinations: vec![DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            }],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        };
        let mut message = ChatMessage::text(message_id, ChatRole::User, text);
        message.data_envelope = Some(envelope);
        AppendUserFollowupParams {
            target_refs: Vec::new(),
            client_timezone: "UTC".into(),
            temporary_conversation: None,
            event_id: event_id.into(),
            run_id: "run-1".into(),
            client_conversation_id: Some("client-run-1".into()),
            actor_id: owner_actor(),
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
        let db = crate::config::test_support::Database::connect(&url)
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn selected_command_decision_keeps_the_personal_session_and_rechecks_the_device() {
        verify_selected_command_device(false).await;
    }

    #[tokio::test]
    async fn device_roster_removal_fences_an_approved_command_resume() {
        verify_selected_command_device(true).await;
    }

    async fn verify_selected_command_device(managed: bool) {
        use crate::agent_session_store::{SignalAgentSessionStore, permission_resume};
        use crate::controller::ai_assistant_session::{AiAssistantDeviceRosterBody, update_roster};
        use desk_diagnose_core::{
            ai_assistant::ai_assistant_provider_registry,
            capability_availability::CapabilityAvailability,
            chat::ToolCall,
            command_confirmation::test_policy,
            conversation_key::{AssistantConversationScope, derive_assistant_conversation_key},
            dynamic_run::{
                PermissionDecisionItem, PermissionItemDecision, PermissionRequestState,
                PermissionRequestedEvent,
            },
            permission_grant::PermissionGrantIssuanceContext,
            permission_tools::{REQUEST_CAPABILITY_GRANTS_TOOL_NAME, build_permission_request},
            seam::{ClaimTurnParams, SessionSeam},
            session::{TriggerOrigin, TurnState},
        };

        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        crate::device_code_store::get_or_create(&db, "device-1")
            .await
            .unwrap();
        let now = Utc::now();
        let mut input = params("command-input", "command-user", "Check disk space");
        input.device_id.clear();
        input.target_refs = vec!["device-1".into()];
        input.policy_revision =
            desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
        input.created_at = now.to_rfc3339();
        input.run_id = derive_assistant_conversation_key(
            &input.actor_id,
            AssistantConversationScope::Personal,
            input.client_conversation_id.as_deref().unwrap(),
        );
        let roster = |expected_revision, target_refs| AiAssistantDeviceRosterBody {
            conversation: input.client_conversation_id.clone().unwrap(),
            expected_revision,
            target_refs,
        };
        if managed {
            update_roster(&db, &roster(0, input.target_refs.clone()))
                .await
                .unwrap();
        }
        let admitted = SignalAgentRunEventStore::new(db.clone())
            .append_user_followup(&input)
            .await
            .unwrap();
        let sessions = SignalAgentSessionStore::new(db.clone())
            .with_client_metadata(
                input.client_conversation_id.clone(),
                AgentSessionSurface::AiAssistant,
            )
            .with_expected_input_revision(admitted.input_revision);
        let mut claim = ClaimTurnParams {
            conversation_id: input.run_id.clone(),
            actor_id: input.actor_id.clone(),
            device_id: String::new(),
            policy_revision: input.policy_revision,
            current_pdp_scope: input.current_scope.clone(),
            turn_id: "selected-command-turn".into(),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::User,
            now: input.created_at.clone(),
        };
        let mut session = sessions.claim_turn(claim.clone()).await.unwrap();
        let mut policy = test_policy();
        policy.actor_id = input.actor_id.clone();
        let registry = ai_assistant_provider_registry().with_command_policy(policy.clone());
        let request = build_permission_request(
            &ToolCall {
                id: "propose".into(),
                name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
                arguments_json:
                    serde_json::json!({"items":[{"item_id":"command","tool_name":"exec_command",
                    "exact_input":{"schema_version":1,"shell":"bash","command":"df -h","timeout_ms":1000},
                    "reason":"Check available disk space"}]})
                    .to_string(),
            },
            &registry,
            "selected-command".into(),
            admitted.input_revision,
            input.created_at.clone(),
        )
        .unwrap();
        session.add_permission_request(request.clone()).unwrap();
        session.last_event_seq += 1;
        let event = PermissionRequestedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "selected-command-event".into(),
                run_id: input.run_id.clone(),
                event_seq: session.last_event_seq,
                input_revision: admitted.input_revision,
                kind: AgentRunEventKind::PermissionRequested,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: vec![],
                result_envelope_ids: vec![],
                created_at: input.created_at.clone(),
            },
            request,
        };
        sessions
            .save_permission_request(&mut session, &event)
            .await
            .unwrap();
        session.finish_turn(TurnState::Idle, input.created_at.clone());
        sessions.save(&mut session).await.unwrap();

        let item = &event.request.items[0];
        let capability = registry.capability_for_tool(&item.tool_name).unwrap();
        let inventory = [CapabilityAvailability {
            provider_id: item.provider_id.clone(),
            capability_id: capability.wire.capability_id.clone(),
            tool_name: item.tool_name.clone(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        let decide =
            |registry| {
                sessions.decide_permission_request(
            &input.run_id, &input.actor_id, "", &event.request.request_id,
            vec![PermissionDecisionItem {
                item_id: item.item_id.clone(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: item.resource_scope.clone(),
                    operation_scope: item.operation_scope.clone(),
                    export_destinations: vec![],
                    ttl_seconds: 60,
                    max_uses: 1,
                },
            }],
            PermissionGrantIssuanceContext {
                surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
                registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms: now.timestamp_millis() as u64,
                implicit_fresh_object_refs: &[],
            },
            &input.created_at,
        )
            };
        policy.target_device_id = "other".into();
        let wrong = registry.clone().with_command_policy(policy);
        assert!(decide(&wrong).await.is_err());
        assert_eq!(
            decide(&registry).await.unwrap().state,
            PermissionRequestState::Approved
        );
        let candidates = sessions.permission_resume_candidates(0, 1).await.unwrap();
        let pending = sessions
            .pending_permission_resume(&candidates[0], now)
            .await
            .unwrap()
            .unwrap();
        assert!(pending.device_id.is_empty());
        let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
            .list_for_subject(&input.run_id, &input.actor_id, "device-1")
            .await
            .unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].run_id, input.run_id);
        assert_eq!(grants[0].target_device_id, "device-1");

        claim.turn_id = permission_resume::turn_id(&input.run_id, &event.request.request_id);
        claim.trigger_origin = TriggerOrigin::PermissionDecision;
        let unbound = sessions.clone().with_permission_resume(
            event.request.request_id.clone(),
            pending.version,
            Vec::new(),
        );
        assert!(unbound.claim_turn(claim.clone()).await.is_err());
        let resume = sessions.with_permission_resume(
            event.request.request_id.clone(),
            pending.version,
            grants,
        );
        if managed {
            update_roster(&db, &roster(1, vec![])).await.unwrap();
            update_roster(&db, &roster(2, input.target_refs.clone()))
                .await
                .unwrap();
            assert!(matches!(
                resume.claim_turn(claim).await,
                Err(desk_diagnose_core::seam::ClaimError::Busy)
            ));
            assert!(
                resume
                    .permission_resume_candidates(0, 1)
                    .await
                    .unwrap()
                    .is_empty()
            );
            return;
        }
        use crate::entity::device_code;
        device_code::Entity::delete_many()
            .filter(device_code::Column::ClientId.eq("device-1"))
            .exec(&db)
            .await
            .unwrap();
        assert!(resume.claim_turn(claim).await.is_err());
    }

    #[tokio::test]
    async fn personal_input_freezes_selected_devices_and_replays_without_reresolving() {
        verify_personal_device_selection(false).await;
    }

    #[tokio::test]
    async fn coordinator_device_roster_removal_is_not_reversed_by_rejoining() {
        verify_personal_device_selection(true).await;
    }

    async fn verify_personal_device_selection(managed: bool) {
        use crate::controller::ai_assistant_session::{AiAssistantDeviceRosterBody, update_roster};
        use desk_diagnose_core::conversation_key::{
            AssistantConversationScope, derive_assistant_conversation_key,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("roster.sqlite");
        let db = if managed {
            file_db(&path).await
        } else {
            let db = crate::config::test_support::Database::connect("sqlite::memory:")
                .await
                .unwrap();
            crate::db::initialize_schema(&db).await.unwrap();
            db
        };
        for reference in ["mac", "linux"] {
            crate::device_code_store::get_or_create(&db, reference)
                .await
                .unwrap();
        }
        let store = SignalAgentRunEventStore::new(db.clone());
        let mut first = params("first-event", "first-input", "Compare these two devices");
        first.device_id.clear();
        first.policy_revision =
            desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
        first.run_id = derive_assistant_conversation_key(
            &first.actor_id,
            AssistantConversationScope::Personal,
            first.client_conversation_id.as_deref().unwrap(),
        );
        first.target_refs = vec!["mac".into(), "linux".into()];
        first.created_at = chrono::Utc::now().to_rfc3339();
        let roster = |expected_revision, target_refs| AiAssistantDeviceRosterBody {
            conversation: first.client_conversation_id.clone().unwrap(),
            expected_revision,
            target_refs,
        };
        if managed {
            let result = update_roster(&db, &roster(0, first.target_refs.clone()))
                .await
                .unwrap();
            assert_eq!(result.session_id, first.run_id);
            assert_eq!(result.revision, 1);
        }
        let receipt = store.append_user_followup(&first).await.unwrap();
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&first.run_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let current = input_context::decode_session(&row, InputSubject::from(&first)).unwrap();
        assert!(current.has_admitted_personal_input());
        assert!(current.admits_assistant_device("mac"));
        assert!(current.admits_assistant_device("linux"));

        if managed {
            use desk_assistant_data_store::device_roster;

            use desk_diagnose_core::{
                chat::{ChatMessage, ToolCall, ToolCallRef},
                seam::{ClaimTurnParams, SessionSeam},
                session::{TriggerOrigin, TurnState},
                subagent::tools,
            };
            let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
                .with_client_metadata(
                    first.client_conversation_id.clone(),
                    AgentSessionSurface::AiAssistant,
                )
                .with_expected_input_revision(receipt.input_revision);
            let turn = ClaimTurnParams {
                conversation_id: first.run_id.clone(),
                actor_id: first.actor_id.clone(),
                device_id: String::new(),
                turn_id: "personal-parent-turn".into(),
                request_id: None,
                connection_id: None,
                policy_revision: first.policy_revision,
                current_pdp_scope: first.current_scope.clone(),
                now: first.created_at.clone(),
                trigger_origin: TriggerOrigin::User,
            };
            let mut parent = sessions.claim_turn(turn.clone()).await.unwrap();
            let call = ToolCall {
                id: "personal-worker-call".into(), name: tools::SPAWN.into(),
                arguments_json: serde_json::json!({
                    "name":"Issue investigation","task":"Compare macOS and Linux",
                    "target_refs":["mac", "linux"],
                    "acceptance_criteria":["Report per-device observations"],"required_for_completion":true
                }).to_string(),
            };
            let mut caller = ChatMessage::assistant_tool_calls(
                "personal-worker-caller",
                "Delegate the comparison",
                vec![ToolCallRef {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments_json: call.arguments_json.clone(),
                }],
            )
            .with_turn_id(turn.turn_id.clone());
            caller.data_envelope = Some(
                desk_diagnose_core::subagent::projection::envelope(
                    &caller.message_id,
                    &caller.text,
                    "model-output",
                    std::slice::from_ref(first.message.data_envelope.as_ref().unwrap()),
                )
                .unwrap(),
            );
            parent.conversation.push(caller);
            sessions.save(&mut parent).await.unwrap();
            let workers = crate::agent_subagent_store::SubAgentStore::new(db.clone());
            let operation = tools::parse(&parent, &call).unwrap();
            let worker_receipt = workers
                .execute_main_tool(&mut parent, &call, operation, "personal-worker-receipt")
                .await
                .unwrap();
            let task_id = worker_receipt.payload["task_id"].as_str().unwrap();
            parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
            sessions.save(&mut parent).await.unwrap();
            let candidate = workers
                .child_runtime_candidate(task_id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                workers
                    .defer_child_candidate(
                        &candidate,
                        Some(desk_diagnose_core::subagent::SubAgentWaitReason::ModelCapacity),
                        None
                    )
                    .await
                    .unwrap()
            );
            use crate::entity::agent_subagent_run as worker_run;
            worker_run::Entity::update_many()
                .filter(worker_run::Column::TaskId.eq(task_id))
                .col_expr(
                    worker_run::Column::NextAttemptAtMs,
                    sea_orm::sea_query::Expr::value(0_i64),
                )
                .exec(&db)
                .await
                .unwrap();
            assert!(
                workers
                    .retry_resource_task(
                        task_id,
                        None,
                        desk_diagnose_core::subagent::device_wait::DeviceWaitStatus::Waiting
                    )
                    .await
                    .unwrap()
            );
            let candidate = workers
                .child_runtime_candidate(task_id)
                .await
                .unwrap()
                .unwrap();
            let desk_diagnose_core::subagent::runtime::RuntimeTurn::Child {
                run,
                creation,
                session,
            } = candidate
            else {
                panic!("expected a Worker request");
            };
            let claim = ClaimTurnParams {
                conversation_id: session.conversation_id,
                turn_id: "personal-worker-turn".into(),
                trigger_origin: TriggerOrigin::DelegatedTask,
                now: chrono::Utc::now().to_rfc3339(),
                ..turn
            };
            let crate::agent_subagent_store::SubAgentClaimOutcome::Claimed(claimed) = workers
                .claim_child(
                    &claim,
                    task_id,
                    run.fence(),
                    &creation.source.model_destination,
                )
                .await
                .unwrap()
            else {
                panic!("personal Worker should claim without a bound device");
            };
            let mut worker = claimed.session;
            assert_eq!(worker.assistant_request_devices().unwrap().len(), 2);
            use desk_diagnose_core::subagent::device_wait::DeviceWaitStatus;
            let child_sessions =
                crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
                    .with_client_metadata(None, AgentSessionSurface::AiAssistant);
            let wait_for_mac = async |worker: &mut PersistedAgentSession, receipt_id: &str| {
                let call = ToolCall {
                    id: format!("call-{receipt_id}"),
                    name: tools::WAIT_DEVICE.into(),
                    arguments_json: serde_json::json!({"target_ref":"mac"}).to_string(),
                };
                let mut caller = ChatMessage::assistant_tool_calls(
                    format!("proposal-{receipt_id}"),
                    "Wait for the Mac",
                    vec![ToolCallRef {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments_json: call.arguments_json.clone(),
                    }],
                )
                .with_turn_id(worker.current_turn_id.clone().unwrap());
                caller.data_envelope = Some(
                    desk_diagnose_core::subagent::projection::envelope(
                        &caller.message_id,
                        &caller.text,
                        "model-output",
                        std::slice::from_ref(
                            worker
                                .authorization_requirement()
                                .unwrap()
                                .data_envelope
                                .as_ref()
                                .unwrap(),
                        ),
                    )
                    .unwrap(),
                );
                worker.conversation.push(caller);
                child_sessions.save(worker).await.unwrap();
                let receipt = workers
                    .wait_for_device_for_turn(worker, &call, "mac".into(), receipt_id)
                    .await
                    .unwrap();
                assert_eq!(receipt.payload["status"], "waiting");
                assert_eq!(worker.turn_state, TurnState::Idle);
            };
            wait_for_mac(&mut worker, "wait-for-mac").await;
            assert!(!workers.reconcile_task(task_id).await.unwrap());
            assert!(
                !workers
                    .retry_resource_task(task_id, Some("linux"), DeviceWaitStatus::Ready)
                    .await
                    .unwrap()
            );
            assert!(
                !workers
                    .retry_resource_task(task_id, Some("mac"), DeviceWaitStatus::Waiting)
                    .await
                    .unwrap()
            );
            assert!(
                workers
                    .retry_resource_task(task_id, Some("mac"), DeviceWaitStatus::Ready)
                    .await
                    .unwrap()
            );
            let runtime = workers
                .child_runtime_candidate(task_id)
                .await
                .unwrap()
                .unwrap();
            let resumed = runtime.session();
            assert_eq!(resumed.input_revision, worker.input_revision);
            assert_eq!(resumed.control_revision, worker.control_revision);
            assert_eq!(resumed.chain_id, worker.chain_id);
            assert_eq!(resumed.automation_turns_used, worker.automation_turns_used);
            let result = resumed
                .conversation
                .iter()
                .find(|message| message.message_id == "wait-for-mac")
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&result.text).unwrap()["status"],
                "ready"
            );
            let next_claim = ClaimTurnParams {
                turn_id: "personal-worker-after-wait".into(),
                now: chrono::Utc::now().to_rfc3339(),
                ..claim
            };
            let crate::agent_subagent_store::SubAgentClaimOutcome::Claimed(claimed) = workers
                .claim_child(
                    &next_claim,
                    task_id,
                    run.fence(),
                    &creation.source.model_destination,
                )
                .await
                .unwrap()
            else {
                panic!("resumed Worker should claim its original request");
            };
            worker = claimed.session;
            wait_for_mac(&mut worker, "wait-for-removed-mac").await;

            assert_eq!(current.assistant_request_device_generation("mac"), Some(1));
            let removed = update_roster(&db, &roster(1, vec!["linux".into()]))
                .await
                .unwrap();
            assert_eq!(removed.revision, 2);
            let replay = store.append_user_followup(&first).await.unwrap();
            assert!(!replay.newly_appended);
            assert_eq!(replay.input_revision, receipt.input_revision);
            let mut next = params(
                "managed-next-event",
                "managed-next-input",
                "Use both devices again",
            );
            next.device_id.clear();
            next.policy_revision = first.policy_revision;
            next.run_id.clone_from(&first.run_id);
            next.target_refs.clone_from(&first.target_refs);
            assert!(store.append_user_followup(&next).await.is_err());
            let restored = update_roster(&db, &roster(2, first.target_refs.clone()))
                .await
                .unwrap();
            assert_eq!(restored.revision, 3);
            drop(child_sessions);
            drop(workers);
            drop(sessions);
            drop(store);
            drop(db);
            let db = file_db(&path).await;
            let store = SignalAgentRunEventStore::new(db.clone());
            let workers_after_restart = crate::agent_subagent_store::SubAgentStore::new(db.clone());
            assert!(
                workers_after_restart
                    .retry_resource_task(task_id, Some("mac"), DeviceWaitStatus::Waiting)
                    .await
                    .unwrap()
            );
            let resumed = workers_after_restart
                .child_runtime_candidate(task_id)
                .await
                .unwrap()
                .unwrap();
            let result = resumed
                .session()
                .conversation
                .iter()
                .find(|message| message.message_id == "wait-for-removed-mac")
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&result.text).unwrap()["status"],
                "unavailable"
            );
            assert_eq!(
                device_roster::current_request_devices(&db, &current)
                    .await
                    .unwrap(),
                vec!["linux"]
            );
            assert!(
                !device_roster::permits_current_target(&db, &current, "mac")
                    .await
                    .unwrap()
            );
            assert!(
                device_roster::permits_current_target(&db, &current, "linux")
                    .await
                    .unwrap()
            );
            assert_eq!(
                device_roster::current_request_devices(&db, &worker)
                    .await
                    .unwrap(),
                vec!["linux"]
            );
            assert!(
                !device_roster::permits_current_target(&db, &worker, "mac")
                    .await
                    .unwrap()
            );
            assert!(
                device_roster::permits_current_target(&db, &worker, "linux")
                    .await
                    .unwrap()
            );
            let next_receipt = store.append_user_followup(&next).await.unwrap();
            assert_eq!(next_receipt.input_revision, receipt.input_revision + 1);
            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&next.run_id))
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            let fresh = input_context::decode_session(&row, InputSubject::from(&next)).unwrap();
            assert_eq!(fresh.assistant_request_device_generation("mac"), Some(3));
            assert_eq!(fresh.assistant_request_device_generation("linux"), Some(1));
            return;
        }

        use crate::entity::device_code;
        device_code::Entity::delete_many()
            .filter(device_code::Column::ClientId.eq("mac"))
            .exec(&db)
            .await
            .unwrap();
        let mut next = params("second-event", "second-input", "Check only Linux");
        next.device_id.clear();
        next.policy_revision = first.policy_revision;
        next.run_id.clone_from(&first.run_id);
        next.target_refs.clone_from(&first.target_refs);
        assert!(store.append_user_followup(&next).await.is_err());
        next.target_refs.remove(0);
        let next_receipt = store.append_user_followup(&next).await.unwrap();
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&next.run_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let current = input_context::decode_session(&row, InputSubject::from(&next)).unwrap();
        assert_eq!(next_receipt.input_revision, receipt.input_revision + 1);
        assert!(!current.admits_assistant_device("mac"));
        assert!(current.admits_assistant_device("linux"));
        assert!(
            !store
                .append_user_followup(&first)
                .await
                .unwrap()
                .newly_appended
        );
        first.target_refs.remove(0);
        assert!(store.append_user_followup(&first).await.is_err());
    }

    #[tokio::test]
    async fn targetless_input_requires_its_personal_key_and_claim_revision() {
        use desk_diagnose_core::{
            conversation_key::{AssistantConversationScope, derive_assistant_conversation_key},
            seam::{ClaimTurnParams, SessionSeam},
            session::TriggerOrigin,
        };

        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        let mut input = params("targetless-event", "targetless-input", "No device required");
        input.device_id.clear();
        input.policy_revision =
            desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
        assert!(store.append_user_followup(&input).await.is_err());
        input.run_id = derive_assistant_conversation_key(
            &input.actor_id,
            AssistantConversationScope::Personal,
            input.client_conversation_id.as_deref().unwrap(),
        );
        input.read_context = Some(ReadContextSelection {
            tool_names: vec![desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME.into()],
            expires_at: None,
            object_attachments: vec![],
            live_targets: vec![],
        });
        assert!(store.append_user_followup(&input).await.is_err());
        input.current_scope.granted = vec![desk_agent_protocol::Capability::WebResearchFetch];
        let receipt = store.append_user_followup(&input).await.unwrap();
        assert!(receipt.newly_appended);
        assert!(
            !store
                .append_user_followup(&input)
                .await
                .unwrap()
                .newly_appended
        );

        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
            .with_client_metadata(
                input.client_conversation_id.clone(),
                AgentSessionSurface::AiAssistant,
            );
        let mut claim = ClaimTurnParams {
            conversation_id: input.run_id.clone(),
            actor_id: input.actor_id.clone(),
            device_id: String::new(),
            policy_revision: input.policy_revision,
            current_pdp_scope: input.current_scope.clone(),
            turn_id: "targetless-turn".into(),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::User,
            now: input.created_at.clone(),
        };
        assert!(sessions.claim_turn(claim.clone()).await.is_err());
        let sessions = sessions.with_expected_input_revision(receipt.input_revision);
        claim.current_pdp_scope.mode = ExecutionMode::ConfirmEachAction;
        assert!(sessions.claim_turn(claim.clone()).await.is_err());
        claim.current_pdp_scope = input.current_scope.clone();
        let mut session = sessions.claim_turn(claim).await.unwrap();
        assert!(session.turn_state.is_active());
        assert!(session.has_admitted_targetless_input());
        assert!(!session.admits_assistant_device("device-1"));

        let snapshot = sessions
            .read_assistant_snapshot_for_subject(&input.run_id, &input.actor_id, "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.session.input_revision, receipt.input_revision);
        assert!(snapshot.capability_grants.is_empty());
        assert!(
            sessions
                .read_assistant_snapshot_for_subject(&input.run_id, "another-actor", "")
                .await
                .unwrap()
                .is_none()
        );
        let listed = sessions
            .list_ai_assistant_sessions(&input.actor_id, "", 10)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, input.run_id);
        assert!(
            sessions
                .list_ai_assistant_sessions("another-actor", "", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            sessions
                .list_ai_assistant_sessions(&input.actor_id, "device-1", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            sessions
                .delete_for_subject(&input.run_id, "another-actor", "")
                .await
                .is_err()
        );
        sessions
            .delete_for_subject(&input.run_id, &input.actor_id, "")
            .await
            .unwrap();
        assert_eq!(
            sessions
                .delete_for_subject(&input.run_id, &input.actor_id, "")
                .await
                .unwrap(),
            None
        );
        assert!(
            sessions
                .list_ai_assistant_sessions(&input.actor_id, "", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(sessions.save(&mut session).await.is_err());
        assert!(store.append_user_followup(&input).await.is_err());
        let marker = crate::entity::agent_file_recovery_cleanup::Entity::find_by_id(&input.run_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(marker.history_deleted_at_unix_ms.is_some());
        assert!(marker.completed_at_unix_ms.is_some());
    }

    #[actix_web::test]
    async fn targetless_web_owner_decision_issues_only_the_frozen_web_grant() {
        check_targetless_web_permission(false).await;
    }

    #[actix_web::test]
    async fn targetless_stop_fences_web_permission_and_allows_new_input() {
        check_targetless_web_permission(true).await;
    }

    async fn check_targetless_web_permission(stop_before_resume: bool) {
        use crate::remote_tool_edge::{SignalAiAssistantTools, SignalRemoteToolPendingStore};
        use desk_agent_protocol::ai_assistant::subagent::AiAssistantStopControl;
        use desk_agent_protocol::capability_provider::ProductSurface;
        use desk_diagnose_core::{
            capability_availability::CapabilityAvailability,
            chat::ToolCall,
            conversation_key::{AssistantConversationScope, derive_assistant_conversation_key},
            dynamic_run::{
                PermissionDecisionItem, PermissionItemDecision, PermissionRequestState,
                PermissionRequestedEvent,
            },
            permission_grant::PermissionGrantIssuanceContext,
            permission_tools::{REQUEST_CAPABILITY_GRANTS_TOOL_NAME, build_permission_request},
            seam::{ClaimTurnParams, SessionSeam, ToolSeam},
            session::{TriggerOrigin, TurnState},
        };
        use desk_signal_facade::model::connection::SharedConnectionMap;
        use std::sync::Arc;

        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        let now = Utc::now();
        let mut input = params("web-input-event", "web-input", "Read public documentation");
        input.device_id.clear();
        input.policy_revision =
            desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
        input.run_id = derive_assistant_conversation_key(
            &input.actor_id,
            AssistantConversationScope::Personal,
            input.client_conversation_id.as_deref().unwrap(),
        );
        input.created_at = now.to_rfc3339();
        let tool = desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME;
        input.current_scope.granted = vec![desk_agent_protocol::Capability::WebResearchFetch];
        input.read_context = Some(ReadContextSelection {
            tool_names: vec![tool.into()],
            expires_at: None,
            object_attachments: vec![],
            live_targets: vec![],
        });
        let receipt = store.append_user_followup(&input).await.unwrap();
        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
            .with_client_metadata(
                input.client_conversation_id.clone(),
                AgentSessionSurface::AiAssistant,
            )
            .with_expected_input_revision(receipt.input_revision);
        let mut session = sessions
            .claim_turn(ClaimTurnParams {
                conversation_id: input.run_id.clone(),
                actor_id: input.actor_id.clone(),
                device_id: String::new(),
                policy_revision: input.policy_revision,
                current_pdp_scope: input.current_scope.clone(),
                turn_id: "targetless-web-turn".into(),
                request_id: None,
                connection_id: None,
                trigger_origin: TriggerOrigin::User,
                now: now.to_rfc3339(),
            })
            .await
            .unwrap();
        let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry_ref();
        let tools = SignalAiAssistantTools::new(
            db.clone(),
            registry.clone(),
            Arc::new(SharedConnectionMap::default()),
            Arc::new(SignalRemoteToolPendingStore::default()),
            String::new(),
            String::new(),
            input.actor_id.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            vec![],
            vec![],
            vec![],
            None,
            None,
            input.message.text.clone(),
            input.run_id.clone(),
            session.current_turn_id.clone().unwrap(),
            input.policy_revision,
            1,
            vec![],
            30_000,
        );
        let destination = input
            .message
            .data_envelope
            .as_ref()
            .unwrap()
            .allowed_destinations[0]
            .clone();
        tools
            .bind_original_input(
                receipt.input_revision,
                input.read_context.clone().unwrap(),
                destination,
                input.client_conversation_id.clone(),
            )
            .unwrap();
        let fetch = ToolCall {
            id: "fetch".into(),
            name: tool.into(),
            arguments_json: r#"{"url":"https://example.com/docs"}"#.into(),
        };
        let denied = tools.run_read(&fetch).await.unwrap_err();
        assert!(
            denied.message.contains("no active capability grant"),
            "{denied:?}"
        );
        let forged = ToolCall {
            id: "edge".into(),
            name: "browser_take_snapshot".into(),
            arguments_json: "{}".into(),
        };
        assert_eq!(
            tools.run_read(&forged).await.unwrap_err().kind,
            desk_agent_protocol::AgentErrorKind::PermissionDenied
        );
        let request = build_permission_request(&ToolCall {
            id: "web-plan".into(),
            name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"items":[{"item_id":"web", "tool_name":tool, "exact_input":{"url":"https://example.com/docs"}, "reason":"Read public documentation"}]}).to_string(),
        }, registry, "web-permission".into(), session.input_revision, now.to_rfc3339()).unwrap();
        session.add_permission_request(request.clone()).unwrap();
        session.last_event_seq += 1;
        let event = PermissionRequestedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "web-permission-event".into(),
                run_id: input.run_id.clone(),
                event_seq: session.last_event_seq,
                input_revision: session.input_revision,
                kind: AgentRunEventKind::PermissionRequested,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: vec![],
                result_envelope_ids: vec![],
                created_at: now.to_rfc3339(),
            },
            request,
        };
        sessions
            .save_permission_request(&mut session, &event)
            .await
            .unwrap();
        if stop_before_resume {
            let mut unapproved = event.clone();
            unapproved.request.request_id = "unapproved-web-permission".into();
            unapproved.event.event_id = "unapproved-web-permission-event".into();
            unapproved.event.correlation_id = Some(unapproved.request.request_id.clone());
            session.last_event_seq += 1;
            unapproved.event.event_seq = session.last_event_seq;
            session
                .add_permission_request(unapproved.request.clone())
                .unwrap();
            sessions
                .save_permission_request(&mut session, &unapproved)
                .await
                .unwrap();
        }
        session
            .conversation
            .iter_mut()
            .find(|message| message.message_id == input.message.message_id)
            .unwrap()
            .turn_id = session.current_turn_id.clone();
        session.finish_turn(TurnState::Idle, now.to_rfc3339());
        sessions.save(&mut session).await.unwrap();
        let queued = if stop_before_resume {
            None
        } else {
            let next = AppendUserFollowupParams {
                event_id: "queued-personal-web".into(),
                message: params("new-event", "queued-personal-web", "Check the next item").message,
                ..input.clone()
            };
            let ask = desk_agent_protocol::ai_assistant::AiAssistantAsk {
                question: next.message.text.clone(),
                client_message_id: next.message.message_id.clone(),
                client_timezone: next.client_timezone.clone(),
                conversation_id: next.client_conversation_id.clone(),
                ..Default::default()
            };
            let OwnerAdmissionResult::Queued(queued) = store
                .admit_user_input(
                    &next,
                    None,
                    OwnerAdmission::Enqueue {
                        ask: &ask,
                        limit: 4,
                        reply_connection_id: None,
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected a durable queued request")
            };
            assert!(matches!(
                store
                    .admit_user_input(
                        &next,
                        None,
                        OwnerAdmission::Activate {
                            request_id: &queued.request_id
                        }
                    )
                    .await
                    .unwrap(),
                OwnerAdmissionResult::Waiting
            ));
            Some((next, queued.request_id.clone()))
        };
        let item = &event.request.items[0];
        let inventory = [CapabilityAvailability {
            provider_id: item.provider_id.clone(),
            capability_id: registry
                .capability_for_tool(tool)
                .unwrap()
                .wire
                .capability_id
                .clone(),
            tool_name: tool.into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        let decide = |actor, request_id| {
            sessions.decide_permission_request(
                &input.run_id,
                actor,
                "",
                request_id,
                vec![PermissionDecisionItem {
                    item_id: item.item_id.clone(),
                    decision: PermissionItemDecision::Approve {
                        resource_scope: item.resource_scope.clone(),
                        operation_scope: item.operation_scope.clone(),
                        export_destinations: vec![],
                        ttl_seconds: 60,
                        max_uses: 1,
                    },
                }],
                PermissionGrantIssuanceContext {
                    surface: ProductSurface::OssPersonalOwner,
                    registry,
                    inventory: &inventory,
                    readiness_revision: 1,
                    now_unix_ms: now.timestamp_millis() as u64,
                    implicit_fresh_object_refs: &[],
                },
                &input.created_at,
            )
        };
        let request_id = &event.request.request_id;
        assert!(decide("another-owner", request_id).await.is_err());
        let first = decide(&input.actor_id, request_id).await.unwrap();
        assert!(first.newly_recorded);
        assert_eq!(first.state, PermissionRequestState::Approved);
        assert!(
            !decide(&input.actor_id, request_id)
                .await
                .unwrap()
                .newly_recorded
        );
        let snapshot = sessions
            .read_assistant_snapshot_for_subject(&input.run_id, &input.actor_id, "")
            .await
            .unwrap()
            .unwrap();
        let [grant] = snapshot.capability_grants.as_slice() else {
            panic!("expected a single web grant")
        };
        assert_eq!(grant.tool_name, tool);
        assert!(grant.target_device_id.is_empty());
        assert_eq!(
            grant.issued_by,
            desk_agent_protocol::capability_grant::CapabilityGrantIssuer::UserDecision
        );
        assert!(
            tools
                .run_read(&fetch)
                .await
                .unwrap_err()
                .message
                .contains("lease is unavailable")
        );
        let candidates = sessions.permission_resume_candidates(0, 1).await.unwrap();
        let [candidate] = candidates.as_slice() else {
            panic!("expected a single pending decision")
        };
        let pending = sessions
            .pending_permission_resume(candidate, now)
            .await
            .unwrap()
            .unwrap();
        if let Some((next, request_id)) = &queued {
            assert!(matches!(
                store
                    .admit_user_input(next, None, OwnerAdmission::Activate { request_id })
                    .await
                    .unwrap(),
                OwnerAdmissionResult::Waiting
            ));
        }
        let mut claim = ClaimTurnParams {
            conversation_id: input.run_id.clone(),
            actor_id: input.actor_id.clone(),
            device_id: String::new(),
            policy_revision: input.policy_revision,
            current_pdp_scope: input.current_scope.clone(),
            turn_id: crate::agent_session_store::permission_resume::turn_id(
                &input.run_id,
                &event.request.request_id,
            ),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::PermissionDecision,
            now: now.to_rfc3339(),
        };
        assert!(sessions.claim_turn(claim.clone()).await.is_err());
        let resume = sessions.clone().with_permission_resume(
            request_id.clone(),
            pending.version,
            snapshot.capability_grants,
        );
        if stop_before_resume {
            let stops = crate::agent_subagent_store::SubAgentStore::new(db);
            let control = AiAssistantStopControl {
                client_request_id: "stop-personal-web".into(),
                expected_input_revision: pending.input_revision,
                expected_control_revision: pending.control_revision,
                subagent_choice: None,
            };
            assert!(
                stops
                    .stop_for_owner(&input.run_id, "another-owner", "", &control)
                    .await
                    .is_err()
            );
            let stopped = stops
                .stop_for_owner(&input.run_id, &input.actor_id, "", &control)
                .await
                .unwrap();
            assert_eq!(
                stopped,
                stops
                    .stop_for_owner(&input.run_id, &input.actor_id, "", &control)
                    .await
                    .unwrap()
            );
            assert_eq!(
                stopped.result.control_revision,
                control.expected_control_revision + 1
            );
            assert!(resume.claim_turn(claim.clone()).await.is_err());
            assert!(
                sessions
                    .pending_permission_resume(candidate, Utc::now())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                sessions
                    .permission_resume_candidates(0, 1)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let snapshot = sessions
                .read_assistant_snapshot_for_subject(&input.run_id, &input.actor_id, "")
                .await
                .unwrap()
                .unwrap();
            assert!(snapshot.session.main_stopped);
            assert_eq!(
                snapshot.session.permission_requests[1].state,
                PermissionRequestState::Withdrawn
            );
            assert_eq!(snapshot.capability_grants.len(), 1);
            assert!(
                decide(&input.actor_id, "unapproved-web-permission")
                    .await
                    .is_err()
            );
            assert!(
                !decide(&input.actor_id, request_id)
                    .await
                    .unwrap()
                    .newly_recorded
            );
            let next = AppendUserFollowupParams {
                event_id: "after-personal-stop".into(),
                message: params("new-event", "after-stop", "A new personal request").message,
                created_at: Utc::now().to_rfc3339(),
                ..input.clone()
            };
            let receipt = store.append_user_followup(&next).await.unwrap();
            claim.trigger_origin = TriggerOrigin::User;
            claim.turn_id = "after-stop-turn".into();
            claim.request_id = Some("after-stop-request".into());
            claim.now = next.created_at;
            let mut claimed = sessions
                .clone()
                .with_expected_input_revision(receipt.input_revision)
                .claim_turn(claim)
                .await
                .unwrap();
            assert!(!claimed.main_stopped);
            assert_eq!(claimed.input_revision, pending.input_revision + 1);
            assert_eq!(claimed.control_revision, stopped.result.control_revision);
            let active_control = AiAssistantStopControl {
                client_request_id: "stop-active-personal-web".into(),
                expected_input_revision: claimed.input_revision,
                expected_control_revision: claimed.control_revision,
                subagent_choice: None,
            };
            let stopped = stops
                .stop_for_owner(&input.run_id, &input.actor_id, "", &active_control)
                .await
                .unwrap();
            assert_eq!(stopped.cancel_request_ids, ["after-stop-request"]);
            claimed.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
            assert!(sessions.save(&mut claimed).await.is_err());
            return;
        }
        let mut resumed = resume.claim_turn(claim.clone()).await.unwrap();
        assert_eq!(resumed.input_revision, session.input_revision);
        assert_eq!(resumed.chain_id, session.chain_id);
        assert!(resume.claim_turn(claim.clone()).await.is_err());
        resumed.finish_turn(TurnState::Idle, now.to_rfc3339());
        resume.save(&mut resumed).await.unwrap();
        assert!(
            resume
                .pending_permission_resume(candidate, now)
                .await
                .unwrap()
                .is_none()
        );
        let (next, request_id) = queued.unwrap();
        let next_receipt = store
            .admit_user_input(
                &next,
                None,
                OwnerAdmission::Activate {
                    request_id: &request_id,
                },
            )
            .await
            .unwrap()
            .native()
            .unwrap();
        assert_eq!(next_receipt.input_revision, resumed.input_revision + 1);
        let saved = main_request::load(&db, &next.actor_id, &next.run_id, &request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.input_revision,
            Some(next_receipt.input_revision as i64)
        );
        assert!(saved.pending_input_json.is_some());
        let queued_sessions = sessions
            .with_expected_input_revision(next_receipt.input_revision)
            .with_queued_input(request_id.clone(), next.message.message_id.clone());
        claim.trigger_origin = TriggerOrigin::User;
        claim.turn_id = "queued-owner-turn".into();
        let next_claim = queued_sessions.claim_turn(claim).await.unwrap();
        assert_eq!(next_claim.input_revision, next_receipt.input_revision);
        assert!(
            main_request::load(&db, &next.actor_id, &next.run_id, &request_id)
                .await
                .unwrap()
                .unwrap()
                .pending_input_json
                .is_none()
        );
    }

    #[tokio::test]
    async fn main_request_queue_recovers_activated_input_after_file_reopen() {
        verify_main_request_recovery(false).await;
    }

    #[tokio::test]
    async fn main_request_queue_stop_and_resume_preserve_the_unclaimed_activation() {
        verify_main_request_recovery(true).await;
    }

    #[tokio::test]
    async fn workbench_activation_preserves_the_previous_work_session() {
        use crate::entity::agent_delegation_group as group;
        use desk_diagnose_core::{native_ids::workbench_work_conversation, session::TurnState};
        use desk_diagnose_core::{
            seam::{ClaimTurnParams, SessionSeam},
            session::TriggerOrigin,
        };

        let directory = tempfile::tempdir().unwrap();
        let db = file_db(&directory.path().join("workbench.sqlite")).await;
        let store = SignalAgentRunEventStore::new(db.clone());
        let now = chrono::Utc::now();
        let mut a = params("event-a", "work-a", "Check work A");
        let mut b = params("event-b", "work-b", "Check work B");
        let workbench_key = desk_diagnose_core::conversation_key::derive_assistant_conversation_key(
            &a.actor_id,
            desk_diagnose_core::conversation_key::AssistantConversationScope::Personal,
            a.client_conversation_id.as_deref().unwrap(),
        );
        let workbench_id = workbench_key.as_str();
        for input in [&mut a, &mut b] {
            input.device_id.clear();
            input.run_id = workbench_id.into();
            input.policy_revision =
                desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
            input.created_at = now.to_rfc3339();
            input.read_context =
                Some(ReadContextSelection::capture(&[], &input.current_scope).unwrap());
        }
        let mut root = PersistedAgentSession::new_workbench_coordinator(
            &a.actor_id,
            workbench_id,
            None,
            a.policy_revision,
            a.current_scope.clone(),
            &a.created_at,
        )
        .unwrap();
        root.adopt_client_metadata(
            a.client_conversation_id.as_deref(),
            AgentSessionSurface::AiAssistant,
        );
        let root_row = agent_session::ActiveModel {
            conversation_id: Set(root.conversation_id.clone()),
            actor_id: Set(root.actor_id.clone()),
            device_id: Set(String::new()),

            state_json: Set(root.encode_json_for_storage().unwrap()),
            version: Set(root.version),
            lease_token: Set(0),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let mut request_ids = Vec::new();
        for input in [&mut a, &mut b] {
            let ask = desk_agent_protocol::ai_assistant::AiAssistantAsk {
                question: input.message.text.clone(),
                client_message_id: input.message.message_id.clone(),
                client_timezone: input.client_timezone.clone(),
                conversation_id: input.client_conversation_id.clone(),
                ..Default::default()
            };
            let OwnerAdmissionResult::Queued(row) = store
                .admit_user_input(
                    input,
                    None,
                    OwnerAdmission::Enqueue {
                        ask: &ask,
                        limit: 4,
                        reply_connection_id: None,
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected queued work");
            };
            input.run_id =
                workbench_work_conversation(&input.actor_id, workbench_id, &row.request_id);
            request_ids.push(row.request_id);
        }
        let activation_a = OwnerAdmission::Activate {
            request_id: &request_ids[0],
        };
        let activation_b = OwnerAdmission::Activate {
            request_id: &request_ids[1],
        };
        assert!(
            store
                .admit_user_input(&b, None, activation_a)
                .await
                .is_err()
        );
        assert!(matches!(
            store
                .admit_user_input(&b, None, activation_b)
                .await
                .unwrap(),
            OwnerAdmissionResult::Waiting
        ));
        let mut root_input = a.clone();
        root_input.run_id = workbench_id.into();
        assert!(
            store
                .admit_user_input(&root_input, None, OwnerAdmission::Native)
                .await
                .is_err()
        );

        let mut saved_a = None;
        for (input, activation, request_id) in [
            (&a, activation_a, &request_ids[0]),
            (&b, activation_b, &request_ids[1]),
        ] {
            let receipt = store
                .admit_user_input(input, None, activation)
                .await
                .unwrap()
                .native()
                .unwrap();
            assert_eq!(receipt.input_revision, 1);
            assert_eq!(receipt.main_request_id.as_ref(), Some(request_id));
            let replay = store
                .admit_user_input(input, None, activation)
                .await
                .unwrap()
                .native()
                .unwrap();
            assert!(!replay.newly_appended);
            let claim = ClaimTurnParams {
                conversation_id: input.run_id.clone(),
                actor_id: input.actor_id.clone(),
                device_id: input.device_id.clone(),
                turn_id: format!("turn-{request_id}"),
                request_id: Some(request_id.clone()),
                connection_id: None,
                policy_revision: input.policy_revision,
                current_pdp_scope: input.current_scope.clone(),
                now: input.created_at.clone(),
                trigger_origin: TriggerOrigin::User,
            };
            let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
                .with_client_metadata(
                    input.client_conversation_id.clone(),
                    AgentSessionSurface::AiAssistant,
                )
                .with_expected_input_revision(1)
                .with_queued_input(request_id.clone(), input.message.message_id.clone());
            let mut session = sessions.claim_turn(claim).await.unwrap();
            assert_eq!(session.agent_role.workbench_id(), Some(workbench_id));
            assert_eq!(
                session.agent_role.workbench_request_id(),
                Some(request_id.as_str())
            );
            let persisted = group::Entity::find()
                .filter(group::Column::RootConversationId.eq(&input.run_id))
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            let source = crate::agent_subagent_store::decode_creation(&persisted).unwrap();
            assert_eq!(source.source.workbench_id(), Some(workbench_id));
            assert_eq!(source.main_request_id.as_ref(), Some(request_id));
            assert_eq!(source.owner_requirement, input.message);
            assert!(
                main_request::load(&db, &input.actor_id, workbench_id, request_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .pending_input_json
                    .is_none()
            );
            if input.run_id == a.run_id {
                assert!(matches!(
                    store
                        .admit_user_input(&b, None, activation_b)
                        .await
                        .unwrap(),
                    OwnerAdmissionResult::Waiting
                ));
                session.automation_turns_used = 3;
                session.review_denials.insert("approval-a".into(), 2);
                session.finish_turn(TurnState::Idle, input.created_at.clone());
                sessions.save(&mut session).await.unwrap();
                saved_a = agent_session::Entity::find()
                    .filter(agent_session::Column::ConversationId.eq(&input.run_id))
                    .one(&db)
                    .await
                    .unwrap();
            }
        }
        assert_eq!(
            agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&a.run_id))
                .one(&db)
                .await
                .unwrap(),
            saved_a
        );
        let root_after = agent_session::Entity::find_by_id(root_row.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let root_after = PersistedAgentSession::decode_json(&root_after.state_json).unwrap();
        assert_eq!(
            root_after.coordinating_work.as_deref(),
            Some(request_ids[1].as_str())
        );
        assert_eq!(root_after.control_revision, root.control_revision);
        assert_eq!(root_after.input_revision, root.input_revision);
        assert_eq!(root_after.conversation, root.conversation);
        let stop = crate::agent_subagent_store::SubAgentStore::new(db.clone());
        let control = desk_agent_protocol::ai_assistant::subagent::AiAssistantStopControl {
            client_request_id: "stop-active-workbench-work".into(),
            expected_input_revision: 0,
            expected_control_revision: root_after.control_revision,
            subagent_choice: None,
        };
        let stopped = stop
            .stop_for_owner(workbench_id, &root_after.actor_id, "", &control)
            .await
            .unwrap();
        assert_eq!(stopped.result.input_revision, 0);
        assert_eq!(stopped.cancel_request_ids, vec![request_ids[1].clone()]);
        let work = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&b.run_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let work = PersistedAgentSession::decode_json(&work.state_json).unwrap();
        assert!(work.main_stopped);
        assert_eq!(work.turn_state, TurnState::Cancelled);
        assert_eq!(
            agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&a.run_id))
                .one(&db)
                .await
                .unwrap(),
            saved_a
        );
        let group = group::Entity::find()
            .filter(group::Column::RootConversationId.eq(&a.run_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(!group.parent_active);
        let revision = stop
            .resume_main_queue_for_owner(
                workbench_id,
                &root_after.actor_id,
                "",
                stopped.result.control_revision,
            )
            .await
            .unwrap();
        let root_row = agent_session::Entity::find_by_id(root_row.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let resumed = PersistedAgentSession::decode_json(&root_row.state_json).unwrap();
        assert_eq!(resumed.control_revision, revision);
        assert!(!resumed.main_stopped);
        assert!(resumed.coordinating_work.is_none());
    }

    async fn verify_main_request_recovery(stopped: bool) {
        use desk_diagnose_core::{
            seam::{ClaimTurnParams, SessionSeam},
            session::TriggerOrigin,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main-requests.sqlite");
        let db = file_db(&path).await;
        let events = SignalAgentRunEventStore::new(db.clone());
        let mut next = params("queued-event", "queued-input", "Analyze next");
        next.device_id.clear();
        next.policy_revision =
            desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
        next.run_id = desk_diagnose_core::conversation_key::derive_assistant_conversation_key(
            &next.actor_id,
            desk_diagnose_core::conversation_key::AssistantConversationScope::Personal,
            next.client_conversation_id.as_deref().unwrap(),
        );
        next.read_context = Some(ReadContextSelection::capture(&[], &next.current_scope).unwrap());
        let ask = desk_agent_protocol::ai_assistant::AiAssistantAsk {
            question: next.message.text.clone(),
            client_message_id: next.message.message_id.clone(),
            client_timezone: next.client_timezone.clone(),
            conversation_id: next.client_conversation_id.clone(),
            ..Default::default()
        };
        let OwnerAdmissionResult::Queued(row) = events
            .admit_user_input(
                &next,
                None,
                OwnerAdmission::Enqueue {
                    ask: &ask,
                    limit: 4,
                    reply_connection_id: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected queued input")
        };
        let activation = OwnerAdmission::Activate {
            request_id: &row.request_id,
        };
        let admitted = events
            .admit_user_input(&next, None, activation)
            .await
            .unwrap()
            .native()
            .unwrap();
        drop(events);
        drop(db);

        let db = file_db(&path).await;
        let heads = main_request::recovery_heads(&db, 0, 4).await.unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].request_id, row.request_id);
        assert_eq!(FrozenOwnerInput::decode(&heads[0]).unwrap().ask, ask);
        use crate::entity::agent_delegation_group as group;
        let persisted = group::Entity::find().one(&db).await.unwrap().unwrap();
        let source = crate::agent_subagent_store::decode_creation(&persisted).unwrap();
        assert_eq!(
            source.main_request_id.as_deref(),
            Some(row.request_id.as_str())
        );
        assert_eq!(source.root_conversation_id, next.run_id);
        assert_eq!(source.parent_input_revision, admitted.input_revision);
        assert_eq!(source.owner_requirement, next.message);
        let events = SignalAgentRunEventStore::new(db.clone());
        let replay = events
            .admit_user_input(&next, None, activation)
            .await
            .unwrap()
            .native()
            .unwrap();
        assert!(!replay.newly_appended);
        assert_eq!(replay.input_revision, admitted.input_revision);
        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
            .with_client_metadata(next.client_conversation_id.clone(), next.surface)
            .with_expected_input_revision(replay.input_revision)
            .with_queued_input(
                replay.main_request_id.unwrap(),
                next.message.message_id.clone(),
            );
        let claim = ClaimTurnParams {
            conversation_id: next.run_id.clone(),
            actor_id: next.actor_id.clone(),
            device_id: next.device_id.clone(),
            policy_revision: next.policy_revision,
            current_pdp_scope: next.current_scope.clone(),
            turn_id: "first-queued-turn".into(),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::User,
            now: next.created_at.clone(),
        };
        if stopped {
            let controls = crate::agent_subagent_store::SubAgentStore::new(db.clone());
            let stop = controls
                .stop_for_owner(
                    &next.run_id,
                    &next.actor_id,
                    "",
                    &desk_agent_protocol::ai_assistant::subagent::AiAssistantStopControl {
                        client_request_id: "pause-queued-input".into(),
                        expected_input_revision: admitted.input_revision,
                        expected_control_revision: 1,
                        subagent_choice: None,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                sessions.claim_turn(claim.clone()).await,
                Err(desk_diagnose_core::seam::ClaimError::Busy)
            ));
            let heads = main_request::recovery_heads(&db, 0, 4).await.unwrap();
            assert_eq!(heads.len(), 1);
            assert_eq!(heads[0].request_id, row.request_id);
            assert!(
                controls
                    .resume_main_queue_for_owner(&next.run_id, &next.actor_id, "", 1)
                    .await
                    .is_err()
            );
            let stopped_revision = stop.result.control_revision;
            let resumed = controls
                .resume_main_queue_for_owner(&next.run_id, &next.actor_id, "", stopped_revision)
                .await
                .unwrap();
            assert_eq!(resumed, stopped_revision + 1);
            assert_eq!(
                controls
                    .resume_main_queue_for_owner(&next.run_id, &next.actor_id, "", stopped_revision)
                    .await
                    .unwrap(),
                resumed
            );
        }
        let owner = sessions.claim_turn(claim.clone()).await.unwrap();
        assert_eq!(owner.input_revision, admitted.input_revision);
        assert!(sessions.claim_turn(claim).await.is_err());
        assert!(
            main_request::recovery_heads(&db, 0, 4)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn durable_ack_is_idempotent_and_survives_file_reopen() {
        let path =
            std::env::temp_dir().join(format!("lrdm-agent-run-event-{}.db", uuid::Uuid::new_v4()));
        let db = file_db(&path).await;
        let store = SignalAgentRunEventStore::new(db.clone());
        let first = store
            .append_user_followup(&params("event-1", "message-1", "first"))
            .await
            .unwrap();
        let duplicate = store
            .append_user_followup(&params("event-1", "message-1", "first"))
            .await
            .unwrap();
        assert_eq!(duplicate.event_seq, first.event_seq);
        assert!(first.newly_appended);
        assert!(!duplicate.newly_appended);
        assert!(!duplicate.already_handled);
        let second = store
            .append_user_followup(&params("event-2", "message-2", "second"))
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
        assert_eq!(
            serde_json::to_value(&session).unwrap()["assistant_request_targets"],
            serde_json::json!({"input_revision": 2, "device_ids": ["device-1"]})
        );
        assert!(session.admits_assistant_device("device-1"));
        assert!(!session.admits_assistant_device("other-device"));
        assert_eq!(session.latest_input_seq, 2);
        assert_eq!(session.input_revision, 2);
        assert_eq!(session.handled_input_seq, 0);
        assert_eq!(session.conversation.len(), 2);
        reopened.close().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn foreign_actor_cannot_create_personal_assistant_input() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        let mut input = params(
            "foreign-input-event",
            "foreign-input-message",
            "Investigate this project",
        );
        input.actor_id = "foreign-test-actor".into();
        assert!(store.append_user_followup(&input).await.is_err());
        assert!(
            agent_session::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            agent_run_event::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::entity::agent_assistant_runtime::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn direct_goal_input_and_opening_are_one_idempotent_transaction() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let mut configured = crate::goal_budget_policy::read(&db).await.unwrap().limits;
        configured.model_calls = Some(4);
        configured.deadline_ms = Some(7 * 86_400_000);
        crate::goal_budget_policy::update(
            &db,
            &desk_agent_protocol::ai_assistant::goal_budget::UpdateGoalBudgetPolicy {
                expected_revision: 0,
                limits: configured,
                device_unavailable_max_ms:
                    desk_diagnose_core::goal_budget::DEFAULT_DEVICE_UNAVAILABLE_MAX_MS,
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
                &params("event-goal-input", "message-goal", "Finish the report"),
                start.clone(),
            )
            .await
            .unwrap();
        let retry = store
            .append_user_goal(
                &params("event-goal-input", "message-goal", "Finish the report"),
                start.clone(),
            )
            .await
            .unwrap();
        assert!(first.newly_appended);
        assert!(!retry.newly_appended);
        assert_eq!(first.input_revision, retry.input_revision);
        let goal =
            crate::agent_goal_store::load_for_subject(&db, "run-1", &owner_actor(), "device-1")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(goal.source_message_id, "message-goal");
        assert_eq!(goal.state, desk_diagnose_core::goal::GoalState::Queued);
        assert_eq!(goal.limits.model_calls, 4);
        let mut extended = configured;
        extended.deadline_ms = Some(8 * 86_400_000);
        extended.model_calls = Some(1);
        crate::goal_budget_policy::update(
            &db,
            &desk_agent_protocol::ai_assistant::goal_budget::UpdateGoalBudgetPolicy {
                expected_revision: 1,
                limits: extended,
                device_unavailable_max_ms:
                    desk_diagnose_core::goal_budget::DEFAULT_DEVICE_UNAVAILABLE_MAX_MS,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            crate::agent_goal_store::continuation_candidates(
                &db,
                goal.deadline_unix_ms + 1,
                8,
                None
            )
            .await
            .unwrap()
            .len(),
            1
        );
        let retry_after_policy_change = store
            .append_user_goal(
                &params("event-goal-input", "message-goal", "Finish the report"),
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
            &owner_actor(),
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

        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        store
            .append_user_followup(&params("event-1", "message-1", "Original goal"))
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
            owner_actor(),
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
            &db,
            "run-1",
            &owner_actor(),
            "device-1",
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
        store.append_user_goal(&input, start.clone()).await.unwrap();
        let new_goal =
            crate::agent_goal_store::load_for_subject(&db, "run-1", &owner_actor(), "device-1")
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
            &owner_actor(),
            "device-1",
        )
        .await
        .unwrap();
        assert_eq!(old.used.slices, 1);
        let mut altered = start;
        altered.previous_completed_goal_id = None;
        assert!(store.append_user_goal(&input, altered).await.is_err());
    }

    #[tokio::test]
    async fn owner_goal_controls_are_fenced_and_durable() {
        use desk_diagnose_core::goal::{GoalOwnerAction, GoalState};
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        store
            .append_user_goal(
                &params(
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
        let opened =
            crate::agent_goal_store::load_for_subject(&db, "run-1", &owner_actor(), "device-1")
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
            &owner_actor(),
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
                &owner_actor(),
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
            &owner_actor(),
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
            crate::agent_goal_store::load_for_subject(&db, "run-1", &owner_actor(), "device-1")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(persisted.state_version, resumed.state_version);
        assert_eq!(persisted.limits, opened.limits);
    }

    #[tokio::test]
    async fn later_input_revises_goal_and_original_goal_input_stays_idempotent() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
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
            .append_user_goal(&original, start.clone())
            .await
            .unwrap();
        let mut next = params("event-revision", "message-revision", "Use the new figures");
        next.created_at = "2026-08-25T00:01:00Z".into();
        let revised = store.append_user_followup(&next).await.unwrap();
        let replay = store.append_user_goal(&original, start).await.unwrap();
        assert_eq!(first.input_revision, 1);
        assert_eq!(revised.input_revision, 2);
        assert!(!replay.newly_appended);
        let goal =
            crate::agent_goal_store::load_for_subject(&db, "run-1", &owner_actor(), "device-1")
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
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalAgentRunEventStore::new(db.clone());
        store
            .append_user_followup(&params("first-event", "first-message", "Work on a report"))
            .await
            .unwrap();
        let now_ms = chrono::DateTime::parse_from_rfc3339("2026-08-25T00:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        let pending = desk_diagnose_core::goal::GoalOpenRequest::new(
            "proposal".into(),
            "run-1".into(),
            owner_actor(),
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
        let accepted = store.append_user_followup(&next).await.unwrap();
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

    #[tokio::test]
    async fn each_client_input_freezes_its_timezone_and_rejects_changed_replays() {
        let dir = tempfile::tempdir().unwrap();
        let db = file_db(&dir.path().join("client-timezones.db")).await;
        let store = SignalAgentRunEventStore::new(db);
        let mut east = params("east-event", "east-message", "昨天做了什么");
        east.created_at = "2026-10-28T17:30:00Z".into();
        east.client_timezone = "Asia/Shanghai".into();
        store.append_user_followup(&east).await.unwrap();
        let mut west = params("west-event", "west-message", "昨天做了什么");
        west.created_at = east.created_at.clone();
        west.client_timezone = "America/Los_Angeles".into();
        store.append_user_followup(&west).await.unwrap();
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&east.run_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let restored = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        let data = restored.assistant_data.as_ref().unwrap();
        assert_eq!(data.current_input_timezone, "America/Los_Angeles");
        assert_eq!(
            data.history_access_scope.as_ref().unwrap().timezone,
            "America/Los_Angeles"
        );
        let records = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq(&east.run_id))
            .filter(agent_run_event::Column::Kind.eq("user_followup"))
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(records.len(), 2);
        let zones = records.iter().map(|row| serde_json::from_str::<serde_json::Value>(&row.payload_json).unwrap()["client_timezone"].as_str().unwrap().to_owned()).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            zones,
            ["Asia/Shanghai".to_owned(), "America/Los_Angeles".to_owned()]
                .into_iter()
                .collect()
        );
        let origins = crate::entity::agent_assistant_origin::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert!(origins.iter().any(
            |row| row.native_id == "east-message" && row.statement_timezone == "Asia/Shanghai"
        ));
        assert!(origins.iter().any(|row| row.native_id == "west-message"
            && row.statement_timezone == "America/Los_Angeles"));
        assert!(
            !store
                .append_user_followup(&east)
                .await
                .unwrap()
                .newly_appended
        );
        let mut changed = east.clone();
        changed.client_timezone = "America/Los_Angeles".into();
        assert!(store.append_user_followup(&changed).await.is_err());
    }
}
