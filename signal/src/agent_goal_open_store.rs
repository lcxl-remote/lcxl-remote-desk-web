//! Durable pending AI-proposed goal requests for the OSS AI Assistant.

use crate::entity::{
    agent_goal_open_request as request_row, agent_goal_run as goal_row, agent_run_event,
    agent_session,
};
use chrono::{DateTime, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::dynamic_run::{AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEventKind};
use desk_diagnose_core::goal::{
    GOAL_SCHEMA_VERSION, GoalLedgerEvent, GoalLimits, GoalModelBinding, GoalOpenRequest,
    GoalOpenRequestEvent, GoalOpenRequestState, GoalRun,
};
use desk_diagnose_core::session::{
    AgentSessionSurface, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Set,
};
use sha2::{Digest, Sha256};

fn invalid() -> DbErr {
    DbErr::Custom("AI Assistant goal opening request is inconsistent".into())
}

fn as_i64(value: u64) -> Result<i64, DbErr> {
    value.try_into().map_err(|_| invalid())
}

fn failed(message: &'static str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub async fn save_model_request(
    db: &DatabaseConnection,
    session: &mut PersistedAgentSession,
    event: &GoalOpenRequestEvent,
) -> Result<(), AgentError> {
    event
        .validate()
        .map_err(|_| failed("invalid goal opening request"))?;
    if session.surface != AgentSessionSurface::AiAssistant
        || session.trigger_origin != TriggerOrigin::User
        || session.turn_state != TurnState::Running
        || event.event.run_id != session.conversation_id
        || event.request.owner_id != session.actor_id
        || event.request.device_id != session.device_id
        || event.event.event_seq != session.last_event_seq
        || event.request.input_revision != session.input_revision
    {
        return Err(failed(
            "goal opening request does not match the current turn",
        ));
    }
    let now = DateTime::parse_from_rfc3339(&event.event.created_at)
        .map_err(|_| failed("invalid goal opening timestamp"))?
        .with_timezone(&Utc);
    let txn = crate::db::begin_write(db, agent_session::Entity)
        .await
        .map_err(|_| failed("goal opening storage is unavailable"))?;
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&session.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&session.device_id))
        .one(&txn)
        .await
        .map_err(|_| failed("goal opening storage is unavailable"))?
        .ok_or_else(|| failed("goal conversation is unavailable"))?;
    let active_goal = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(&txn)
        .await
        .map_err(|_| failed("goal storage is unavailable"))?
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()
        .map_err(|_| failed("goal state is invalid"))?;
    let proposal_matches_goal = match (&event.request.target_goal_id, active_goal.as_ref()) {
        (None, None) => true,
        (Some(id), Some(goal)) => {
            id == &goal.goal_id
                && event.request.target_goal_revision == Some(goal.goal_revision)
                && goal.can_propose_revision(session.input_revision).is_ok()
        }
        _ => false,
    };
    if let Some(previous_id) = &event.request.previous_completed_goal_id {
        let previous = crate::agent_goal_store::load_completed_on(
            &txn,
            previous_id,
            &session.conversation_id,
            &session.actor_id,
            &session.device_id,
        )
        .await
        .map_err(|_| failed("previous completed goal is unavailable"))?;
        desk_diagnose_core::goal::GoalCompletionReference::from_completed(
            &previous,
            &session.conversation_id,
            &session.actor_id,
            &session.device_id,
        )
        .map_err(|_| failed("previous completed goal is invalid"))?;
    }
    if row.version != session.version
        || row.lease_token
            != as_i64(session.lease_token).map_err(|_| failed("invalid goal lease"))?
        || row.lease_deadline.is_none_or(|deadline| deadline < now)
        || request_row::Entity::find()
            .filter(request_row::Column::ConversationId.eq(&session.conversation_id))
            .filter(request_row::Column::Status.eq("pending"))
            .one(&txn)
            .await
            .map_err(|_| failed("goal opening storage is unavailable"))?
            .is_some()
        || !proposal_matches_goal
        || desk_diagnose_core::permission_resume::latest_user_requirement(&session.conversation)
            .is_none_or(|message| message.message_id != event.request.source_message_id)
    {
        return Err(failed("a goal is already active or awaiting approval"));
    }
    let mut stored = session.clone();
    desk_diagnose_core::image_input::strip_session_images(&mut stored.conversation);
    stored.version = row
        .version
        .checked_add(1)
        .ok_or_else(|| failed("goal conversation version exhausted"))?;
    stored.updated_at = event.event.created_at.clone();
    let state_json = stored
        .encode_json_for_storage()
        .map_err(|_| failed("invalid goal conversation state"))?;
    let changed = agent_session::Entity::update_many()
        .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
        .col_expr(agent_session::Column::Version, Expr::value(stored.version))
        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
        .filter(agent_session::Column::Id.eq(row.id))
        .filter(agent_session::Column::Version.eq(row.version))
        .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
        .exec(&txn)
        .await
        .map_err(|_| failed("goal opening storage is unavailable"))?;
    if changed.rows_affected != 1 {
        return Err(failed(
            "goal conversation changed before proposal was saved",
        ));
    }
    let mut stored_event = event.clone();
    if stored_event.request.target_goal_id.is_none() {
        let policy = crate::goal_budget_policy::read(&txn)
            .await
            .map_err(|_| failed("goal budget policy is unavailable"))?;
        stored_event.request.limits = desk_diagnose_core::goal_budget::effective_limits(&policy)
            .map_err(|_| failed("invalid goal budget policy"))?;
        stored_event
            .validate()
            .map_err(|_| failed("invalid goal proposal"))?;
    }
    insert_on(&txn, &stored_event.request)
        .await
        .map_err(|_| failed("goal opening request could not be saved"))?;
    agent_run_event::ActiveModel {
        event_id: Set(event.event.event_id.clone()),
        run_id: Set(event.event.run_id.clone()),
        event_seq: Set(
            as_i64(event.event.event_seq).map_err(|_| failed("invalid goal event sequence"))?
        ),
        input_revision: Set(as_i64(event.event.input_revision)
            .map_err(|_| failed("invalid goal input revision"))?),
        kind: Set(event.event.kind.as_str().into()),
        correlation_id: Set(Some(event.request.request_id.clone())),
        input_seq: Set(None),
        actor_id: Set(Some(session.actor_id.clone())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&stored_event)
            .map_err(|_| failed("invalid goal event payload"))?),
        payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await
    .map_err(|_| failed("goal opening event could not be saved"))?;
    txn.commit()
        .await
        .map_err(|_| failed("goal opening transaction could not be committed"))?;
    session.version = stored.version;
    session.updated_at = stored.updated_at;
    Ok(())
}

fn project(request: &GoalOpenRequest) -> Result<request_row::ActiveModel, DbErr> {
    request.validate().map_err(|_| invalid())?;
    Ok(request_row::ActiveModel {
        request_id: Set(request.request_id.clone()),
        conversation_id: Set(request.conversation_id.clone()),
        actor_id: Set(request.owner_id.clone()),
        device_id: Set(request.device_id.clone()),
        source_message_id: Set(request.source_message_id.clone()),
        input_revision: Set(as_i64(request.input_revision)?),
        goal_text: Set(request.goal_text.clone()),
        target_goal_id: Set(request.target_goal_id.clone()),
        target_goal_revision: Set(request.target_goal_revision.map(as_i64).transpose()?),
        previous_completed_goal_id: Set(request.previous_completed_goal_id.clone()),
        limits_json: Set(serde_json::to_string(&request.limits).map_err(|_| invalid())?),
        model_binding_json: Set(
            serde_json::to_string(&request.model_binding).map_err(|_| invalid())?
        ),
        status: Set(request.state.as_str().into()),
        expires_at: Set(as_i64(request.expires_at_unix_ms)?),
        decided_at: Set(request.decided_at_unix_ms.map(as_i64).transpose()?),
        decision_event_id: Set(request.decision_event_id.clone()),
        resulting_goal_id: Set(request.resulting_goal_id.clone()),
        created_at: Set(as_i64(request.created_at_unix_ms)?),
        ..Default::default()
    })
}

pub(crate) fn decode(row: &request_row::Model) -> Result<GoalOpenRequest, DbErr> {
    let request = GoalOpenRequest {
        schema_version: GOAL_SCHEMA_VERSION,
        request_id: row.request_id.clone(),
        conversation_id: row.conversation_id.clone(),
        owner_id: row.actor_id.clone(),
        device_id: row.device_id.clone(),
        source_message_id: row.source_message_id.clone(),
        input_revision: row.input_revision.try_into().map_err(|_| invalid())?,
        goal_text: row.goal_text.clone(),
        target_goal_id: row.target_goal_id.clone(),
        target_goal_revision: row
            .target_goal_revision
            .map(|value| value.try_into().map_err(|_| invalid()))
            .transpose()?,
        previous_completed_goal_id: row.previous_completed_goal_id.clone(),
        limits: serde_json::from_str::<GoalLimits>(&row.limits_json).map_err(|_| invalid())?,
        model_binding: serde_json::from_str::<GoalModelBinding>(&row.model_binding_json)
            .map_err(|_| invalid())?,
        state: GoalOpenRequestState::from_code(&row.status).map_err(|_| invalid())?,
        created_at_unix_ms: row.created_at.try_into().map_err(|_| invalid())?,
        expires_at_unix_ms: row.expires_at.try_into().map_err(|_| invalid())?,
        decided_at_unix_ms: row
            .decided_at
            .map(|value| value.try_into().map_err(|_| invalid()))
            .transpose()?,
        decision_event_id: row.decision_event_id.clone(),
        resulting_goal_id: row.resulting_goal_id.clone(),
    };
    request.validate().map_err(|_| invalid())?;
    Ok(request)
}

pub(crate) async fn insert_on(
    txn: &DatabaseTransaction,
    request: &GoalOpenRequest,
) -> Result<(), DbErr> {
    project(request)?.insert(txn).await?;
    Ok(())
}

pub(crate) async fn replace_pending_on(
    txn: &DatabaseTransaction,
    request: &GoalOpenRequest,
) -> Result<bool, DbErr> {
    if request.state == GoalOpenRequestState::Pending {
        return Err(invalid());
    }
    let result = request_row::Entity::update_many()
        .set(project(request)?)
        .filter(request_row::Column::RequestId.eq(&request.request_id))
        .filter(request_row::Column::Status.eq("pending"))
        .exec(txn)
        .await?;
    Ok(result.rows_affected == 1)
}

pub(crate) async fn pending_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
) -> Result<Option<GoalOpenRequest>, DbErr> {
    let row = request_row::Entity::find()
        .filter(request_row::Column::ConversationId.eq(conversation_id))
        .filter(request_row::Column::ActorId.eq(actor_id))
        .filter(request_row::Column::DeviceId.eq(device_id))
        .filter(request_row::Column::Status.eq("pending"))
        .one(db)
        .await?;
    row.as_ref().map(decode).transpose()
}

/// An owner decision changes the request and (on approval) creates the goal in
/// the same transaction. No permission grant is issued by this operation.
pub async fn decide_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
    request_id: &str,
    approve: bool,
    now: DateTime<Utc>,
) -> Result<Option<(GoalOpenRequest, Option<GoalRun>)>, DbErr> {
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
        || !session.turn_state.can_claim()
        || session_row
            .lease_deadline
            .as_ref()
            .is_some_and(|deadline| deadline >= &now)
    {
        return Ok(None);
    }
    let Some(request_row) = request_row::Entity::find()
        .filter(request_row::Column::RequestId.eq(request_id))
        .filter(request_row::Column::ConversationId.eq(conversation_id))
        .filter(request_row::Column::ActorId.eq(actor_id))
        .filter(request_row::Column::DeviceId.eq(device_id))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let mut request = decode(&request_row)?;
    let active_goal = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(&txn)
        .await?
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()?;
    let target_matches = match (&request.target_goal_id, active_goal.as_ref()) {
        (None, None) => true,
        (Some(id), Some(goal)) => {
            id == &goal.goal_id
                && request.target_goal_revision == Some(goal.goal_revision)
                && goal.can_propose_revision(session.input_revision).is_ok()
        }
        _ => false,
    };
    if request.state != GoalOpenRequestState::Pending
        || request.input_revision != session.input_revision
        || !target_matches
    {
        return Ok(None);
    }
    let state = if now_ms >= request.expires_at_unix_ms {
        GoalOpenRequestState::Expired
    } else if approve {
        GoalOpenRequestState::Approved
    } else {
        GoalOpenRequestState::Denied
    };
    let decision_id =
        GoalOpenRequestEvent::id_for(request_id, state, AgentRunEventKind::GoalOpenDecided)
            .map_err(|_| invalid())?;
    let mut goal = if state == GoalOpenRequestState::Approved {
        let current_model_binding = GoalModelBinding::from_destination(
            &crate::model_provider::load(&txn)
                .await?
                .destination_identity()
                .map_err(|_| invalid())?,
        )
        .map_err(|_| invalid())?;
        if let Some(mut existing) = active_goal.clone() {
            request
                .approve_revision(
                    &mut existing,
                    session.input_revision,
                    &current_model_binding,
                    decision_id,
                    now_ms,
                )
                .map_err(|_| invalid())?;
            Some(existing)
        } else {
            let previous = if let Some(previous_id) = &request.previous_completed_goal_id {
                Some(
                    crate::agent_goal_store::load_completed_on(
                        &txn,
                        previous_id,
                        conversation_id,
                        actor_id,
                        device_id,
                    )
                    .await?,
                )
            } else {
                None
            };
            Some(
                request
                    .approve(
                        session.input_revision,
                        &current_model_binding,
                        format!("goal-{:x}", Sha256::digest(request_id.as_bytes())),
                        decision_id,
                        now_ms,
                        previous.as_ref(),
                    )
                    .map_err(|_| invalid())?,
            )
        }
    } else {
        request
            .close(state, decision_id, now_ms)
            .map_err(|_| invalid())?;
        None
    };
    if let Some(goal) = goal.as_mut() {
        goal.apply_budget_policy(&crate::goal_budget_policy::read(&txn).await?)
            .map_err(|_| invalid())?;
    }
    session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
    let decision = GoalOpenRequestEvent::new(
        &request,
        AgentRunEventKind::GoalOpenDecided,
        session.last_event_seq,
        now.to_rfc3339(),
    )
    .map_err(|_| invalid())?;
    let opened = if let Some(goal) = goal.as_ref() {
        session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
        Some(
            GoalLedgerEvent::new(
                goal,
                if request.target_goal_id.is_some() {
                    AgentRunEventKind::GoalRevised
                } else {
                    AgentRunEventKind::GoalOpened
                },
                session.last_event_seq,
                now.to_rfc3339(),
            )
            .map_err(|_| invalid())?,
        )
    } else {
        None
    };
    session.version = session_row.version.checked_add(1).ok_or_else(invalid)?;
    session.updated_at = now.to_rfc3339();
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
    if changed.rows_affected != 1 || !replace_pending_on(&txn, &request).await? {
        return Err(invalid());
    }
    if let Some(goal) = goal.as_ref() {
        if let Some(previous) = active_goal.as_ref() {
            if !crate::agent_goal_store::replace_on(
                &txn,
                goal,
                previous.state_version,
                previous.lease_epoch,
                None,
                None,
            )
            .await?
            {
                return Err(invalid());
            }
        } else {
            crate::agent_goal_store::insert_on(&txn, goal).await?;
        }
    }
    for (event, payload) in [
        (
            Some(&decision.event),
            Some(serde_json::to_string(&decision).map_err(|_| invalid())?),
        ),
        (
            opened.as_ref().map(|entry| &entry.event),
            opened
                .as_ref()
                .map(|entry| serde_json::to_string(entry).map_err(|_| invalid()))
                .transpose()?,
        ),
    ] {
        let (Some(event), Some(payload)) = (event, payload) else {
            continue;
        };
        agent_run_event::ActiveModel {
            event_id: Set(event.event_id.clone()),
            run_id: Set(event.run_id.clone()),
            event_seq: Set(as_i64(event.event_seq)?),
            input_revision: Set(as_i64(event.input_revision)?),
            kind: Set(event.kind.as_str().into()),
            correlation_id: Set(event.correlation_id.clone()),
            input_seq: Set(None),
            actor_id: Set(Some(actor_id.to_owned())),
            source_envelope_ids_json: Set("[]".into()),
            result_envelope_ids_json: Set("[]".into()),
            payload_json: Set(payload),
            payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }
    txn.commit().await?;
    Ok(Some((request, goal)))
}

pub async fn expire_due(
    db: &DatabaseConnection,
    now: DateTime<Utc>,
    limit: u64,
) -> Result<(), DbErr> {
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
    let rows = request_row::Entity::find()
        .filter(request_row::Column::Status.eq("pending"))
        .filter(request_row::Column::ExpiresAt.lte(as_i64(now_ms)?))
        .order_by_asc(request_row::Column::ExpiresAt)
        .limit(limit)
        .all(db)
        .await?;
    for row in rows {
        if let Err(error) = decide_for_subject(
            db,
            &row.conversation_id,
            &row.actor_id,
            &row.device_id,
            &row.request_id,
            false,
            now.clone(),
        )
        .await
        {
            log::warn!(
                "[ai-assistant-goal] failed to expire request {}: {error}",
                row.request_id
            );
        }
    }
    Ok(())
}
