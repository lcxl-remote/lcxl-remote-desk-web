//! Native save and optional read-only history share the original transaction.
//! No native/source SELECT or mutation follows the final history quota phase.

use super::*;
use crate::assistant_data_store::{self, AssistantStoreError};
use desk_agent_protocol::ai_assistant::history::HistoryAdmission;
use sea_orm::ConnectionTrait;

pub(super) async fn with_history(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    state_json: &str,
    new_version: i64,
    now: chrono::DateTime<chrono::Utc>,
    lease_deadline: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<sea_orm::UpdateResult, AgentError> {
    let txn = crate::db::begin_write(db, agent_session::Entity)
        .await
        .map_err(history_error)?;
    let mut prepared = desk_diagnose_core::future::boxed(|| {
        assistant_data_store::prepare_visible_history_on(&txn, session)
    })
    .await
    .map_err(history_error)?;
    desk_diagnose_core::future::boxed(|| prepared.observe_native_status_on(&txn))
        .await
        .map_err(history_error)?;
    let captures = prepared.captures();
    let requests = prepared.requests(&captures).map_err(history_error)?;
    let deadlines = prepared.deadlines().map_err(history_error)?;
    let pending = desk_diagnose_core::future::boxed(|| {
        assistant_data_store::history::staged::prepare_requests_on(
            &txn,
            &requests,
            &prepared.policy,
        )
    })
    .await
    .map_err(history_error)?;
    let held_until = prepared
        .finish_preparation_on(&txn)
        .await
        .map_err(history_error)?;
    let result = persist_on(
        &txn,
        session,
        state_json,
        new_version,
        now,
        lease_deadline,
        Some(prepared.original.as_ref()),
    )
    .await
    .map_err(history_error)?;
    if result.rows_affected != 1 {
        txn.rollback().await.map_err(history_error)?;
        return Ok(result);
    }
    if desk_diagnose_core::future::boxed(|| prepared.write_citations_on(&txn))
        .await
        .is_err()
    {
        log::warn!("assistant answer retained without an optional citation projection");
    }
    let outcomes = desk_diagnose_core::future::boxed(|| pending.finalize_on())
        .await
        .map_err(history_error)?;
    let completed = u64::try_from(chrono::Utc::now().timestamp_millis())
        .map_err(|_| history_error(AssistantStoreError::Inaccessible))?;
    if outcomes.len() != deadlines.len()
        || held_until.is_some_and(|until| until <= completed)
        || outcomes.iter().zip(deadlines).any(|(outcome, deadline)| {
            matches!(
                outcome.admission,
                HistoryAdmission::Indexed | HistoryAdmission::Redacted
            ) && deadline.is_some_and(|until| until <= completed)
        })
    {
        return Err(history_error(AssistantStoreError::Inaccessible));
    }
    txn.commit().await.map_err(history_error)?;
    Ok(result)
}

/// The original version/lease CAS is retained for every surface. Personal
/// history additionally fences the exact row read under its original owner.
pub(crate) async fn persist_on<C: ConnectionTrait>(
    db: &C,
    session: &PersistedAgentSession,
    state_json: &str,
    new_version: i64,
    now: chrono::DateTime<chrono::Utc>,
    lease_deadline: Option<chrono::DateTime<chrono::Utc>>,
    original: Option<&agent_session::Model>,
) -> Result<sea_orm::UpdateResult, sea_orm::DbErr> {
    // A native caller without an assistant-data scope retains the original
    // save path. It cannot drop a scope already present in the durable row.
    let absent_scope = if original.is_none()
        && session.surface == AgentSessionSurface::AiAssistant
        && session.assistant_data.is_none()
    {
        let Some(row) = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .one(db)
            .await?
        else {
            return Ok(sea_orm::UpdateResult { rows_affected: 0 });
        };
        let persisted = Box::new(
            PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| sea_orm::DbErr::Custom("invalid native assistant scope".into()))?,
        );
        if persisted.assistant_data.is_some()
            || persisted.actor_id != session.actor_id
            || persisted.device_id != session.device_id
            || persisted.surface != session.surface
            || persisted.agent_role != session.agent_role
        {
            return Err(sea_orm::DbErr::Custom(
                "native save cannot remove or replace an assistant data scope".into(),
            ));
        }
        Some(Box::new(row))
    } else {
        None
    };
    let original = original.or(absent_scope.as_deref());
    let mut query = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(state_json.to_owned()),
        )
        .col_expr(agent_session::Column::Version, Expr::value(new_version))
        .col_expr(
            agent_session::Column::LeaseDeadline,
            Expr::value(lease_deadline),
        )
        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_session::Column::Version.eq(session.version))
        .filter(agent_session::Column::LeaseToken.eq(session.lease_token as i64));

    if let Some(row) = original {
        query = query
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::ActorId.eq(&row.actor_id))
            .filter(agent_session::Column::DeviceId.eq(&row.device_id))
            .filter(agent_session::Column::StateJson.eq(&row.state_json));
    }
    query.exec(db).await
}

fn history_error(error: impl Into<AssistantStoreError>) -> AgentError {
    let error = error.into();
    match error {
        AssistantStoreError::Backend(error) => internal(format!("save agent session: {error}")),
        _ => internal("agent session or original assistant source changed while saving"),
    }
}
