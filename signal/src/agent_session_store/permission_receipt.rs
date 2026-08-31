//! Immutable permission receipts, independent of current grants and readiness.

use super::*;
use desk_diagnose_core::dynamic_run::{
    AgentRunEvent, AgentRunEventKind, PermissionDecidedEvent, PermissionDecisionItem,
    PermissionRequestState,
};
use sea_orm::ConnectionTrait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionDecisionOutcome {
    pub state: PermissionRequestState,
    pub newly_recorded: bool,
}

fn invalid() -> AgentError {
    internal("The original permission receipt or subject is inconsistent; refresh the session")
}

pub(super) fn session(
    row: &agent_session::Model,
    run_id: &str,
    actor_id: &str,
    device_id: &str,
) -> Result<PersistedAgentSession, AgentError> {
    let mut session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if row.conversation_id != run_id
        || row.actor_id != actor_id
        || row.device_id != device_id
        || session.conversation_id != run_id
        || session.actor_id != actor_id
        || session.device_id != device_id
        || session.surface != AgentSessionSurface::DeviceAssistant
        || row.version < 0
    {
        return Err(invalid());
    }
    session.version = row.version;
    Ok(session)
}

async fn event_row(
    db: &impl ConnectionTrait,
    run_id: &str,
    request_id: &str,
    kind: AgentRunEventKind,
) -> Result<Option<agent_run_event::Model>, AgentError> {
    let mut rows = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::RunId.eq(run_id))
        .filter(agent_run_event::Column::CorrelationId.eq(request_id))
        .filter(agent_run_event::Column::Kind.eq(kind.as_str()))
        .limit(2)
        .all(db)
        .await
        .map_err(|_| internal("permission receipt storage is unavailable"))?;
    if rows.len() > 1 {
        return Err(invalid());
    }
    Ok(rows.pop())
}

fn validate_event(
    row: &agent_run_event::Model,
    event: &AgentRunEvent,
    session: &PersistedAgentSession,
    request_id: &str,
) -> Result<(), AgentError> {
    event.validate().map_err(|_| invalid())?;
    let sources: Vec<String> =
        serde_json::from_str(&row.source_envelope_ids_json).map_err(|_| invalid())?;
    let results: Vec<String> =
        serde_json::from_str(&row.result_envelope_ids_json).map_err(|_| invalid())?;
    let created_at = DateTime::parse_from_rfc3339(&event.created_at).map_err(|_| invalid())?;
    if row.event_id != event.event_id
        || row.run_id != session.conversation_id
        || event.run_id != session.conversation_id
        || row.actor_id.as_deref() != Some(session.actor_id.as_str())
        || row.kind != event.kind.as_str()
        || row.correlation_id.as_deref() != Some(request_id)
        || event.correlation_id.as_deref() != Some(request_id)
        || row.event_seq != i64::try_from(event.event_seq).map_err(|_| invalid())?
        || row.input_revision != i64::try_from(event.input_revision).map_err(|_| invalid())?
        || row.payload_schema_version != i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)
        || row.input_seq.is_some()
        || sources != event.source_envelope_ids
        || results != event.result_envelope_ids
        || created_at.with_timezone(&Utc) != row.created_at
        || event.event_seq > session.last_event_seq
        || event.input_revision > session.input_revision
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) async fn requested_on(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
    request_id: &str,
) -> Result<PermissionRequestedEvent, AgentError> {
    let row = event_row(
        db,
        &session.conversation_id,
        request_id,
        AgentRunEventKind::PermissionRequested,
    )
    .await?
    .ok_or_else(invalid)?;
    let requested: PermissionRequestedEvent =
        serde_json::from_str(&row.payload_json).map_err(|_| invalid())?;
    requested.validate().map_err(|_| invalid())?;
    validate_event(&row, &requested.event, session, request_id)?;
    if requested.request.state != PermissionRequestState::Pending {
        return Err(invalid());
    }
    Ok(requested)
}

pub(super) async fn replay_on(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
    request_id: &str,
    decisions: &[PermissionDecisionItem],
) -> Result<Option<PermissionRequestState>, AgentError> {
    let Some(row) = event_row(
        db,
        &session.conversation_id,
        request_id,
        AgentRunEventKind::PermissionDecided,
    )
    .await?
    else {
        return Ok(None);
    };
    let event: PermissionDecidedEvent =
        serde_json::from_str(&row.payload_json).map_err(|_| invalid())?;
    event.validate().map_err(|_| invalid())?;
    validate_event(&row, &event.event, session, request_id)?;
    let mut requested = requested_on(db, session, request_id).await?;
    if requested.request.input_revision != event.request_input_revision
        || requested.event.event_seq >= event.event.event_seq
        || requested
            .request
            .apply_user_decision(&event.items)
            .map_err(|_| invalid())?
            != event.resulting_state
        || row.event_id
            != stable_event_id(
                "permission-decision",
                &format!(
                    "{}:{}:{}",
                    session.conversation_id, event.event.event_seq, request_id
                ),
            )
        || decisions.len() != event.items.len()
        || !event.items.iter().all(|item| {
            decisions
                .iter()
                .filter(|other| other.item_id == item.item_id)
                .count()
                == 1
                && decisions.contains(item)
        })
    {
        return Err(invalid());
    }
    Ok(Some(event.resulting_state))
}

impl SignalAgentSessionStore {
    /// Current caller/target authorization is the caller's responsibility. This
    /// reads an immutable receipt, not renewed authority or a new resume trigger.
    pub async fn replay_permission_decision(
        &self,
        run_id: &str,
        actor_id: &str,
        device_id: &str,
        request_id: &str,
        decisions: &[PermissionDecisionItem],
    ) -> Result<Option<PermissionRequestState>, AgentError> {
        let txn = self.db.begin().await.map_err(|_| invalid())?;
        let row = find(&txn, run_id)
            .await
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
        let session = session(&row, run_id, actor_id, device_id)?;
        let state = replay_on(&txn, &session, request_id, decisions).await?;
        txn.commit().await.map_err(|_| invalid())?;
        Ok(state)
    }
}
