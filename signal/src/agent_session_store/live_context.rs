//! Single-instance SQLite live metadata and immutable first-selection receipts.

use super::*;
use desk_agent_protocol::{ExecutionMode, device_assistant::DeviceAssistantContextUpdate};
use desk_diagnose_core::{
    dynamic_run::{AgentRunEvent, AgentRunEventKind},
    live_context::{ContextSelectionClaim, reconcile_live_context, validate_durable_update},
};
use serde::{Deserialize, Serialize};

pub struct UpdateLiveContext {
    pub run_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub update: DeviceAssistantContextUpdate,
    pub selection: Option<ContextSelectionClaim>,
    pub created_at: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    event: AgentRunEvent,
    actor_id: String,
    device_id: String,
    update: DeviceAssistantContextUpdate,
    changed: bool,
}

fn error() -> AgentError {
    internal("Device Assistant live context receipt or subject is inconsistent")
}

fn storage_error(_: sea_orm::DbErr) -> AgentError {
    AgentError {
        retryable: true,
        ..internal("Device Assistant live context storage is unavailable; retry the same request")
    }
}

fn event_id(params: &UpdateLiveContext) -> String {
    let mut hash = Sha256::new();
    for value in [&params.run_id, &params.update.client_request_id] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("live-context-{:x}", hash.finalize())
}

fn check_missing_receipt(
    session: &PersistedAgentSession,
    params: &UpdateLiveContext,
) -> Result<(), AgentError> {
    // Retained metadata proves that this request already existed, but cannot
    // reconstruct its first result. Never manufacture history from current state.
    if session.context_attachments.iter().any(|attachment| {
        params
            .update
            .selected_capability_ids
            .iter()
            .any(|capability| {
                attachment.client_request_id
                    == desk_diagnose_core::live_context::selection_request_id(
                        &params.update.client_request_id,
                        capability,
                    )
            })
    }) {
        return Err(internal(
            "Original live context receipt is unavailable; refresh the conversation and use a new request ID",
        ));
    }
    Ok(())
}

impl SignalAgentSessionStore {
    fn validate_live_request(&self, params: &UpdateLiveContext) -> Result<(), AgentError> {
        validate_durable_update(&params.update)?;
        if params.run_id.trim().is_empty()
            || params.run_id.len() > 128
            || params.actor_id.trim().is_empty()
            || params.device_id.trim().is_empty()
            || self.surface != AgentSessionSurface::DeviceAssistant
            || self.client_conversation_id.as_deref()
                != Some(params.update.conversation_id.as_str())
        {
            return Err(error());
        }
        Ok(())
    }

    fn live_session(
        &self,
        row: &agent_session::Model,
        params: &UpdateLiveContext,
    ) -> Result<PersistedAgentSession, AgentError> {
        let mut session =
            PersistedAgentSession::decode_json(&row.state_json).map_err(|_| error())?;
        if row.conversation_id != params.run_id
            || row.actor_id != params.actor_id
            || row.device_id != params.device_id
            || session.conversation_id != params.run_id
            || row.version < 0
            || session.client_conversation_id.as_deref()
                != Some(params.update.conversation_id.as_str())
        {
            return Err(error());
        }
        session
            .check_subject(&params.actor_id, &params.device_id)
            .map_err(|_| error())?;
        session.check_surface(self.surface).map_err(|_| error())?;
        // The SQLite row is the existing session store's CAS version authority.
        session.version = row.version;
        Ok(session)
    }

    /// Probe original history before resolving the current model. The write
    /// transaction repeats this lookup, so a concurrent first request still wins.
    pub async fn replay_live_context(
        &self,
        params: &UpdateLiveContext,
    ) -> Result<Option<bool>, AgentError> {
        self.validate_live_request(params)?;
        let txn = self.db.begin().await.map_err(storage_error)?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&params.run_id))
            .one(&txn)
            .await
            .map_err(storage_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session = self.live_session(&row, params)?;
        let receipt = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::EventId.eq(event_id(params)))
            .one(&txn)
            .await
            .map_err(storage_error)?;
        let result = receipt
            .map(|row| replay(&row, params, &session))
            .transpose()?;
        if result.is_none() {
            check_missing_receipt(&session, params)?;
        }
        txn.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn update_live_context(
        &self,
        params: &UpdateLiveContext,
    ) -> Result<bool, AgentError> {
        self.validate_live_request(params)?;
        let time = DateTime::parse_from_rfc3339(&params.created_at)
            .map_err(|_| error())?
            .with_timezone(&Utc);
        let now_ms = u64::try_from(time.timestamp_millis()).map_err(|_| error())?;
        let id = event_id(params);
        for _ in 0..CLAIM_ATTEMPTS {
            let txn = self.db.begin().await.map_err(storage_error)?;
            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&params.run_id))
                .one(&txn)
                .await
                .map_err(storage_error)?;
            let mut session = match row.as_ref() {
                Some(row) => self.live_session(row, params)?,
                None => {
                    let mut session = PersistedAgentSession::new(
                        &params.run_id,
                        &params.actor_id,
                        &params.device_id,
                        0,
                        AgentScope {
                            granted: vec![],
                            mode: ExecutionMode::ReadOnly,
                            expires_at: None,
                            policy_name: None,
                        },
                        &params.created_at,
                    );
                    session
                        .adopt_client_metadata(Some(&params.update.conversation_id), self.surface);
                    session
                }
            };
            if let Some(receipt) = agent_run_event::Entity::find()
                .filter(agent_run_event::Column::EventId.eq(&id))
                .one(&txn)
                .await
                .map_err(storage_error)?
            {
                let changed = replay(&receipt, params, &session)?;
                txn.commit().await.map_err(storage_error)?;
                return Ok(changed);
            }
            check_missing_receipt(&session, params)?;
            if session.turn_state.is_active() {
                return Err(AgentError {
                    retryable: true,
                    ..transport("Device Assistant context is busy")
                });
            }
            let selection = params.selection.as_ref().ok_or_else(error)?;
            if selection.selected_capability_ids != params.update.selected_capability_ids
                || selection.now_unix_ms > now_ms
                || now_ms.saturating_sub(selection.now_unix_ms) > 30_000
            {
                return Err(error());
            }
            let mut current = selection.clone();
            current.now_unix_ms = now_ms;
            let changed = reconcile_live_context(&mut session, &current)?;
            session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(error)?;
            session.version = match row.as_ref() {
                Some(row) => row.version.checked_add(1).ok_or_else(error)?,
                None => 0,
            };
            let updated_at = row.as_ref().map_or(time, |row| row.updated_at.max(time));
            session.updated_at = updated_at.to_rfc3339();
            let receipt = Receipt {
                event: AgentRunEvent {
                    schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                    event_id: id.clone(),
                    run_id: params.run_id.clone(),
                    event_seq: session.last_event_seq,
                    input_revision: session.input_revision,
                    kind: AgentRunEventKind::LiveContextUpdated,
                    correlation_id: Some(params.update.client_request_id.clone()),
                    source_envelope_ids: vec![],
                    result_envelope_ids: vec![],
                    created_at: params.created_at.clone(),
                },
                actor_id: params.actor_id.clone(),
                device_id: params.device_id.clone(),
                update: params.update.clone(),
                changed,
            };
            receipt.event.validate().map_err(|_| error())?;
            let state = session.encode_json_for_storage().map_err(|_| error())?;
            if let Some(row) = row {
                let result = agent_session::Entity::update_many()
                    .col_expr(agent_session::Column::StateJson, Expr::value(state))
                    .col_expr(agent_session::Column::Version, Expr::value(session.version))
                    .col_expr(agent_session::Column::UpdatedAt, Expr::value(updated_at))
                    .filter(agent_session::Column::Id.eq(row.id))
                    .filter(agent_session::Column::Version.eq(row.version))
                    .exec(&txn)
                    .await
                    .map_err(storage_error)?;
                if result.rows_affected != 1 {
                    txn.rollback().await.map_err(storage_error)?;
                    continue;
                }
            } else {
                let inserted = agent_session::ActiveModel {
                    conversation_id: Set(params.run_id.clone()),
                    actor_id: Set(params.actor_id.clone()),
                    device_id: Set(params.device_id.clone()),
                    state_json: Set(state),
                    version: Set(0),
                    lease_token: Set(0),
                    lease_deadline: Set(None),
                    created_at: Set(time),
                    updated_at: Set(updated_at),
                    ..Default::default()
                }
                .insert(&txn)
                .await;
                if inserted.is_err() {
                    txn.rollback().await.map_err(storage_error)?;
                    continue;
                }
            }
            agent_run_event::ActiveModel {
                event_id: Set(id.clone()),
                run_id: Set(params.run_id.clone()),
                event_seq: Set(i64::try_from(session.last_event_seq).map_err(|_| error())?),
                input_revision: Set(i64::try_from(session.input_revision).map_err(|_| error())?),
                kind: Set("live_context_updated".into()),
                correlation_id: Set(Some(params.update.client_request_id.clone())),
                input_seq: Set(None),
                actor_id: Set(Some(params.actor_id.clone())),
                source_envelope_ids_json: Set("[]".into()),
                result_envelope_ids_json: Set("[]".into()),
                payload_json: Set(serde_json::to_string(&receipt).map_err(|_| error())?),
                payload_schema_version: Set(1),
                created_at: Set(time),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .map_err(storage_error)?;
            txn.commit().await.map_err(storage_error)?;
            return Ok(changed);
        }
        Err(transport("Device Assistant live context update conflicted"))
    }
}

fn replay(
    row: &agent_run_event::Model,
    params: &UpdateLiveContext,
    session: &PersistedAgentSession,
) -> Result<bool, AgentError> {
    let receipt: Receipt = serde_json::from_str(&row.payload_json).map_err(|_| error())?;
    receipt.event.validate().map_err(|_| error())?;
    let event = &receipt.event;
    if row.payload_schema_version != 1
        || row.event_id != event_id(params)
        || row.event_id != event.event_id
        || row.run_id != params.run_id
        || row.run_id != event.run_id
        || row.kind != "live_context_updated"
        || event.kind != AgentRunEventKind::LiveContextUpdated
        || row.event_seq <= 0
        || row.event_seq as u64 != event.event_seq
        || event.event_seq > session.last_event_seq
        || row.input_revision < 0
        || row.input_revision as u64 != event.input_revision
        || event.input_revision > session.input_revision
        || row.correlation_id.as_deref() != Some(params.update.client_request_id.as_str())
        || event.correlation_id != row.correlation_id
        || row.actor_id.as_deref() != Some(params.actor_id.as_str())
        || receipt.actor_id != params.actor_id
        || receipt.device_id != params.device_id
        || receipt.update != params.update
        || row.input_seq.is_some()
        || row.source_envelope_ids_json != "[]"
        || row.result_envelope_ids_json != "[]"
        || !event.source_envelope_ids.is_empty()
        || !event.result_envelope_ids.is_empty()
        || DateTime::parse_from_rfc3339(&event.created_at)
            .map_err(|_| error())?
            .with_timezone(&Utc)
            != row.created_at
    {
        return Err(error());
    }
    Ok(receipt.changed)
}

#[cfg(test)]
mod tests;
