//! Single-instance directory consent and immutable request receipts.

use super::*;
use desk_diagnose_core::file_scope::transaction::{
    self, FILE_SCOPE_EVENT_KIND, FileScopeMutation, FileScopeReceipt, FileScopeUpdate,
};

fn failure() -> AgentError {
    transport("Invalid or conflicting conversation directory authorization")
}

fn storage(_: sea_orm::DbErr) -> AgentError {
    AgentError {
        retryable: true,
        ..internal("Conversation directory storage is unavailable")
    }
}

impl SignalAgentSessionStore {
    pub async fn read_file_scope_receipt(
        &self,
        subject: &desk_diagnose_core::file_scope::FileScopeSubject,
        client_conversation_id: &str,
        client_request_id: &str,
    ) -> Result<Option<FileScopeReceipt>, AgentError> {
        if self.surface != AgentSessionSurface::DeviceAssistant
            || self.client_conversation_id.as_deref() != Some(client_conversation_id)
        {
            return Err(failure());
        }
        let txn = self.db.begin().await.map_err(storage)?;
        let stored = agent_run_event::Entity::find()
            .filter(
                agent_run_event::Column::EventId
                    .eq(transaction::receipt_event_id(subject, client_request_id)),
            )
            .one(&txn)
            .await
            .map_err(storage)?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        let receipt: FileScopeReceipt =
            serde_json::from_str(&stored.payload_json).map_err(|_| failure())?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&subject.conversation_id))
            .one(&txn)
            .await
            .map_err(storage)?
            .ok_or_else(failure)?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| failure())?;
        if &receipt.update.subject != subject
            || receipt.update.client_conversation_id != client_conversation_id
            || receipt.update.client_request_id != client_request_id
            || row.actor_id != subject.actor_id
            || row.device_id != subject.device_id
            || stored.kind != FILE_SCOPE_EVENT_KIND
            || stored.payload_schema_version != 1
            || stored.run_id != subject.conversation_id
            || stored.actor_id.as_deref() != Some(subject.actor_id.as_str())
            || stored.event_seq != receipt.event_seq as i64
            || stored.event_id != receipt.event_id
            || row.version != session.version
            || stored.correlation_id.as_deref() != Some(client_request_id)
        {
            return Err(failure());
        }
        transaction::replay(&session, &receipt.update, &receipt).map_err(|_| failure())?;
        txn.commit().await.map_err(storage)?;
        Ok(Some(receipt))
    }

    /// Caller authenticates the current device owner before entering this store.
    /// Scope changes and their original receipt share one SQLite transaction.
    pub async fn update_file_scope(
        &self,
        update: &FileScopeUpdate,
        now: DateTime<Utc>,
    ) -> Result<FileScopeReceipt, AgentError> {
        self.update_file_scope_guarded(update, now, None).await
    }

    pub(crate) async fn propose_file_scope_for_turn(
        &self,
        session: &mut PersistedAgentSession,
        proposal: desk_diagnose_core::file_scope::DirectoryProposal,
        now: DateTime<Utc>,
    ) -> Result<(), AgentError> {
        use desk_diagnose_core::file_scope::DirectoryConsentSource;
        if proposal.source != DirectoryConsentSource::ModelProposal
            || !session.turn_state.is_active()
        {
            return Err(failure());
        }
        let update = FileScopeUpdate {
            subject: session
                .file_scope_subject(
                    &session.actor_id,
                    &session.device_id,
                    &session.conversation_id,
                )
                .map_err(|_| failure())?,
            client_conversation_id: session.client_conversation_id.clone().ok_or_else(failure)?,
            client_request_id: proposal.request_id.clone(),
            expected_revision: session.file_scope.revision(),
            mutation: FileScopeMutation::Propose { proposal },
        };
        let (prepared, expected) = transaction::prepare(
            session,
            &update,
            u64::try_from(now.timestamp_millis()).map_err(|_| failure())?,
        )
        .map_err(|_| failure())?;
        let receipt = self
            .update_file_scope_guarded(&update, now, Some((session.version, session.lease_token)))
            .await?;
        if receipt != expected {
            return Err(failure());
        }
        *session = prepared;
        Ok(())
    }

    async fn update_file_scope_guarded(
        &self,
        update: &FileScopeUpdate,
        now: DateTime<Utc>,
        held: Option<(i64, u64)>,
    ) -> Result<FileScopeReceipt, AgentError> {
        update.validate().map_err(|_| failure())?;
        if self.surface != AgentSessionSurface::DeviceAssistant
            || self.client_conversation_id.as_deref()
                != Some(update.client_conversation_id.as_str())
        {
            return Err(failure());
        }
        let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| failure())?;
        let now =
            DateTime::<Utc>::from_timestamp_millis(now.timestamp_millis()).ok_or_else(failure)?;
        for _ in 0..CLAIM_ATTEMPTS {
            let txn = self.db.begin().await.map_err(storage)?;
            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&update.subject.conversation_id))
                .one(&txn)
                .await
                .map_err(storage)?;
            let mut session = if let Some(row) = &row {
                let session =
                    PersistedAgentSession::decode_json(&row.state_json).map_err(|_| failure())?;
                if row.actor_id != update.subject.actor_id
                    || row.device_id != update.subject.device_id
                    || row.version < 0
                    || session.version != row.version
                    || session.lease_token > i64::MAX as u64
                    || session.lease_token as i64 != row.lease_token
                {
                    return Err(failure());
                }
                session
            } else {
                if !matches!(
                    update.mutation,
                    FileScopeMutation::Propose { .. } | FileScopeMutation::Select { .. }
                ) {
                    return Err(failure());
                }
                let mut session = PersistedAgentSession::new(
                    &update.subject.conversation_id,
                    &update.subject.actor_id,
                    &update.subject.device_id,
                    0,
                    AgentScope {
                        granted: vec![],
                        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                        expires_at: None,
                        policy_name: None,
                    },
                    &now.to_rfc3339(),
                );
                session.adopt_client_metadata(
                    Some(&update.client_conversation_id),
                    AgentSessionSurface::DeviceAssistant,
                );
                session.version = -1;
                session
            };
            update.validate_session(&session).map_err(|_| failure())?;
            if held.is_some_and(|expected| expected != (session.version, session.lease_token)) {
                return Err(failure());
            }
            let existing = agent_run_event::Entity::find()
                .filter(agent_run_event::Column::EventId.eq(update.event_id()))
                .one(&txn)
                .await
                .map_err(storage)?;
            if let Some(existing) = existing {
                if row.is_none() {
                    return Err(failure());
                }
                let receipt: FileScopeReceipt =
                    serde_json::from_str(&existing.payload_json).map_err(|_| failure())?;
                transaction::replay(&session, update, &receipt).map_err(|_| failure())?;
                if existing.payload_schema_version != 1
                    || existing.kind != FILE_SCOPE_EVENT_KIND
                    || existing.run_id != update.subject.conversation_id
                    || existing.actor_id.as_deref() != Some(update.subject.actor_id.as_str())
                    || existing.correlation_id.as_deref() != Some(update.client_request_id.as_str())
                    || existing.event_seq != receipt.event_seq as i64
                    || existing.input_revision < 0
                    || existing.input_revision as u64 != receipt.event.input_revision
                    || existing.source_envelope_ids_json != "[]"
                    || existing.result_envelope_ids_json != "[]"
                    || existing.input_seq.is_some()
                    || DateTime::parse_from_rfc3339(&receipt.event.created_at)
                        .map(|time| time.with_timezone(&Utc))
                        .ok()
                        != Some(existing.created_at)
                {
                    return Err(failure());
                }
                txn.commit().await.map_err(storage)?;
                return Ok(receipt);
            }
            let (next, receipt) =
                transaction::prepare(&session, update, now_ms).map_err(|_| failure())?;
            session = next;
            let updated_at = row.as_ref().map_or(now, |row| row.updated_at.max(now));
            session.updated_at = updated_at.to_rfc3339();
            let state = session.encode_json_for_storage().map_err(|_| failure())?;
            if let Some(row) = row {
                let saved = agent_session::Entity::update_many()
                    .col_expr(agent_session::Column::StateJson, Expr::value(state))
                    .col_expr(agent_session::Column::Version, Expr::value(session.version))
                    .col_expr(agent_session::Column::UpdatedAt, Expr::value(updated_at))
                    .filter(agent_session::Column::Id.eq(row.id))
                    .filter(agent_session::Column::Version.eq(row.version))
                    .exec(&txn)
                    .await
                    .map_err(storage)?;
                if saved.rows_affected != 1 {
                    txn.rollback().await.map_err(storage)?;
                    continue;
                }
            } else {
                let inserted = agent_session::ActiveModel {
                    conversation_id: Set(update.subject.conversation_id.clone()),
                    actor_id: Set(update.subject.actor_id.clone()),
                    device_id: Set(update.subject.device_id.clone()),
                    state_json: Set(state),
                    version: Set(session.version),
                    lease_token: Set(0),
                    lease_deadline: Set(None),
                    created_at: Set(now),
                    updated_at: Set(now),
                    ..Default::default()
                }
                .insert(&txn)
                .await;
                if inserted.is_err() {
                    txn.rollback().await.map_err(storage)?;
                    continue;
                }
            }
            agent_run_event::ActiveModel {
                event_id: Set(receipt.event_id.clone()),
                run_id: Set(update.subject.conversation_id.clone()),
                event_seq: Set(receipt.event_seq as i64),
                input_revision: Set(i64::try_from(session.input_revision).map_err(|_| failure())?),
                kind: Set(FILE_SCOPE_EVENT_KIND.into()),
                correlation_id: Set(Some(update.client_request_id.clone())),
                input_seq: Set(None),
                actor_id: Set(Some(update.subject.actor_id.clone())),
                source_envelope_ids_json: Set("[]".into()),
                result_envelope_ids_json: Set("[]".into()),
                payload_json: Set(serde_json::to_string(&receipt).map_err(|_| failure())?),
                payload_schema_version: Set(1),
                created_at: Set(now),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .map_err(storage)?;
            txn.commit().await.map_err(storage)?;
            return Ok(receipt);
        }
        Err(AgentError {
            retryable: true,
            ..failure()
        })
    }
}

#[cfg(test)]
mod tests;
