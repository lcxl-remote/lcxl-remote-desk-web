//! SQLite append-only event ledger for the OSS dynamic Device Assistant run.

mod input_context;
pub use desk_diagnose_core::input_read_context::ReadContextSelection;
pub use input_context::InputSubject;

use chrono::{DateTime, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope};
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::dynamic_run::{
    AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind, UserFollowupEvent,
};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set, TransactionTrait,
};

use crate::entity::{agent_run_event, agent_session};

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
        validate_append_params(&params)?;
        for _ in 0..APPEND_ATTEMPTS {
            let txn =
                self.db.begin().await.map_err(|error| {
                    internal(format!("begin user follow-up transaction: {error}"))
                })?;

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
            session.require_permission_revalidation(session.input_revision);
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
                message_envelope: envelope,
            };
            followup
                .validate()
                .map_err(|error| internal(format!("validate user follow-up event: {error}")))?;

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
    if params.surface != AgentSessionSurface::DeviceAssistant {
        return Err(internal("user follow-up ledger is Device Assistant only"));
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
                source_provider_id: "device-assistant-user".into(),
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
            surface: AgentSessionSurface::DeviceAssistant,
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
}
