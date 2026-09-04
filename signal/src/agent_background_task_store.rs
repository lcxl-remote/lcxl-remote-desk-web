//! SQLite projection for generic Provider executions that continue in the
//! background. The stable task/call/generation is created once; progress and
//! completion only advance that same row and append ordered run events.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use desk_agent_protocol::capability_provider::{
    CapabilityCancelRequest, CapabilityCompletionClass, CapabilityCompletionEvent,
    CapabilityProgressEvent,
};
use desk_agent_protocol::data_lineage::{ContentRef, DataEnvelope};
use desk_diagnose_core::dynamic_run::{
    AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind,
    BackgroundCancelDeliveredRunEvent, BackgroundCancelRequestedRunEvent,
    BackgroundCompletionRunEvent, BackgroundProgressRunEvent, BackgroundTaskRecord,
    BackgroundTaskState,
};
use desk_diagnose_core::session::{PersistedAgentSession, WorkKind};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder,
    Set, TransactionTrait,
};
use sha2::{Digest, Sha256};

use crate::agent_session_store::EventAppend;
use crate::entity::{agent_action_item, agent_run_event, agent_session};

pub const BACKGROUND_ACTION_KIND: &str = "capability_background";
pub const BACKGROUND_STATUS_RUNNING: &str = "background_running";
pub const BACKGROUND_STATUS_CANCEL_REQUESTED: &str = "background_cancel_requested";
pub const BACKGROUND_STATUS_SUCCEEDED: &str = "background_succeeded";
pub const BACKGROUND_STATUS_FAILED: &str = "background_failed";
pub const BACKGROUND_STATUS_CANCELLED: &str = "background_cancelled";
pub const BACKGROUND_STATUS_UNKNOWN: &str = "background_outcome_unknown";
pub const BACKGROUND_RESULT_SCHEMA_VERSION: u16 = 1;
pub const MAX_BACKGROUND_RESULT_TEXT_BYTES: usize = 64 * 1024;

const MAX_CAS_ATTEMPTS: usize = 8;

#[derive(Debug, Clone)]
pub struct BackgroundTaskCreate {
    pub record: BackgroundTaskRecord,
    pub actor_id: String,
    pub target_device_id: String,
    pub policy_revision: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundEventOutcome {
    Applied,
    Duplicate,
}

/// One bounded result body persisted before a completion becomes visible. The
/// envelope binds the exact UTF-8 bytes; terminal metadata without local bytes
/// remains supported through `output = None`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackgroundResultOutput {
    pub text: String,
    pub envelope: DataEnvelope,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BackgroundCompletionPayload {
    schema_version: u16,
    completion: CapabilityCompletionEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<BackgroundResultOutput>,
}

impl BackgroundCompletionPayload {
    fn validate(&self, record: &BackgroundTaskRecord) -> Result<(), String> {
        if self.schema_version != BACKGROUND_RESULT_SCHEMA_VERSION {
            return Err("unsupported background result schema".into());
        }
        self.completion
            .validate()
            .map_err(|error| error.to_string())?;
        if self.completion.task != record.task {
            return Err("background result task identity mismatch".into());
        }
        let Some(output) = &self.output else {
            return Ok(());
        };
        if output.text.len() > MAX_BACKGROUND_RESULT_TEXT_BYTES {
            return Err("background result text exceeds the durable bound".into());
        }
        output
            .envelope
            .validate()
            .map_err(|error| error.to_string())?;
        let digest = format!("{:x}", Sha256::digest(output.text.as_bytes()));
        if output.envelope.digest_sha256 != digest {
            return Err("background result bytes do not match their DataEnvelope".into());
        }
        let size_bytes = match &output.envelope.content {
            ContentRef::ImmutableBlob { size_bytes, .. }
            | ContentRef::EphemeralObservation { size_bytes, .. }
            | ContentRef::Artifact { size_bytes, .. } => *size_bytes,
        };
        if size_bytes != output.text.len() as u64 {
            return Err("background result size does not match its DataEnvelope".into());
        }
        if output.envelope.provenance.source_provider_id != record.task.provider_id
            || output.envelope.provenance.source_tool_name != record.tool_name
        {
            return Err("background result provenance does not match the Provider call".into());
        }
        if self.completion.result_envelope_ids != vec![output.envelope.envelope_id.clone()] {
            return Err("background completion does not bind the exact result envelope".into());
        }
        Ok(())
    }
}

/// Provider-side cancellation must be idempotent by the stable request id. A
/// transport error may mean that the Provider received the request but its ACK
/// was lost, so the durable publisher is required to resend the exact request.
#[async_trait]
pub trait BackgroundCancelDispatcher: Send + Sync {
    async fn deliver_cancel(&self, request: &CapabilityCancelRequest) -> Result<(), String>;
}

#[derive(Clone)]
pub struct SignalBackgroundTaskStore {
    db: DatabaseConnection,
}

impl SignalBackgroundTaskStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Persist the one stable task identity returned by Accepted. A duplicate
    /// task id is idempotent only when the entire server-owned record matches.
    pub async fn create(
        &self,
        create: BackgroundTaskCreate,
    ) -> Result<agent_action_item::Model, DbErr> {
        create
            .record
            .validate()
            .map_err(|error| DbErr::Custom(format!("invalid background task: {error}")))?;
        if let Some(existing) = self.row(&create.record.task.task_id).await? {
            let stored = decode_record(&existing)?;
            if stored == create.record
                && existing.actor_id == create.actor_id
                && existing.target_device_id == create.target_device_id
            {
                return Ok(existing);
            }
            return Err(DbErr::Custom(
                "background task id is already bound to another execution".into(),
            ));
        }
        let now = parse_time(&create.record.started_at);
        let payload_json = serde_json::to_string(&create.record)
            .map_err(|error| DbErr::Custom(format!("encode background task: {error}")))?;
        let task_id = create.record.task.task_id.clone();
        agent_action_item::ActiveModel {
            kind: Set(BACKGROUND_ACTION_KIND.into()),
            action_request_id: Set(task_id.clone()),
            exec_request_id: Set(None),
            conversation_id: Set(create.record.task.run_id.clone()),
            turn_id: Set(create.record.turn_id.clone()),
            tool_call_id: Set(create.record.task.call_id.clone()),
            actor_id: Set(create.actor_id),
            target_device_id: Set(create.target_device_id),
            status: Set(BACKGROUND_STATUS_RUNNING.into()),
            attempt: Set(0),
            execution_id: Set(Some(format!(
                "{}:{}",
                create.record.task.task_id, create.record.task.generation
            ))),
            draft_hash: Set(create.record.canonical_input_digest_sha256.clone()),
            policy_revision: Set(create.policy_revision),
            is_side_effecting: Set(create.record.effect.is_side_effecting()),
            payload_json: Set(payload_json),
            payload_schema_version: Set(i32::from(create.record.schema_version)),
            completion_event_id: Set(format!("background:{task_id}:terminal")),
            completion_delivery_state: Set("pending".into()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await
    }

    pub async fn load(&self, task_id: &str) -> Result<Option<BackgroundTaskRecord>, DbErr> {
        self.row(task_id)
            .await?
            .map(|row| decode_record(&row))
            .transpose()
    }

    pub async fn list_for_run(&self, run_id: &str) -> Result<Vec<BackgroundTaskRecord>, DbErr> {
        agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ConversationId.eq(run_id))
            .filter(agent_action_item::Column::Kind.eq(BACKGROUND_ACTION_KIND))
            .order_by_asc(agent_action_item::Column::Id)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| decode_record(&row))
            .collect()
    }

    pub async fn apply_progress(
        &self,
        progress: &CapabilityProgressEvent,
        updated_at: &str,
    ) -> Result<BackgroundEventOutcome, DbErr> {
        progress
            .validate()
            .map_err(|error| DbErr::Custom(format!("invalid background progress: {error}")))?;
        for _ in 0..MAX_CAS_ATTEMPTS {
            let txn = self.db.begin().await?;
            let row = load_task_row(&txn, &progress.task.task_id).await?;
            let Some(row) = row else {
                txn.rollback().await.ok();
                return Err(DbErr::Custom("background task was not found".into()));
            };
            let mut record = decode_record(&row)?;
            if progress.task != record.task {
                txn.rollback().await.ok();
                return Err(DbErr::Custom(
                    "background progress identity mismatch".into(),
                ));
            }
            if progress.sequence == record.progress_sequence {
                txn.rollback().await.ok();
                return Ok(BackgroundEventOutcome::Duplicate);
            }
            record
                .apply_progress(progress, updated_at.to_string())
                .map_err(|error| DbErr::Custom(format!("reject background progress: {error}")))?;
            let mut session = load_session(&txn, &record.task.run_id).await?;
            let old_session_version = session.version;
            let event_seq = next_event_seq(&mut session, updated_at)?;
            let update = BackgroundProgressRunEvent {
                event: AgentRunEvent {
                    schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                    event_id: format!(
                        "background:{}:progress:{}",
                        record.task.task_id, progress.sequence
                    ),
                    run_id: record.task.run_id.clone(),
                    event_seq,
                    input_revision: record.task.input_revision,
                    kind: AgentRunEventKind::BackgroundProgress,
                    correlation_id: Some(record.task.task_id.clone()),
                    source_envelope_ids: Vec::new(),
                    result_envelope_ids: Vec::new(),
                    created_at: updated_at.to_string(),
                },
                progress: progress.clone(),
            };
            update
                .validate()
                .map_err(|error| DbErr::Custom(format!("invalid progress event: {error}")))?;
            let changed = persist_transition(
                &txn,
                &row,
                &record,
                BACKGROUND_STATUS_RUNNING,
                progress.sequence,
                serde_json::to_string(progress).map_err(json_error)?,
                &mut session,
                old_session_version,
                &update.event,
                serde_json::to_string(&update).map_err(json_error)?,
                updated_at,
                None,
            )
            .await?;
            if changed {
                txn.commit().await?;
                return Ok(BackgroundEventOutcome::Applied);
            }
            txn.rollback().await.ok();
        }
        Err(DbErr::Custom("background progress CAS conflicted".into()))
    }

    pub async fn apply_completion(
        &self,
        completion: &CapabilityCompletionEvent,
        terminal_at: &str,
    ) -> Result<BackgroundEventOutcome, DbErr> {
        self.apply_completion_with_output(completion, None, terminal_at)
            .await
    }

    pub async fn apply_completion_with_output(
        &self,
        completion: &CapabilityCompletionEvent,
        output: Option<BackgroundResultOutput>,
        terminal_at: &str,
    ) -> Result<BackgroundEventOutcome, DbErr> {
        completion
            .validate()
            .map_err(|error| DbErr::Custom(format!("invalid background completion: {error}")))?;
        for _ in 0..MAX_CAS_ATTEMPTS {
            let txn = self.db.begin().await?;
            let row = load_task_row(&txn, &completion.task.task_id).await?;
            let Some(row) = row else {
                txn.rollback().await.ok();
                return Err(DbErr::Custom("background task was not found".into()));
            };
            let mut record = decode_record(&row)?;
            if completion.task != record.task {
                txn.rollback().await.ok();
                return Err(DbErr::Custom(
                    "background completion identity mismatch".into(),
                ));
            }
            let completion_payload = BackgroundCompletionPayload {
                schema_version: BACKGROUND_RESULT_SCHEMA_VERSION,
                completion: completion.clone(),
                output: output.clone(),
            };
            completion_payload
                .validate(&record)
                .map_err(|error| DbErr::Custom(format!("reject background result: {error}")))?;
            if record.state.is_terminal() {
                let same = record.progress_sequence == completion.sequence
                    && record.result_envelope_ids == completion.result_envelope_ids
                    && record.state
                        == desk_diagnose_core::dynamic_run::BackgroundTaskState::from_completion(
                            completion.completion,
                        )
                    && row
                        .result_json
                        .as_deref()
                        .and_then(|json| {
                            serde_json::from_str::<BackgroundCompletionPayload>(json).ok()
                        })
                        .as_ref()
                        == Some(&completion_payload);
                txn.rollback().await.ok();
                return if same {
                    Ok(BackgroundEventOutcome::Duplicate)
                } else {
                    Err(DbErr::Custom(
                        "conflicting terminal background completion".into(),
                    ))
                };
            }
            record
                .apply_completion(completion, terminal_at.to_string())
                .map_err(|error| DbErr::Custom(format!("reject background completion: {error}")))?;
            let mut session = load_session(&txn, &record.task.run_id).await?;
            let old_session_version = session.version;
            let event_seq = next_event_seq(&mut session, terminal_at)?;
            let update = BackgroundCompletionRunEvent {
                event: AgentRunEvent {
                    schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                    event_id: format!("background:{}:completion", record.task.task_id),
                    run_id: record.task.run_id.clone(),
                    event_seq,
                    input_revision: record.task.input_revision,
                    kind: AgentRunEventKind::BackgroundCompletion,
                    correlation_id: Some(record.task.task_id.clone()),
                    source_envelope_ids: Vec::new(),
                    result_envelope_ids: completion.result_envelope_ids.clone(),
                    created_at: terminal_at.to_string(),
                },
                completion: completion.clone(),
            };
            update
                .validate()
                .map_err(|error| DbErr::Custom(format!("invalid completion event: {error}")))?;
            let status = completion_status(completion.completion);
            let changed = persist_transition(
                &txn,
                &row,
                &record,
                status,
                completion.sequence,
                serde_json::to_string(&completion_payload).map_err(json_error)?,
                &mut session,
                old_session_version,
                &update.event,
                serde_json::to_string(&update).map_err(json_error)?,
                terminal_at,
                None,
            )
            .await?;
            if changed {
                txn.commit().await?;
                return Ok(BackgroundEventOutcome::Applied);
            }
            txn.rollback().await.ok();
        }
        Err(DbErr::Custom("background completion CAS conflicted".into()))
    }

    /// Persist an owner cancellation intent without accepting a browser-supplied
    /// task identity. The stable Provider task reference is recovered from the
    /// authorized row so callers cannot redirect cancellation to another run.
    pub async fn request_cancel_for_subject(
        &self,
        task_id: &str,
        run_id: &str,
        actor_id: &str,
        target_device_id: &str,
        request_id: &str,
        reason: &str,
        requested_at: &str,
    ) -> Result<BackgroundEventOutcome, DbErr> {
        let row = self
            .row(task_id)
            .await?
            .ok_or_else(|| DbErr::Custom("background task was not found".into()))?;
        if row.conversation_id != run_id
            || row.actor_id != actor_id
            || row.target_device_id != target_device_id
        {
            return Err(DbErr::Custom(
                "background task was not found or not accessible".into(),
            ));
        }
        let record = decode_record(&row)?;
        if !record.supports_cancel {
            return Err(DbErr::Custom(
                "background task does not support cancellation".into(),
            ));
        }
        self.request_cancel(
            &CapabilityCancelRequest {
                task: record.task,
                request_id: request_id.to_string(),
                requested_by_actor_id: actor_id.to_string(),
                reason: reason.to_string(),
            },
            requested_at,
        )
        .await
    }

    async fn request_cancel(
        &self,
        request: &CapabilityCancelRequest,
        requested_at: &str,
    ) -> Result<BackgroundEventOutcome, DbErr> {
        request
            .validate()
            .map_err(|error| DbErr::Custom(format!("invalid background cancel: {error}")))?;
        for _ in 0..MAX_CAS_ATTEMPTS {
            let txn = self.db.begin().await?;
            let row = load_task_row(&txn, &request.task.task_id).await?;
            let Some(row) = row else {
                txn.rollback().await.ok();
                return Err(DbErr::Custom("background task was not found".into()));
            };
            let mut record = decode_record(&row)?;
            if request.task != record.task {
                txn.rollback().await.ok();
                return Err(DbErr::Custom("background cancel identity mismatch".into()));
            }
            if record.state == BackgroundTaskState::CancelRequested
                && record.cancel_request_id.as_deref() == Some(request.request_id.as_str())
            {
                txn.rollback().await.ok();
                return Ok(BackgroundEventOutcome::Duplicate);
            }
            record
                .apply_cancel_request(request, requested_at.to_string())
                .map_err(|error| DbErr::Custom(format!("reject background cancel: {error}")))?;
            let mut session = load_session(&txn, &record.task.run_id).await?;
            let old_session_version = session.version;
            let event_seq = next_event_seq(&mut session, requested_at)?;
            let update = BackgroundCancelRequestedRunEvent {
                event: AgentRunEvent {
                    schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                    event_id: format!(
                        "background:{}:cancel:{}",
                        record.task.task_id, request.request_id
                    ),
                    run_id: record.task.run_id.clone(),
                    event_seq,
                    input_revision: record.task.input_revision,
                    kind: AgentRunEventKind::CancelRequested,
                    correlation_id: Some(record.task.task_id.clone()),
                    source_envelope_ids: Vec::new(),
                    result_envelope_ids: Vec::new(),
                    created_at: requested_at.to_string(),
                },
                request: request.clone(),
            };
            update
                .validate()
                .map_err(|error| DbErr::Custom(format!("invalid cancel event: {error}")))?;
            let progress_sequence = record.progress_sequence;
            let changed = persist_transition(
                &txn,
                &row,
                &record,
                BACKGROUND_STATUS_CANCEL_REQUESTED,
                progress_sequence,
                serde_json::to_string(request).map_err(json_error)?,
                &mut session,
                old_session_version,
                &update.event,
                serde_json::to_string(&update).map_err(json_error)?,
                requested_at,
                Some(&request.requested_by_actor_id),
            )
            .await?;
            if changed {
                txn.commit().await?;
                return Ok(BackgroundEventOutcome::Applied);
            }
            txn.rollback().await.ok();
        }
        Err(DbErr::Custom("background cancel CAS conflicted".into()))
    }

    /// Deliver every outstanding cancel intent through an idempotent Provider
    /// seam. A failed call is intentionally left pending: it may represent an
    /// ACK-loss window after the Provider accepted the request, so the next pass
    /// retries the exact same request id and bytes.
    pub async fn deliver_pending_cancellations_once(
        &self,
        dispatcher: &dyn BackgroundCancelDispatcher,
    ) -> Result<(), DbErr> {
        for request in self.pending_cancel_requests().await? {
            dispatcher.deliver_cancel(&request).await.map_err(|error| {
                DbErr::Custom(format!(
                    "deliver background cancel {}: {error}",
                    request.request_id
                ))
            })?;
            self.ack_cancel_delivery(&request, &Utc::now().to_rfc3339())
                .await?;
        }
        Ok(())
    }

    async fn pending_cancel_requests(&self) -> Result<Vec<CapabilityCancelRequest>, DbErr> {
        let rows = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::Kind.eq(BACKGROUND_ACTION_KIND))
            .filter(agent_action_item::Column::Status.eq(BACKGROUND_STATUS_CANCEL_REQUESTED))
            .order_by_asc(agent_action_item::Column::Id)
            .all(&self.db)
            .await?;
        let mut pending = Vec::new();
        for row in rows {
            let record = decode_record(&row)?;
            let Some(request_id) = record.cancel_request_id.as_deref() else {
                return Err(DbErr::Custom(
                    "cancel-requested background task has no request id".into(),
                ));
            };
            let request_event = agent_run_event::Entity::find()
                .filter(agent_run_event::Column::RunId.eq(&record.task.run_id))
                .filter(
                    agent_run_event::Column::Kind.eq(AgentRunEventKind::CancelRequested.as_str()),
                )
                .filter(agent_run_event::Column::CorrelationId.eq(&record.task.task_id))
                .order_by_desc(agent_run_event::Column::EventSeq)
                .one(&self.db)
                .await?
                .ok_or_else(|| {
                    DbErr::Custom("cancel-requested task has no durable request event".into())
                })?;
            let update: BackgroundCancelRequestedRunEvent =
                serde_json::from_str(&request_event.payload_json).map_err(json_error)?;
            update.validate().map_err(|error| {
                DbErr::Custom(format!("invalid durable cancel request: {error}"))
            })?;
            if update.request.request_id != request_id || update.request.task != record.task {
                return Err(DbErr::Custom(
                    "durable cancel request does not match background task".into(),
                ));
            }
            let delivered_event_id = cancel_delivered_event_id(&update.request);
            let delivered = agent_run_event::Entity::find()
                .filter(agent_run_event::Column::EventId.eq(delivered_event_id))
                .one(&self.db)
                .await?
                .is_some();
            if !delivered {
                pending.push(update.request);
            }
        }
        Ok(pending)
    }

    async fn ack_cancel_delivery(
        &self,
        request: &CapabilityCancelRequest,
        delivered_at: &str,
    ) -> Result<BackgroundEventOutcome, DbErr> {
        request
            .validate()
            .map_err(|error| DbErr::Custom(format!("invalid delivered cancel: {error}")))?;
        let event_id = cancel_delivered_event_id(request);
        for _ in 0..MAX_CAS_ATTEMPTS {
            let txn = self.db.begin().await?;
            if agent_run_event::Entity::find()
                .filter(agent_run_event::Column::EventId.eq(&event_id))
                .one(&txn)
                .await?
                .is_some()
            {
                txn.rollback().await.ok();
                return Ok(BackgroundEventOutcome::Duplicate);
            }
            let row = load_task_row(&txn, &request.task.task_id)
                .await?
                .ok_or_else(|| DbErr::Custom("background task was not found".into()))?;
            let record = decode_record(&row)?;
            if record.task != request.task
                || record.state != BackgroundTaskState::CancelRequested
                || record.cancel_request_id.as_deref() != Some(request.request_id.as_str())
            {
                txn.rollback().await.ok();
                return Err(DbErr::Custom(
                    "background cancel changed before delivery acknowledgement".into(),
                ));
            }
            let mut session = load_session(&txn, &record.task.run_id).await?;
            let old_version = session.version;
            let event_seq = next_event_seq(&mut session, delivered_at)?;
            let update = BackgroundCancelDeliveredRunEvent {
                event: AgentRunEvent {
                    schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                    event_id: event_id.clone(),
                    run_id: record.task.run_id.clone(),
                    event_seq,
                    input_revision: record.task.input_revision,
                    kind: AgentRunEventKind::CancelDelivered,
                    correlation_id: Some(record.task.task_id.clone()),
                    source_envelope_ids: Vec::new(),
                    result_envelope_ids: Vec::new(),
                    created_at: delivered_at.to_string(),
                },
                request: request.clone(),
            };
            update.validate().map_err(|error| {
                DbErr::Custom(format!("invalid cancel-delivered event: {error}"))
            })?;
            let new_version = old_version + 1;
            session.version = new_version;
            let state_json = session
                .encode_json_for_storage()
                .map_err(|error| DbErr::Custom(format!("encode cancel delivery: {error}")))?;
            let now = parse_time(delivered_at);
            let saved = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                .col_expr(agent_session::Column::Version, Expr::value(new_version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
                .filter(agent_session::Column::ConversationId.eq(&record.task.run_id))
                .filter(agent_session::Column::Version.eq(old_version))
                .exec(&txn)
                .await?;
            if saved.rows_affected != 1 {
                txn.rollback().await.ok();
                continue;
            }
            agent_run_event::ActiveModel {
                event_id: Set(update.event.event_id.clone()),
                run_id: Set(update.event.run_id.clone()),
                event_seq: Set(i64::try_from(update.event.event_seq).map_err(|_| {
                    DbErr::Custom("cancel delivery event sequence exceeds SQLite range".into())
                })?),
                input_revision: Set(i64::try_from(update.event.input_revision).map_err(|_| {
                    DbErr::Custom("cancel delivery revision exceeds SQLite range".into())
                })?),
                kind: Set(update.event.kind.as_str().into()),
                correlation_id: Set(update.event.correlation_id.clone()),
                input_seq: Set(None),
                actor_id: Set(None),
                source_envelope_ids_json: Set("[]".into()),
                result_envelope_ids_json: Set("[]".into()),
                payload_json: Set(serde_json::to_string(&update).map_err(json_error)?),
                payload_schema_version: Set(i32::from(update.event.schema_version)),
                created_at: Set(now),
                ..Default::default()
            }
            .insert(&txn)
            .await?;
            txn.commit().await?;
            return Ok(BackgroundEventOutcome::Applied);
        }
        Err(DbErr::Custom(
            "background cancel delivery CAS conflicted".into(),
        ))
    }

    async fn pending_deliveries(&self) -> Result<Vec<agent_action_item::Model>, DbErr> {
        agent_action_item::Entity::find()
            .filter(agent_action_item::Column::Kind.eq(BACKGROUND_ACTION_KIND))
            .filter(agent_action_item::Column::Status.is_in([
                BACKGROUND_STATUS_SUCCEEDED,
                BACKGROUND_STATUS_FAILED,
                BACKGROUND_STATUS_CANCELLED,
                BACKGROUND_STATUS_UNKNOWN,
            ]))
            .filter(agent_action_item::Column::CompletionDeliveryState.eq("pending"))
            .order_by_asc(agent_action_item::Column::Id)
            .all(&self.db)
            .await
    }

    async fn consume_delivery(&self, event_id: &str) -> Result<(), DbErr> {
        agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::CompletionDeliveryState,
                Expr::value("consumed"),
            )
            .filter(agent_action_item::Column::Kind.eq(BACKGROUND_ACTION_KIND))
            .filter(agent_action_item::Column::CompletionEventId.eq(event_id))
            .filter(agent_action_item::Column::CompletionDeliveryState.eq("pending"))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Deliver terminal Provider metadata to the durable conversation and run
    /// one bounded, tool-free completion turn. The envelope bytes themselves
    /// remain outside this projection; only server-owned ids/classification are
    /// exposed as untrusted output.
    async fn publish_once(&self) -> Result<(), DbErr> {
        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(self.db.clone());
        for row in self.pending_deliveries().await? {
            let record = decode_record(&row)?;
            let execution_id = row.execution_id.as_deref().ok_or_else(|| {
                DbErr::Custom("background delivery has no execution identity".into())
            })?;
            let now = Utc::now().to_rfc3339();
            let completion_payload: BackgroundCompletionPayload = serde_json::from_str(
                row.result_json
                    .as_deref()
                    .ok_or_else(|| DbErr::Custom("background completion has no result".into()))?,
            )
            .map_err(json_error)?;
            completion_payload.validate(&record).map_err(|error| {
                DbErr::Custom(format!("invalid stored background result: {error}"))
            })?;
            let metadata = serde_json::json!({
                "provider_id": record.task.provider_id,
                "capability_id": record.task.capability_id,
                "task_id": record.task.task_id,
                "state": record.state,
                "result_envelope_ids": record.result_envelope_ids,
            })
            .to_string();
            let (result_text, result_envelope) = match completion_payload.output {
                Some(output) => (output.text, Some(output.envelope)),
                None => (metadata, None),
            };
            let outcome = sessions
                .deliver_work_completion_with_envelope(
                    &row.conversation_id,
                    row.id,
                    WorkKind::CapabilityProvider,
                    &row.completion_event_id,
                    execution_id,
                    &row.tool_call_id,
                    &row.action_request_id,
                    &result_text,
                    result_envelope,
                    &now,
                )
                .await
                .map_err(agent_error)?;
            if matches!(outcome, EventAppend::Appended | EventAppend::AlreadyPresent)
                && self
                    .follow_up_completion(&sessions, &row, &now, WorkKind::CapabilityProvider)
                    .await?
            {
                self.consume_delivery(&row.completion_event_id).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn follow_up_completion(
        &self,
        sessions: &crate::agent_session_store::SignalAgentSessionStore,
        row: &agent_action_item::Model,
        now: &str,
        work_kind: WorkKind,
    ) -> Result<bool, DbErr> {
        const MAX_AUTO_FOLLOW_UP_TURNS: u32 = 3;
        let Some(session) = sessions
            .pending_auto_trigger(&row.conversation_id, &row.completion_event_id)
            .await
            .map_err(agent_error)?
        else {
            return Ok(true);
        };
        let Some(pending) = session
            .pending_auto_triggers
            .iter()
            .find(|pending| pending.event_id == row.completion_event_id)
        else {
            return Ok(true);
        };
        let expired = session
            .scope_snapshot
            .expires_at
            .as_ref()
            .is_some_and(|raw| {
                DateTime::parse_from_rfc3339(raw)
                    .map(|deadline| deadline < Utc::now())
                    .unwrap_or(true)
            });
        if pending.chain_id != session.chain_id
            || session.automation_turns_used >= MAX_AUTO_FOLLOW_UP_TURNS
            || expired
        {
            return Ok(!matches!(
                sessions
                    .prune_auto_trigger(&row.conversation_id, &row.completion_event_id, now)
                    .await
                    .map_err(agent_error)?,
                EventAppend::Busy
            ));
        }
        match crate::agent_runtime::resume_completion_turn(self.db.clone(), session, work_kind)
            .await
        {
            Ok(desk_diagnose_core::agent_loop::LoopOutcome::TurnBusy) => Ok(false),
            Ok(_) => {
                if sessions
                    .pending_auto_trigger(&row.conversation_id, &row.completion_event_id)
                    .await
                    .map_err(agent_error)?
                    .is_some()
                {
                    return Ok(!matches!(
                        sessions
                            .prune_auto_trigger(
                                &row.conversation_id,
                                &row.completion_event_id,
                                &Utc::now().to_rfc3339(),
                            )
                            .await
                            .map_err(agent_error)?,
                        EventAppend::Busy
                    ));
                }
                Ok(true)
            }
            Err(error) => {
                let exhausted = sessions
                    .pending_auto_trigger(&row.conversation_id, &row.completion_event_id)
                    .await
                    .map_err(agent_error)?
                    .is_some_and(|latest| latest.automation_turns_used >= MAX_AUTO_FOLLOW_UP_TURNS);
                if exhausted || error.kind == desk_agent_protocol::AgentErrorKind::PermissionDenied
                {
                    Ok(!matches!(
                        sessions
                            .prune_auto_trigger(
                                &row.conversation_id,
                                &row.completion_event_id,
                                &Utc::now().to_rfc3339(),
                            )
                            .await
                            .map_err(agent_error)?,
                        EventAppend::Busy
                    ))
                } else {
                    log::warn!(
                        "[capability-background] automatic completion follow-up failed task={}: {}",
                        row.action_request_id,
                        error.message
                    );
                    Ok(false)
                }
            }
        }
    }

    async fn row(&self, task_id: &str) -> Result<Option<agent_action_item::Model>, DbErr> {
        load_task_row(&self.db, task_id).await
    }
}

fn agent_error(error: desk_agent_protocol::AgentError) -> DbErr {
    DbErr::Custom(format!("background completion delivery: {}", error.message))
}

pub fn start_completion_publisher(db: DatabaseConnection) {
    actix_web::rt::spawn(async move {
        let store = SignalBackgroundTaskStore::new(db);
        loop {
            if crate::capability_grant_store::SignalCapabilityGrantStore::new(store.db.clone())
                .publish_computer_results_once()
                .await
                .is_err()
            {
                log::warn!("[computer-action] original completion scan failed");
            }
            if let Err(error) = store.publish_once().await {
                log::warn!("[capability-background] completion publisher failed: {error}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

fn cancel_delivered_event_id(request: &CapabilityCancelRequest) -> String {
    format!(
        "background-cancel-delivered-{:x}",
        Sha256::digest(
            format!(
                "{}:{}:{}:{}",
                request.task.task_id,
                request.task.generation,
                request.request_id,
                request.requested_by_actor_id
            )
            .as_bytes()
        )
    )
}

fn completion_status(completion: CapabilityCompletionClass) -> &'static str {
    match completion {
        CapabilityCompletionClass::Succeeded => BACKGROUND_STATUS_SUCCEEDED,
        CapabilityCompletionClass::Failed => BACKGROUND_STATUS_FAILED,
        CapabilityCompletionClass::Cancelled => BACKGROUND_STATUS_CANCELLED,
        CapabilityCompletionClass::OutcomeUnknown => BACKGROUND_STATUS_UNKNOWN,
    }
}

async fn load_task_row<C: sea_orm::ConnectionTrait>(
    db: &C,
    task_id: &str,
) -> Result<Option<agent_action_item::Model>, DbErr> {
    let row = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ActionRequestId.eq(task_id))
        .one(db)
        .await?;
    if let Some(row) = &row
        && row.kind != BACKGROUND_ACTION_KIND
    {
        return Err(DbErr::Custom(
            "task id belongs to a non-background action".into(),
        ));
    }
    Ok(row)
}

pub(crate) fn decode_record(row: &agent_action_item::Model) -> Result<BackgroundTaskRecord, DbErr> {
    serde_json::from_str(&row.payload_json)
        .map_err(|error| DbErr::Custom(format!("decode background task: {error}")))
}

async fn load_session<C: sea_orm::ConnectionTrait>(
    db: &C,
    run_id: &str,
) -> Result<PersistedAgentSession, DbErr> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(run_id))
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom("background task run was not found".into()))?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|error| DbErr::Custom(format!("decode background task run: {error}")))?;
    session.version = row.version;
    Ok(session)
}

fn next_event_seq(session: &mut PersistedAgentSession, updated_at: &str) -> Result<u64, DbErr> {
    session.last_event_seq = session
        .last_event_seq
        .checked_add(1)
        .ok_or_else(|| DbErr::Custom("background event sequence exhausted".into()))?;
    session.updated_at = updated_at.to_string();
    Ok(session.last_event_seq)
}

#[allow(clippy::too_many_arguments)]
async fn persist_transition<C: sea_orm::ConnectionTrait>(
    db: &C,
    old_row: &agent_action_item::Model,
    record: &BackgroundTaskRecord,
    status: &str,
    progress_sequence: u64,
    result_json: String,
    session: &mut PersistedAgentSession,
    old_session_version: i64,
    event: &AgentRunEvent,
    event_payload_json: String,
    updated_at: &str,
    actor_id: Option<&str>,
) -> Result<bool, DbErr> {
    let progress_sequence = i32::try_from(progress_sequence)
        .map_err(|_| DbErr::Custom("background progress exceeds SQLite range".into()))?;
    let payload_json = serde_json::to_string(record).map_err(json_error)?;
    let now = parse_time(updated_at);
    let action = agent_action_item::Entity::update_many()
        .col_expr(
            agent_action_item::Column::PayloadJson,
            Expr::value(payload_json),
        )
        .col_expr(agent_action_item::Column::Status, Expr::value(status))
        .col_expr(
            agent_action_item::Column::Attempt,
            Expr::value(progress_sequence),
        )
        .col_expr(
            agent_action_item::Column::ResultJson,
            Expr::value(Some(result_json)),
        )
        .col_expr(
            agent_action_item::Column::ResultSchemaVersion,
            Expr::value(Some(1)),
        )
        .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
        .filter(agent_action_item::Column::Id.eq(old_row.id))
        .filter(agent_action_item::Column::Status.eq(old_row.status.clone()))
        .filter(agent_action_item::Column::Attempt.eq(old_row.attempt))
        .exec(db)
        .await?;
    if action.rows_affected != 1 {
        return Ok(false);
    }

    session.version = old_session_version + 1;
    let state_json = session
        .encode_json_for_storage()
        .map_err(|error| DbErr::Custom(format!("encode background task run: {error}")))?;
    let run = agent_session::Entity::update_many()
        .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
        .col_expr(agent_session::Column::Version, Expr::value(session.version))
        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
        .filter(agent_session::Column::ConversationId.eq(&event.run_id))
        .filter(agent_session::Column::Version.eq(old_session_version))
        .exec(db)
        .await?;
    if run.rows_affected != 1 {
        return Ok(false);
    }

    agent_run_event::ActiveModel {
        event_id: Set(event.event_id.clone()),
        run_id: Set(event.run_id.clone()),
        event_seq: Set(i64::try_from(event.event_seq)
            .map_err(|_| DbErr::Custom("background event sequence exceeds SQLite range".into()))?),
        input_revision: Set(i64::try_from(event.input_revision)
            .map_err(|_| DbErr::Custom("background input revision exceeds SQLite range".into()))?),
        kind: Set(event.kind.as_str().into()),
        correlation_id: Set(event.correlation_id.clone()),
        input_seq: Set(None),
        actor_id: Set(actor_id.map(str::to_string)),
        source_envelope_ids_json: Set(
            serde_json::to_string(&event.source_envelope_ids).map_err(json_error)?
        ),
        result_envelope_ids_json: Set(
            serde_json::to_string(&event.result_envelope_ids).map_err(json_error)?
        ),
        payload_json: Set(event_payload_json),
        payload_schema_version: Set(i32::from(event.schema_version)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(true)
}

fn parse_time(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn json_error(error: serde_json::Error) -> DbErr {
    DbErr::Custom(format!("encode background event: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::capability_provider::{
        CapabilityEffect, CapabilityTaskRef, ExecutionPolicy,
    };
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, RetentionBoundary, Sensitivity,
    };
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::chat::ToolCall;
    use desk_diagnose_core::dynamic_run::{
        BACKGROUND_TASK_SCHEMA_VERSION, BackgroundTaskState, GrantRequestItem,
        PERMISSION_REQUEST_SCHEMA_VERSION, PermissionDecisionItem, PermissionItemDecision,
        PermissionRequest, PermissionRequestState,
    };
    use desk_diagnose_core::simulated_grant::{SimulatedCapabilityCall, SimulatedGrantAuthorizer};
    use sea_orm::{ConnectionTrait, Database, PaginatorTrait, QueryOrder, Schema};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct AckLossCancelDispatcher {
        calls: AtomicUsize,
        requests: Mutex<Vec<CapabilityCancelRequest>>,
    }

    #[async_trait]
    impl BackgroundCancelDispatcher for AckLossCancelDispatcher {
        async fn deliver_cancel(&self, request: &CapabilityCancelRequest) -> Result<(), String> {
            self.requests.lock().unwrap().push(request.clone());
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("simulated ACK loss after Provider accepted cancellation".into())
            } else {
                Ok(())
            }
        }
    }

    async fn create_file_db(path: &std::path::Path) -> DatabaseConnection {
        let db = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        db.execute_unprepared("PRAGMA journal_mode = WAL")
            .await
            .unwrap();
        db.execute_unprepared("PRAGMA synchronous = FULL")
            .await
            .unwrap();
        let schema = Schema::new(db.get_database_backend());
        for statement in [
            schema.create_table_from_entity(agent_action_item::Entity),
            schema.create_table_from_entity(agent_session::Entity),
            schema.create_table_from_entity(agent_run_event::Entity),
        ] {
            db.execute(&statement).await.unwrap();
        }
        let mut session = PersistedAgentSession::new(
            "run-1",
            "actor-1",
            "device-1",
            1,
            AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-08-26T00:00:00Z",
        );
        session.input_revision = 1;
        session.version = 1;
        agent_session::ActiveModel {
            conversation_id: Set(session.conversation_id.clone()),
            actor_id: Set(session.actor_id.clone()),
            device_id: Set(session.device_id.clone()),
            state_json: Set(session.encode_json_for_storage().unwrap()),
            version: Set(1),
            lease_token: Set(0),
            created_at: Set(parse_time("2026-08-26T00:00:00Z")),
            updated_at: Set(parse_time("2026-08-26T00:00:00Z")),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        db
    }

    fn record() -> BackgroundTaskRecord {
        BackgroundTaskRecord {
            schema_version: BACKGROUND_TASK_SCHEMA_VERSION,
            task: CapabilityTaskRef {
                task_id: "task-1".into(),
                call_id: "call-1".into(),
                run_id: "run-1".into(),
                provider_id: "file.workspace".into(),
                capability_id: "file.report.create".into(),
                input_revision: 1,
                generation: 1,
            },
            turn_id: "turn-1".into(),
            tool_name: "create_report".into(),
            canonical_input_digest_sha256:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            effect: CapabilityEffect::WriteArtifact,
            execution_policy: ExecutionPolicy::Adaptive {
                foreground_budget_ms: 5_000,
            },
            supports_cancel: true,
            state: BackgroundTaskState::Running,
            progress_sequence: 0,
            started_at: "2026-08-26T00:00:00Z".into(),
            updated_at: "2026-08-26T00:00:00Z".into(),
            terminal_at: None,
            cancel_request_id: None,
            result_envelope_ids: Vec::new(),
        }
    }

    #[tokio::test]
    async fn background_progress_and_completion_survive_wal_reopen_without_redispatch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("background-task.db");
        let db = create_file_db(&path).await;
        let store = SignalBackgroundTaskStore::new(db.clone());
        store
            .create(BackgroundTaskCreate {
                record: record(),
                actor_id: "actor-1".into(),
                target_device_id: "device-1".into(),
                policy_revision: 1,
            })
            .await
            .unwrap();
        let progress = CapabilityProgressEvent {
            task: record().task,
            sequence: 1,
            completed_units: Some(1),
            total_units: Some(2),
            message_key: Some("report.progress".into()),
        };
        assert_eq!(
            store
                .apply_progress(&progress, "2026-08-26T00:00:01Z")
                .await
                .unwrap(),
            BackgroundEventOutcome::Applied
        );
        assert_eq!(
            store
                .apply_progress(&progress, "2026-08-26T00:00:01Z")
                .await
                .unwrap(),
            BackgroundEventOutcome::Duplicate
        );
        drop(store);
        db.close().await.unwrap();

        let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let store = SignalBackgroundTaskStore::new(reopened.clone());
        let after_reopen = store.load("task-1").await.unwrap().unwrap();
        assert_eq!(after_reopen.progress_sequence, 1);
        assert_eq!(after_reopen.task.call_id, "call-1");
        assert_eq!(after_reopen.task.generation, 1);

        assert!(
            store
                .request_cancel_for_subject(
                    "task-1",
                    "run-1",
                    "other-actor",
                    "device-1",
                    "cancel-forged",
                    "owner requested stop",
                    "2026-08-26T00:00:01.250Z",
                )
                .await
                .is_err()
        );
        assert_eq!(
            store
                .request_cancel_for_subject(
                    "task-1",
                    "run-1",
                    "actor-1",
                    "device-1",
                    "cancel-1",
                    "owner requested stop",
                    "2026-08-26T00:00:01.500Z",
                )
                .await
                .unwrap(),
            BackgroundEventOutcome::Applied
        );
        assert_eq!(
            store
                .request_cancel_for_subject(
                    "task-1",
                    "run-1",
                    "actor-1",
                    "device-1",
                    "cancel-1",
                    "owner requested stop",
                    "2026-08-26T00:00:01.500Z",
                )
                .await
                .unwrap(),
            BackgroundEventOutcome::Duplicate
        );

        let completion = CapabilityCompletionEvent {
            task: after_reopen.task.clone(),
            sequence: 2,
            completion: CapabilityCompletionClass::Succeeded,
            result_envelope_ids: vec!["result-envelope-1".into()],
        };
        assert_eq!(
            store
                .apply_completion(&completion, "2026-08-26T00:00:02Z")
                .await
                .unwrap(),
            BackgroundEventOutcome::Applied
        );
        assert_eq!(
            store
                .apply_completion(&completion, "2026-08-26T00:00:02Z")
                .await
                .unwrap(),
            BackgroundEventOutcome::Duplicate
        );
        let terminal = store.load("task-1").await.unwrap().unwrap();
        assert_eq!(terminal.state, BackgroundTaskState::Succeeded);
        assert_eq!(terminal.result_envelope_ids, vec!["result-envelope-1"]);
        assert!(
            store
                .request_cancel_for_subject(
                    "task-1",
                    "run-1",
                    "actor-1",
                    "device-1",
                    "cancel-after-terminal",
                    "too late",
                    "2026-08-26T00:00:02.500Z",
                )
                .await
                .is_err(),
            "a terminal completion cannot be rewritten into cancel-requested"
        );
        let stale_cancel_dispatcher = AckLossCancelDispatcher::default();
        store
            .deliver_pending_cancellations_once(&stale_cancel_dispatcher)
            .await
            .unwrap();
        assert_eq!(
            stale_cancel_dispatcher.calls.load(Ordering::SeqCst),
            0,
            "terminal completion suppresses the older pending cancel delivery"
        );
        let action_row = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ActionRequestId.eq("task-1"))
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(reopened.clone());
        assert_eq!(
            sessions
                .deliver_work_completion(
                    "run-1",
                    action_row.id,
                    WorkKind::CapabilityProvider,
                    &action_row.completion_event_id,
                    action_row.execution_id.as_deref().unwrap(),
                    "call-1",
                    "task-1",
                    r#"{"state":"succeeded","result_envelope_ids":["result-envelope-1"]}"#,
                    "2026-08-26T00:00:03Z",
                )
                .await
                .unwrap(),
            EventAppend::Appended
        );
        let session_row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq("run-1"))
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        let delivered = PersistedAgentSession::decode_json(&session_row.state_json).unwrap();
        assert_eq!(
            delivered.pending_auto_triggers[0].kind,
            WorkKind::CapabilityProvider
        );
        assert_eq!(
            delivered.conversation.last().unwrap().role,
            desk_diagnose_core::chat::ChatRole::UntrustedOutput
        );
        drop(sessions);
        drop(store);
        reopened.close().await.unwrap();

        let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let store = SignalBackgroundTaskStore::new(reopened.clone());
        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(reopened.clone());
        assert!(
            sessions
                .pending_auto_trigger("run-1", &action_row.completion_event_id)
                .await
                .unwrap()
                .is_some(),
            "completion-triggered continuation survives a real SQLite reopen"
        );
        store
            .consume_delivery(&action_row.completion_event_id)
            .await
            .unwrap();
        let consumed = agent_action_item::Entity::find_by_id(action_row.id)
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(consumed.completion_delivery_state, "consumed");
        let events = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("run-1"))
            .order_by_asc(agent_run_event::Column::EventSeq)
            .all(&reopened)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            vec![
                "background_progress",
                "cancel_requested",
                "background_completion"
            ]
        );
        assert_eq!(events[0].event_seq, 1);
        assert_eq!(events[1].event_seq, 2);
        assert_eq!(events[2].event_seq, 3);
        assert_eq!(events[1].actor_id.as_deref(), Some("actor-1"));
        assert_eq!(
            agent_action_item::Entity::find()
                .filter(agent_action_item::Column::ActionRequestId.eq("task-1"))
                .count(&reopened)
                .await
                .unwrap(),
            1,
            "Adaptive foreground-to-background must keep one durable execution"
        );
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_delivery_replays_the_exact_request_after_ack_loss_without_claiming_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("background-cancel.db");
        let db = create_file_db(&path).await;
        let store = SignalBackgroundTaskStore::new(db.clone());
        store
            .create(BackgroundTaskCreate {
                record: record(),
                actor_id: "actor-1".into(),
                target_device_id: "device-1".into(),
                policy_revision: 1,
            })
            .await
            .unwrap();
        store
            .request_cancel_for_subject(
                "task-1",
                "run-1",
                "actor-1",
                "device-1",
                "cancel-stable-1",
                "owner requested stop",
                "2026-08-26T00:00:01Z",
            )
            .await
            .unwrap();

        let dispatcher = Arc::new(AckLossCancelDispatcher::default());
        assert!(
            store
                .deliver_pending_cancellations_once(dispatcher.as_ref())
                .await
                .is_err(),
            "an ACK-loss window must remain durably pending"
        );
        assert_eq!(
            store.load("task-1").await.unwrap().unwrap().state,
            BackgroundTaskState::CancelRequested,
            "delivery is not proof that execution stopped"
        );
        drop(store);
        db.close().await.unwrap();

        let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let store = SignalBackgroundTaskStore::new(reopened.clone());
        store
            .deliver_pending_cancellations_once(dispatcher.as_ref())
            .await
            .unwrap();
        store
            .deliver_pending_cancellations_once(dispatcher.as_ref())
            .await
            .unwrap();
        assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 2);
        {
            let requests = dispatcher.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0], requests[1],
                "retry bytes and identity stay exact"
            );
            assert_eq!(requests[1].request_id, "cancel-stable-1");
        }

        let events = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("run-1"))
            .order_by_asc(agent_run_event::Column::EventSeq)
            .all(&reopened)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["cancel_requested", "cancel_delivered"]
        );
        assert_eq!(
            store.load("task-1").await.unwrap().unwrap().state,
            BackgroundTaskState::CancelRequested
        );
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_provider_result_bytes_and_envelope_survive_reopen_and_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("background-result.db");
        let db = create_file_db(&path).await;
        let store = SignalBackgroundTaskStore::new(db.clone());
        store
            .create(BackgroundTaskCreate {
                record: record(),
                actor_id: "actor-1".into(),
                target_device_id: "device-1".into(),
                policy_revision: 1,
            })
            .await
            .unwrap();
        let text = "fake read-only Provider counted 3 workbooks".to_string();
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "result-envelope-1".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "result-blob-1".into(),
                sha256: digest.clone(),
                size_bytes: text.len() as u64,
                media_type: "text/plain".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "file.workspace".into(),
                source_tool_name: "create_report".into(),
                source_object_id: Some("fake-read-only-task".into()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest,
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        };
        let completion = CapabilityCompletionEvent {
            task: record().task,
            sequence: 1,
            completion: CapabilityCompletionClass::Succeeded,
            result_envelope_ids: vec![envelope.envelope_id.clone()],
        };
        store
            .apply_completion_with_output(
                &completion,
                Some(BackgroundResultOutput {
                    text: text.clone(),
                    envelope: envelope.clone(),
                }),
                "2026-08-26T00:00:02Z",
            )
            .await
            .unwrap();
        drop(store);
        db.close().await.unwrap();

        let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let row = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ActionRequestId.eq("task-1"))
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        let payload: BackgroundCompletionPayload =
            serde_json::from_str(row.result_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload.output.as_ref().unwrap().text, text);
        assert_eq!(payload.output.as_ref().unwrap().envelope, envelope);

        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(reopened.clone());
        let output = payload.output.unwrap();
        assert_eq!(
            sessions
                .deliver_work_completion_with_envelope(
                    "run-1",
                    row.id,
                    WorkKind::CapabilityProvider,
                    &row.completion_event_id,
                    row.execution_id.as_deref().unwrap(),
                    "call-1",
                    "task-1",
                    &output.text,
                    Some(output.envelope.clone()),
                    "2026-08-26T00:00:03Z",
                )
                .await
                .unwrap(),
            EventAppend::Appended
        );
        let session = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq("run-1"))
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        let session = PersistedAgentSession::decode_json(&session.state_json).unwrap();
        let delivered = session.conversation.last().unwrap();
        assert_eq!(delivered.text, text);
        assert_eq!(delivered.data_envelope.as_ref(), Some(&output.envelope));
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn stage2_fake_read_only_provider_accepts_once_then_resumes_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("selected-workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("north.xlsx"), b"fake workbook north").unwrap();
        std::fs::write(workspace.join("south.xlsx"), b"fake workbook south").unwrap();
        std::fs::write(workspace.join("notes.txt"), b"ignored").unwrap();
        let db_path = directory.path().join("fake-provider.db");
        let db = create_file_db(&db_path).await;
        let store = SignalBackgroundTaskStore::new(db.clone());

        let root_scope = format!("root:{}", workspace.display());
        let request = PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "permission-file-batch-1".into(),
            input_revision: 1,
            state: PermissionRequestState::Pending,
            items: vec![GrantRequestItem {
                item_id: "inspect-directory".into(),
                provider_id: "file.workspace".into(),
                tool_name: "inspect_workbook_directory".into(),
                expected_effect: CapabilityEffect::ReadDevice,
                resource_scope: vec![root_scope.clone()],
                operation_scope: vec!["enumerate_workbooks".into()],
                export_destinations: Vec::new(),
                canonical_input_json: None,
                canonical_input_digest_sha256: None,
                suggested_ttl_seconds: 120,
                suggested_max_uses: 1,
                reason: "inspect the selected workbook directory".into(),
            }],
            created_at: "2026-08-26T00:00:00Z".into(),
        };
        let decisions = vec![PermissionDecisionItem {
            item_id: "inspect-directory".into(),
            decision: PermissionItemDecision::Approve {
                resource_scope: vec![root_scope.clone()],
                operation_scope: vec!["enumerate_workbooks".into()],
                export_destinations: Vec::new(),
                ttl_seconds: 60,
                max_uses: 1,
            },
        }];
        let mut authorizer = SimulatedGrantAuthorizer::from_decision(&request, &decisions).unwrap();
        let resource_scope = vec![root_scope];
        let operation_scope = vec!["enumerate_workbooks".into()];
        let simulated_call = SimulatedCapabilityCall {
            provider_id: "file.workspace",
            tool_name: "inspect_workbook_directory",
            effect: CapabilityEffect::ReadDevice,
            resource_scope: &resource_scope,
            operation_scope: &operation_scope,
            export_destinations: &[],
        };
        assert!(authorizer.match_and_consume(&simulated_call));

        let model_call = ToolCall {
            id: "call-file-batch-1".into(),
            name: "inspect_workbook_directory".into(),
            arguments_json: serde_json::json!({ "directory": workspace }).to_string(),
        };
        let canonical_digest =
            format!("{:x}", Sha256::digest(model_call.arguments_json.as_bytes()));
        let task = CapabilityTaskRef {
            task_id: "task-file-batch-1".into(),
            call_id: model_call.id.clone(),
            run_id: "run-1".into(),
            provider_id: "file.workspace".into(),
            capability_id: "file.workbook.inspect_batch".into(),
            input_revision: 1,
            generation: 1,
        };
        store
            .create(BackgroundTaskCreate {
                record: BackgroundTaskRecord {
                    schema_version: BACKGROUND_TASK_SCHEMA_VERSION,
                    task: task.clone(),
                    turn_id: "turn-file-batch-1".into(),
                    tool_name: model_call.name.clone(),
                    canonical_input_digest_sha256: canonical_digest,
                    effect: CapabilityEffect::ReadDevice,
                    execution_policy: ExecutionPolicy::DurableRequired,
                    supports_cancel: true,
                    state: BackgroundTaskState::Running,
                    progress_sequence: 0,
                    started_at: "2026-08-26T00:00:01Z".into(),
                    updated_at: "2026-08-26T00:00:01Z".into(),
                    terminal_at: None,
                    cancel_request_id: None,
                    result_envelope_ids: Vec::new(),
                },
                actor_id: "actor-1".into(),
                target_device_id: "device-1".into(),
                policy_revision: 1,
            })
            .await
            .unwrap();
        let accepted =
            desk_agent_protocol::capability_provider::CapabilityInvocationOutcome::Accepted {
                task: task.clone(),
            };
        accepted.validate().unwrap();
        assert!(
            !authorizer.match_and_consume(&simulated_call),
            "returning Accepted must not mint or reserve a second simulated use"
        );

        // Crash immediately after Accepted: the Provider has not enumerated a
        // directory yet, but the exact task and canonical input are durable.
        drop(store);
        db.close().await.unwrap();
        let reopened = Database::connect(format!("sqlite://{}?mode=rw", db_path.display()))
            .await
            .unwrap();
        let store = SignalBackgroundTaskStore::new(reopened.clone());
        let resumed = store.load(&task.task_id).await.unwrap().unwrap();
        assert_eq!(resumed.task, task);
        assert_eq!(resumed.progress_sequence, 0);

        let arguments: serde_json::Value =
            serde_json::from_str(&model_call.arguments_json).unwrap();
        let selected_directory = std::path::PathBuf::from(arguments["directory"].as_str().unwrap());
        let workbook_count = std::fs::read_dir(selected_directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("xlsx")
            })
            .count();
        store
            .apply_progress(
                &CapabilityProgressEvent {
                    task: task.clone(),
                    sequence: 1,
                    completed_units: Some(workbook_count as u64),
                    total_units: Some(workbook_count as u64),
                    message_key: Some("workbooks.enumerated".into()),
                },
                "2026-08-26T00:00:02Z",
            )
            .await
            .unwrap();
        let text = serde_json::json!({ "workbook_count": workbook_count }).to_string();
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "result-file-batch-1".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "blob-file-batch-1".into(),
                sha256: digest.clone(),
                size_bytes: text.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: task.provider_id.clone(),
                source_tool_name: model_call.name.clone(),
                source_object_id: Some(task.task_id.clone()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest,
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        };
        store
            .apply_completion_with_output(
                &CapabilityCompletionEvent {
                    task: task.clone(),
                    sequence: 2,
                    completion: CapabilityCompletionClass::Succeeded,
                    result_envelope_ids: vec![envelope.envelope_id.clone()],
                },
                Some(BackgroundResultOutput { text, envelope }),
                "2026-08-26T00:00:03Z",
            )
            .await
            .unwrap();
        assert_eq!(workbook_count, 2);
        assert_eq!(
            store.load(&task.task_id).await.unwrap().unwrap().state,
            BackgroundTaskState::Succeeded
        );
        assert_eq!(
            agent_action_item::Entity::find()
                .filter(agent_action_item::Column::ActionRequestId.eq(&task.task_id))
                .count(&reopened)
                .await
                .unwrap(),
            1,
            "Accepted and restart recovery keep one execution row"
        );
        reopened.close().await.unwrap();
    }
}
