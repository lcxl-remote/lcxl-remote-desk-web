//! Durable input selection and subject checks for the single-instance backend.

#[cfg(test)]
mod tests;

use super::*;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::{
    context_attachment::{ContextAttachment, MAX_CONTEXT_ATTACHMENTS},
    input_read_context::{validate_current_objects, validate_objects},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy)]
pub struct InputSubject<'a> {
    pub run_id: &'a str,
    pub actor_id: &'a str,
    pub device_id: &'a str,
    pub client_conversation_id: Option<&'a str>,
}

impl<'a> From<&'a AppendUserFollowupParams> for InputSubject<'a> {
    fn from(params: &'a AppendUserFollowupParams) -> Self {
        Self {
            run_id: &params.run_id,
            actor_id: &params.actor_id,
            device_id: &params.device_id,
            client_conversation_id: params.client_conversation_id.as_deref(),
        }
    }
}

pub(super) fn decode_session(
    row: &agent_session::Model,
    subject: InputSubject<'_>,
) -> Result<PersistedAgentSession, AgentError> {
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| internal("invalid input session"))?;
    if row.conversation_id != subject.run_id
        || row.actor_id != subject.actor_id
        || row.device_id != subject.device_id
        || session.conversation_id != subject.run_id
        || session.actor_id != subject.actor_id
        || session.device_id != subject.device_id
        || session.client_conversation_id.as_deref() != subject.client_conversation_id
        || session.surface != AgentSessionSurface::DeviceAssistant
        || row.version < 0
        || session.handled_input_seq > session.latest_input_seq
    {
        return Err(internal("input session subject mismatch"));
    }
    // Existing SQLite sessions use the row version as the CAS authority.
    session.version = row.version;
    Ok(session)
}

#[derive(Serialize, Deserialize)]
struct StoredInput {
    #[serde(flatten)]
    event: UserFollowupEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_context: Option<ReadContextSelection>,
}

pub(super) fn payload_version(selection: Option<&ReadContextSelection>) -> i32 {
    match selection {
        Some(selection) if !selection.object_attachments.is_empty() => 3,
        Some(_) => 2,
        None => i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION),
    }
}

pub(super) fn encode_event(
    event: &UserFollowupEvent,
    selection: Option<&ReadContextSelection>,
) -> Result<String, AgentError> {
    serde_json::to_string(&StoredInput {
        event: event.clone(),
        read_context: selection.cloned(),
    })
    .map_err(|_| internal("invalid input event"))
}

pub(super) fn decode_event(
    row: &agent_run_event::Model,
) -> Result<(UserFollowupEvent, Option<ReadContextSelection>), AgentError> {
    let stored: StoredInput =
        serde_json::from_str(&row.payload_json).map_err(|_| internal("invalid stored input"))?;
    let followup = &stored.event;
    let event = &followup.event;
    followup
        .validate()
        .map_err(|_| internal("invalid input event contract"))?;
    if row.payload_schema_version != payload_version(stored.read_context.as_ref())
        || row.kind != AgentRunEventKind::UserFollowup.as_str()
        || row.kind != event.kind.as_str()
        || row.event_id != event.event_id
        || row.run_id != event.run_id
        || row.event_seq <= 0
        || row.event_seq as u64 != event.event_seq
        || row.input_revision <= 0
        || row.input_revision as u64 != event.input_revision
        || row.input_seq != i64::try_from(followup.input_seq).ok()
        || followup.input_seq == 0
        || row.actor_id.as_deref() != Some(followup.actor_id.as_str())
        || row.correlation_id.as_deref() != Some(followup.message_id.as_str())
        || row.correlation_id != event.correlation_id
        || serde_json::from_str::<Vec<String>>(&row.source_envelope_ids_json)
            .map_err(|_| internal("invalid input sources"))?
            != event.source_envelope_ids
        || serde_json::from_str::<Vec<String>>(&row.result_envelope_ids_json)
            .map_err(|_| internal("invalid input results"))?
            != event.result_envelope_ids
        || parse_time(&event.created_at)? != row.created_at
    {
        return Err(internal("input event columns or schema mismatch"));
    }
    if let Some(selection) = &stored.read_context {
        selection.validate()?;
    }
    Ok((stored.event, stored.read_context))
}

pub(super) fn validate_replay(
    row: &agent_run_event::Model,
    params: &AppendUserFollowupParams,
    session: &PersistedAgentSession,
) -> Result<(), AgentError> {
    let (event, selection) = decode_event(row)?;
    let mut expected = params.message.clone();
    expected.turn_id = None;
    let mut messages = session
        .conversation
        .iter()
        .filter(|message| message.message_id == expected.message_id);
    let mut stored = messages
        .next()
        .ok_or_else(|| internal("original input message missing"))?
        .clone();
    if messages.next().is_some() {
        return Err(internal("ambiguous original message"));
    }
    stored.turn_id = None;
    if row.event_id != params.event_id
        || event.event.run_id != params.run_id
        || event.actor_id != params.actor_id
        || session.device_id != params.device_id
        || session.actor_id != params.actor_id
        || session.conversation_id != params.run_id
        || session.surface != params.surface
        || session.client_conversation_id != params.client_conversation_id
        || event.message_id != params.message.message_id
        || params.message.data_envelope.as_ref() != Some(&event.message_envelope)
        || stored != expected
        || selection != params.read_context
        || event.input_seq > session.latest_input_seq
        || event.event.input_revision > session.input_revision
        || event.event.event_seq > session.last_event_seq
    {
        return Err(internal(
            "input receipt conflicts with the accepted message or context",
        ));
    }
    Ok(())
}

pub(super) fn validate_selection(
    session: &PersistedAgentSession,
    selection: &ReadContextSelection,
    message: &ChatMessage,
    now: DateTime<Utc>,
) -> Result<(), AgentError> {
    selection.validate()?;
    if selection
        .expires_at
        .as_ref()
        .is_some_and(|expiry| parse_time(expiry).map_or(true, |expiry| expiry <= now))
    {
        return Err(internal("original read context expired"));
    }
    if selection.object_attachments.is_empty() {
        return Ok(());
    }
    let destinations = &message
        .data_envelope
        .as_ref()
        .ok_or_else(|| internal("input envelope missing"))?
        .allowed_destinations;
    let [destination @ DestinationIdentity::Model { .. }] = destinations.as_slice() else {
        return Err(internal("object input has no exact model destination"));
    };
    validate_current_objects(
        session,
        &selection.object_attachments,
        destination,
        u64::try_from(now.timestamp_millis()).map_err(|_| internal("invalid input time"))?,
    )
}

impl SignalAgentRunEventStore {
    pub async fn select_objects(
        &self,
        subject: InputSubject<'_>,
        event_id: &str,
        ids: &[String],
        now: u64,
    ) -> Result<Vec<ContextAttachment>, AgentError> {
        let canonical: std::collections::BTreeSet<_> = ids.iter().collect();
        if ids.len() > MAX_CONTEXT_ATTACHMENTS
            || ids.len() != canonical.len()
            || ids.iter().any(|id| id.trim().is_empty() || id.len() > 512)
        {
            return Err(internal("invalid selected attachment IDs"));
        }
        let txn = self
            .db
            .begin()
            .await
            .map_err(|_| internal("input selection storage unavailable"))?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(subject.run_id))
            .one(&txn)
            .await
            .map_err(|_| internal("input session storage unavailable"))?;
        let Some(row) = row else {
            if ids.is_empty() {
                return Ok(vec![]);
            }
            return Err(internal("selected attachment session missing"));
        };
        let session = decode_session(&row, subject)?;
        let previous = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::EventId.eq(event_id))
            .one(&txn)
            .await
            .map_err(|_| internal("input event storage unavailable"))?;
        let objects = if let Some(row) = previous {
            let (event, selection) = decode_event(&row)?;
            if event.event.run_id != subject.run_id || event.actor_id != subject.actor_id {
                return Err(internal("input selection subject mismatch"));
            }
            let objects = selection
                .map(|selection| selection.object_attachments)
                .unwrap_or_default();
            if objects
                .iter()
                .map(|object| &object.attachment_id)
                .collect::<std::collections::BTreeSet<_>>()
                != canonical
            {
                return Err(internal("retry changed the original object selection"));
            }
            objects
        } else {
            canonical
                .into_iter()
                .map(|id| {
                    session
                        .context_attachments
                        .iter()
                        .find(|object| &object.attachment_id == id && object.is_active_at(now))
                        .cloned()
                        .ok_or_else(|| internal("selected attachment is unavailable"))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        validate_objects(&objects)?;
        txn.commit()
            .await
            .map_err(|_| internal("input selection storage unavailable"))?;
        Ok(objects)
    }

    /// Only the current original input may resume; a later follow-up fences it.
    pub async fn original_read_context(
        &self,
        subject: InputSubject<'_>,
        input_revision: u64,
    ) -> Result<Option<ReadContextSelection>, AgentError> {
        let txn = self
            .db
            .begin()
            .await
            .map_err(|_| internal("input context storage unavailable"))?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(subject.run_id))
            .one(&txn)
            .await
            .map_err(|_| internal("input context storage unavailable"))?
            .ok_or_else(|| internal("input session unavailable"))?;
        let session = decode_session(&row, subject)?;
        if session.input_revision != input_revision {
            return Err(internal("original input revision changed"));
        }
        let selection = original_on(&txn, &session).await?;
        txn.commit()
            .await
            .map_err(|_| internal("input context storage unavailable"))?;
        Ok(selection)
    }
}

async fn original_on(
    txn: &sea_orm::DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<Option<ReadContextSelection>, AgentError> {
    let rows = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::RunId.eq(&session.conversation_id))
        .filter(
            agent_run_event::Column::InputRevision
                .eq(to_i64("input_revision", session.input_revision)?),
        )
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::UserFollowup.as_str()))
        .limit(2)
        .all(txn)
        .await
        .map_err(|_| internal("input context storage unavailable"))?;
    let [row] = rows.as_slice() else {
        return Err(internal("original input missing or ambiguous"));
    };
    let (event, selection) = decode_event(row)?;
    let message = session
        .conversation
        .iter()
        .find(|message| message.message_id == event.message_id)
        .cloned()
        .ok_or_else(|| internal("original message missing"))?;
    let subject = InputSubject {
        run_id: &session.conversation_id,
        actor_id: &session.actor_id,
        device_id: &session.device_id,
        client_conversation_id: session.client_conversation_id.as_deref(),
    };
    let params = AppendUserFollowupParams {
        event_id: row.event_id.clone(),
        run_id: subject.run_id.into(),
        client_conversation_id: subject.client_conversation_id.map(str::to_string),
        actor_id: subject.actor_id.into(),
        device_id: subject.device_id.into(),
        surface: AgentSessionSurface::DeviceAssistant,
        policy_revision: session.policy_revision,
        current_scope: session.scope_snapshot.clone(),
        read_context: selection.clone(),
        message,
        created_at: event.event.created_at,
    };
    validate_replay(row, &params, session)?;

    Ok(selection)
}

impl SignalAgentRunEventStore {
    /// Recheck the durable input and current attachment state around device I/O.
    pub async fn validate_object_read(
        &self,
        subject: InputSubject<'_>,
        input_revision: u64,
        original: &ReadContextSelection,
        destination: &DestinationIdentity,
        now: u64,
    ) -> Result<(), AgentError> {
        let txn = self
            .db
            .begin()
            .await
            .map_err(|_| internal("object read storage unavailable"))?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(subject.run_id))
            .one(&txn)
            .await
            .map_err(|_| internal("object read storage unavailable"))?
            .ok_or_else(|| internal("object read session missing"))?;
        let session = decode_session(&row, subject)?;
        if session.input_revision != input_revision
            || original_on(&txn, &session).await?.as_ref() != Some(original)
            || original.expires_at.as_ref().is_some_and(|expiry| {
                parse_time(expiry)
                    .ok()
                    .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                    .is_none_or(|expiry| expiry <= now)
            })
        {
            return Err(internal("original object input changed or expired"));
        }
        validate_current_objects(&session, &original.object_attachments, destination, now)?;
        txn.commit()
            .await
            .map_err(|_| internal("object read storage unavailable"))
    }
}
