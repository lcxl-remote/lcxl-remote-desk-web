//! Durable intake for a main assistant request. The host owns the conversation lock.

use desk_agent_protocol::{
    ai_assistant::{AiAssistantAsk, MainRequestFailure, MainRequestReceipt, MainRequestState},
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    action_turn_fence::AssistantTurnFence,
    chat::{ChatMessage, ChatRole},
    dynamic_run::AssistantInputTarget,
    input_read_context::ReadContextSelection,
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, ExprTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait, Set,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AssistantStoreError, entity::agent_main_request as request, signed};

pub mod coordination;
pub mod dispatch;

const QUEUED: &str = "queued";
const ACTIVATED: &str = "activated";
const CANCELLED: &str = "cancelled";
const SUPERSEDED: &str = "superseded";
const FAILED: &str = "failed";
pub const PENDING_PER_CONVERSATION: u64 = 32;

/// Only ordinary conversational input enters the queue. Native Goal and
/// rehearsal entry points retain their separate admission contracts.
pub fn accepts_owner_ask(ask: &AiAssistantAsk) -> bool {
    !ask.start_goal
        && ask
            .conversation_id
            .as_deref()
            .map(str::trim)
            .is_some_and(desk_diagnose_core::conversation_key::is_valid_client_conversation_id)
}

pub enum OwnerQueueTurn {
    Submit {
        ask: AiAssistantAsk,
        reply_connection_id: String,
    },
    Activate {
        request_id: String,
        conversation_id: String,
        execution_conversation_id: String,
        destination: DestinationIdentity,
    },
}

impl OwnerQueueTurn {
    /// The client continuation still resolves the queue's subject. Only the
    /// executor can route an item to its separate native work conversation.
    pub fn execution_conversation<'a>(
        &'a self,
        actor: &str,
        original_conversation: &str,
    ) -> Result<Option<&'a str>, AssistantStoreError> {
        let Self::Activate {
            request_id,
            conversation_id,
            execution_conversation_id,
            ..
        } = self
        else {
            return Ok(None);
        };
        if original_conversation != conversation_id
            || (execution_conversation_id != conversation_id
                && *execution_conversation_id
                    != native_ids::workbench_work_conversation(actor, conversation_id, request_id))
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(Some(execution_conversation_id))
    }

    pub fn admission(&self) -> OwnerAdmission<'_> {
        match self {
            Self::Submit {
                ask,
                reply_connection_id,
            } => OwnerAdmission::Enqueue {
                ask,
                limit: PENDING_PER_CONVERSATION,
                reply_connection_id: Some(reply_connection_id),
            },
            Self::Activate { request_id, .. } => OwnerAdmission::Activate { request_id },
        }
    }
}

#[derive(Clone, Copy)]
pub enum OwnerAdmission<'a> {
    Native,
    Enqueue {
        ask: &'a AiAssistantAsk,
        limit: u64,
        reply_connection_id: Option<&'a str>,
    },
    Activate {
        request_id: &'a str,
    },
}

pub enum OwnerAdmissionResult<R> {
    Native(R),
    Queued(Box<request::Model>),
    Dispatched,
    Waiting,
}

/// Queue retries and activation reuse the accepted binding. Remote name lookup
/// is reserved for first intake; a later repository may reuse the same name.
pub async fn frozen_read_selection(
    db: &impl ConnectionTrait,
    queue: Option<&OwnerQueueTurn>,
    actor: &str,
    conversation: &str,
    device: &str,
    ask: &AiAssistantAsk,
) -> Result<Option<ReadContextSelection>, AssistantStoreError> {
    let row = match queue {
        None => return Ok(None),
        Some(OwnerQueueTurn::Submit { ask: original, .. }) => {
            if original.selected_issues != ask.selected_issues {
                return Err(AssistantStoreError::Inaccessible);
            }
            replay(db, actor, conversation, original, device).await?
        }
        Some(OwnerQueueTurn::Activate {
            request_id,
            conversation_id,
            ..
        }) => Some(
            load(db, actor, conversation_id, request_id)
                .await?
                .ok_or(AssistantStoreError::Inaccessible)?,
        ),
    };
    let Some(row) = row else {
        return Ok(None);
    };
    if row.pending_input_json.is_none() {
        return Ok(Some(ReadContextSelection::default()));
    }
    let frozen = FrozenOwnerInput::decode(&row)?;
    if frozen.ask.selected_issues != ask.selected_issues {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(frozen.reads))
}

impl<R> OwnerAdmissionResult<R> {
    pub fn native(self) -> Result<R, AssistantStoreError> {
        match self {
            Self::Native(receipt) => Ok(receipt),
            _ => Err(AssistantStoreError::Invalid(
                "Expected native assistant input receipt",
            )),
        }
    }
}

/// A released model lease alone is insufficient to start another request.
/// Hosts also check their durable approval and delivery records under the lock.
pub fn holds_input_slot(session: &PersistedAgentSession) -> bool {
    session.main_stopped
        || !session.agent_role.is_main()
        || session.turn_state.is_active()
        || !session.execution_state.tasks().is_empty()
        || session.subagent_wait.is_some()
        || session.ready_subagent_wait.is_some()
        || session.ready_subagent_notification.is_some()
        || session.permission_requests.iter().any(|permission| {
            permission.input_revision == session.input_revision
                && permission.state
                    == desk_diagnose_core::dynamic_run::PermissionRequestState::Pending
        })
        || session
            .pending_auto_triggers
            .iter()
            .any(|trigger| trigger.chain_id == session.chain_id)
        || (session.latest_input_seq > session.handled_input_seq
            && session
                .conversation
                .iter()
                .rev()
                .find(|message| {
                    message.role == ChatRole::User
                        && !desk_diagnose_core::permission_resume::is_resume_control_message(
                            message,
                        )
                })
                .is_some_and(|message| message.turn_id.is_none()))
}

pub fn same_input(fence: &AssistantTurnFence, session: &PersistedAgentSession) -> bool {
    fence.conversation_id == session.conversation_id
        && fence.actor_id == session.actor_id
        && fence.input_revision == session.input_revision
        && fence.control_revision == session.control_revision
}

/// Resume queue eligibility without reopening the stopped turn or its work.
pub fn resume_queue_control(
    session: &mut PersistedAgentSession,
    expected_control_revision: u64,
    now: &str,
) -> Result<bool, AssistantStoreError> {
    if !session.agent_role.is_main()
        || session.surface != desk_diagnose_core::session::AgentSessionSurface::AiAssistant
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let next = expected_control_revision
        .checked_add(1)
        .filter(|next| *next <= i64::MAX as u64)
        .ok_or(AssistantStoreError::Invalid(
            "Assistant control revision exhausted",
        ))?;
    if !session.main_stopped && session.control_revision == next {
        return Ok(false);
    }
    if !session.main_stopped || session.control_revision != expected_control_revision {
        return Err(AssistantStoreError::Stale {
            current_revision: session.control_revision,
        });
    }
    session.control_revision = next;
    session.main_stopped = false;
    for permission in &mut session.permission_requests {
        if permission.state.can_user_decide() {
            permission.state = desk_diagnose_core::dynamic_run::PermissionRequestState::Withdrawn;
        }
    }
    session.updated_at = now.into();
    Ok(true)
}

pub fn pending_conversation_ids() -> sea_orm::sea_query::SelectStatement {
    request::Entity::find()
        .select_only()
        .column(request::Column::ConversationId)
        .filter(recoverable_condition())
        .into_query()
}

fn recoverable_condition() -> Condition {
    Condition::any().add(request::Column::State.eq(QUEUED)).add(
        Condition::all()
            .add(request::Column::State.eq(ACTIVATED))
            .add(request::Column::PendingInputJson.is_not_null()),
    )
}

/// Page conversation heads, including an activated input whose native claim
/// did not commit. Requests behind that input wait for the next pass.
pub async fn recovery_heads(
    db: &impl ConnectionTrait,
    after_id: i64,
    limit: u64,
) -> Result<Vec<request::Model>, AssistantStoreError> {
    use request::Column;
    use sea_orm::sea_query::Func;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let active = Condition::all()
        .add(Column::State.eq(ACTIVATED))
        .add(Column::PendingInputJson.is_not_null());
    let heads = request::Entity::find()
        .select_only()
        .expr(Func::coalesce([
            Expr::case(Column::State.eq(ACTIVATED), Expr::col(Column::Id))
                .finally(Expr::value(None::<i64>))
                .min(),
            Expr::col(Column::Id).min(),
        ]))
        .filter(Condition::any().add(active).add(queued_and_due(now_ms)))
        .group_by(Column::ConversationId)
        .into_query();
    Ok(request::Entity::find()
        .filter(request::Column::Id.gt(after_id))
        .filter(request::Column::Id.in_subquery(heads))
        .order_by_asc(request::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// A newer input revision owns the native execution slot. Retire only the
/// superseded activation's remaining recovery copy.
pub async fn retire_superseded(
    db: &impl ConnectionTrait,
    row: &request::Model,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    if let Some(revision) = row.input_revision {
        clear_payload(request::Entity::update_many(), now_ms)
            .col_expr(request::Column::State, Expr::value(SUPERSEDED))
            .filter(request::Column::Id.eq(row.id))
            .filter(request::Column::RequestId.eq(&row.request_id))
            .filter(request::Column::InputRevision.eq(revision))
            .filter(request::Column::State.eq(ACTIVATED))
            .filter(request::Column::PendingInputJson.is_not_null())
            .exec(db)
            .await?;
    }
    Ok(())
}

pub async fn fail_unactivated(
    db: &impl ConnectionTrait,
    row: &request::Model,
    now_ms: i64,
    reason: MainRequestFailure,
) -> Result<(), AssistantStoreError> {
    fail_payload(pending_row_update(row), now_ms, reason)
        .filter(request::Column::UpdatedAtMs.eq(row.updated_at_ms))
        .filter(request::Column::CoordinationAttempts.eq(row.coordination_attempts))
        .exec(db)
        .await?;
    Ok(())
}

/// Close an activation that lost its authority before the first claim. The host
/// commits the returned session mutation under the native row CAS.
pub async fn fail_unclaimed(
    txn: &impl ConnectionTrait,
    row: &request::Model,
    session: &mut PersistedAgentSession,
    now: chrono::DateTime<chrono::Utc>,
    reason: MainRequestFailure,
) -> Result<bool, AssistantStoreError> {
    if session.queue_conversation_id() != row.conversation_id
        || session
            .agent_role
            .workbench_request_id()
            .is_some_and(|id| id != row.request_id)
        || session.actor_id != row.actor_id
        || row.input_revision != Some(signed(session.input_revision)?)
        || session.main_stopped
        || session.turn_state.is_active()
        || session.latest_input_seq <= session.handled_input_seq
        || !desk_diagnose_core::permission_resume::latest_user_requirement(&session.conversation)
            .is_some_and(|message| {
                message.message_id == row.client_message_id && message.turn_id.is_none()
            })
    {
        return Ok(false);
    }
    let updated = fail_payload(
        request::Entity::update_many(),
        now.timestamp_millis(),
        reason,
    )
    .filter(request::Column::Id.eq(row.id))
    .filter(request::Column::RequestId.eq(&row.request_id))
    .filter(request::Column::State.eq(ACTIVATED))
    .filter(request::Column::InputRevision.eq(row.input_revision))
    .filter(request::Column::PendingInputJson.is_not_null())
    .exec(txn)
    .await?;
    if updated.rows_affected != 1 {
        return Ok(false);
    }
    session.handled_input_seq = session.latest_input_seq;
    session.finish_turn(
        desk_diagnose_core::session::TurnState::Failed,
        now.to_rfc3339(),
    );
    session.current_request_id = Some(row.request_id.clone());
    session.terminal_error = Some(failure_error(reason));
    Ok(true)
}

fn failure_code(reason: MainRequestFailure) -> &'static str {
    match reason {
        MainRequestFailure::SourceUnavailable => "source_unavailable",
        MainRequestFailure::DestinationChanged => "destination_changed",
        MainRequestFailure::CoordinationExhausted => "coordination_exhausted",
    }
}

pub fn failure_error(reason: MainRequestFailure) -> desk_agent_protocol::AgentError {
    use desk_agent_protocol::AgentErrorKind;
    let (kind, message) = match reason {
        MainRequestFailure::SourceUnavailable => (
            AgentErrorKind::PermissionDenied,
            "Queued assistant input expired or its original access was removed",
        ),
        MainRequestFailure::DestinationChanged => (
            AgentErrorKind::ModelRejected,
            "Queued assistant input no longer matches its original model destination",
        ),
        MainRequestFailure::CoordinationExhausted => (
            AgentErrorKind::ModelRejected,
            "Queued assistant input could not be routed after repeated attempts",
        ),
    };
    desk_agent_protocol::AgentError {
        kind,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

pub async fn delete_for_conversation(
    txn: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
) -> Result<(), sea_orm::DbErr> {
    request::Entity::delete_many()
        .filter(request::Column::ActorId.eq(actor_id))
        .filter(request::Column::ConversationId.eq(conversation_id))
        .exec(txn)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenOwnerInput {
    pub ask: AiAssistantAsk,
    pub device_id: String,
    pub targets: Vec<AssistantInputTarget>,
    pub reads: ReadContextSelection,
    pub destination: DestinationIdentity,
}

impl FrozenOwnerInput {
    pub fn authorization_expired(&self, now_ms: i64) -> bool {
        self.reads
            .expires_at
            .as_deref()
            .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
            .is_some_and(|date| date.timestamp_millis() <= now_ms)
            || self.reads.object_attachments.iter().any(|attachment| {
                !attachment.is_active_at(u64::try_from(now_ms).unwrap_or_default())
            })
    }

    pub async fn roster_matches(
        &self,
        db: &impl ConnectionTrait,
        actor: &str,
        conversation: &str,
    ) -> Result<bool, AssistantStoreError> {
        if self.targets.is_empty() {
            return Ok(true);
        }
        let roster = crate::device_roster::load(db, actor, conversation).await?;
        Ok(self.targets.iter().all(|target| match &roster {
            Some(roster) => roster
                .members
                .get(&target.device_id)
                .copied()
                .is_some_and(|generation| Some(generation) == target.member_generation),
            None => target.member_generation.is_none(),
        }))
    }

    pub fn capture(
        ask: &AiAssistantAsk,
        device_id: &str,
        targets: &[AssistantInputTarget],
        reads: Option<&ReadContextSelection>,
        message: &ChatMessage,
    ) -> Result<Self, AssistantStoreError> {
        let invalid =
            || AssistantStoreError::Invalid("Assistant request differs from its native input");
        let reads = reads.ok_or_else(invalid)?;
        reads.validate().map_err(|_| invalid())?;
        let [destination] = message
            .data_envelope
            .as_ref()
            .map(|envelope| envelope.allowed_destinations.as_slice())
            .ok_or_else(invalid)?
        else {
            return Err(invalid());
        };
        if ask.start_goal
            || ask.previous_completed_goal_id.is_some()
            || ask.question != message.text
            || reads.external_issues.len() != ask.selected_issues.len()
            || !ask.selected_issues.iter().all(|selected| {
                reads
                    .external_issues
                    .iter()
                    .any(|access| access.matches_selection(selected))
            })
            || ask.client_message_id != message.message_id
            || !ask
                .target_refs
                .iter()
                .map(String::as_str)
                .eq(targets.iter().map(|target| target.reference.as_str()))
        {
            return Err(invalid());
        }
        Ok(Self {
            ask: ask.clone(),
            device_id: device_id.into(),
            targets: targets.to_vec(),
            reads: reads.clone(),
            destination: destination.clone(),
        })
    }

    pub fn decode(row: &request::Model) -> Result<Self, AssistantStoreError> {
        let invalid = || AssistantStoreError::Invalid("Invalid pending assistant request");
        let input: Self =
            serde_json::from_str(row.pending_input_json.as_deref().ok_or_else(invalid)?)
                .map_err(|_| invalid())?;
        if !matches!(
            (row.state.as_str(), row.input_revision),
            (QUEUED, None) | (ACTIVATED, Some(1..))
        ) || input.ask.client_message_id != row.client_message_id
            || input.submission_sha256()? != row.submission_sha256
        {
            return Err(invalid());
        }
        Ok(input)
    }

    fn submission_sha256(&self) -> Result<String, AssistantStoreError> {
        submission_sha256(&self.ask, &self.device_id)
    }
}

fn submission_sha256(ask: &AiAssistantAsk, device_id: &str) -> Result<String, AssistantStoreError> {
    let submitted = serde_json::to_vec(&(ask, device_id))
        .map_err(|_| AssistantStoreError::Invalid("Could not encode assistant request"))?;
    Ok(format!("{:x}", Sha256::digest(submitted)))
}

fn owner_request_id(actor_id: &str, conversation_id: &str, message_id: &str) -> String {
    native_ids::stable(
        Kind::AssistantMainRequest,
        &(actor_id, conversation_id, message_id),
    )
}

pub async fn load(
    db: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
    request_id: &str,
) -> Result<Option<request::Model>, AssistantStoreError> {
    Ok(request::Entity::find()
        .filter(request::Column::ActorId.eq(actor_id))
        .filter(request::Column::ConversationId.eq(conversation_id))
        .filter(request::Column::RequestId.eq(request_id))
        .one(db)
        .await?)
}

/// A native work conversation can only be activated by its exact queued owner
/// request. The host verifies the returned coordinator under its control lock.
pub async fn workbench_for_activation(
    db: &impl ConnectionTrait,
    admission: OwnerAdmission<'_>,
    actor_id: &str,
    work_conversation_id: &str,
) -> Result<Option<String>, AssistantStoreError> {
    if !native_ids::is(Kind::WorkbenchWorkConversation, work_conversation_id) {
        return Ok(None);
    }
    let OwnerAdmission::Activate { request_id } = admission else {
        return Err(AssistantStoreError::Inaccessible);
    };
    let workbench_id = request::Entity::find()
        .select_only()
        .column(request::Column::ConversationId)
        .filter(request::Column::RequestId.eq(request_id))
        .filter(request::Column::ActorId.eq(actor_id))
        .into_tuple::<String>()
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if native_ids::workbench_work_conversation(actor_id, &workbench_id, request_id)
        != work_conversation_id
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(workbench_id))
}

pub fn decode_workbench_coordinator(
    state_json: &str,
    actor_id: &str,
    workbench_id: &str,
) -> Result<Box<PersistedAgentSession>, AssistantStoreError> {
    let session = PersistedAgentSession::decode_boxed_json(state_json)
        .map_err(|_| AssistantStoreError::Invalid("Invalid workbench coordinator"))?;
    if session.actor_id != actor_id
        || session.conversation_id != workbench_id
        || !is_workbench_root(&session)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(session)
}

pub fn is_workbench_root(session: &PersistedAgentSession) -> bool {
    session.agent_role.workbench_id() == Some(session.conversation_id.as_str())
        && session.agent_role.workbench_request_id().is_none()
}

/// Native work identity is derived from its durable activation receipt.
pub async fn activated_workbench_conversations_on(
    db: &impl ConnectionTrait,
    root: &PersistedAgentSession,
) -> Result<std::collections::BTreeSet<String>, sea_orm::DbErr> {
    let requests = request::Entity::find()
        .filter(request::Column::ActorId.eq(&root.actor_id))
        .filter(request::Column::ConversationId.eq(&root.conversation_id))
        .filter(request::Column::InputRevision.is_not_null())
        .select_only()
        .column(request::Column::RequestId)
        .into_tuple::<String>()
        .all(db)
        .await?;
    Ok(requests
        .into_iter()
        .map(|request| {
            native_ids::workbench_work_conversation(&root.actor_id, &root.conversation_id, &request)
        })
        .collect())
}

/// Activations that have not begun execution remain in the paused intake queue.
pub async fn unclaimed_workbench_conversations(
    db: &impl ConnectionTrait,
    root: &PersistedAgentSession,
) -> Result<std::collections::BTreeSet<String>, AssistantStoreError> {
    let requests = request::Entity::find()
        .filter(request::Column::ActorId.eq(&root.actor_id))
        .filter(request::Column::ConversationId.eq(&root.conversation_id))
        .filter(request::Column::State.eq(ACTIVATED))
        .filter(request::Column::PendingInputJson.is_not_null())
        .select_only()
        .column(request::Column::RequestId)
        .into_tuple::<String>()
        .all(db)
        .await?;
    Ok(requests
        .into_iter()
        .map(|request| {
            native_ids::workbench_work_conversation(&root.actor_id, &root.conversation_id, &request)
        })
        .collect())
}

pub fn coordinating_work_conversation(session: &PersistedAgentSession) -> Option<String> {
    let request = session.coordinating_work.as_deref()?;
    Some(native_ids::workbench_work_conversation(
        &session.actor_id,
        &session.conversation_id,
        request,
    ))
}

pub fn workbench_execution(
    root: &PersistedAgentSession,
    row: &request::Model,
) -> Result<Option<String>, AssistantStoreError> {
    let Some(workbench_id) = root.agent_role.workbench_id() else {
        return Ok(None);
    };
    if workbench_id != row.conversation_id
        || root.conversation_id != row.conversation_id
        || root.actor_id != row.actor_id
        || root.agent_role.workbench_request_id().is_some()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(native_ids::workbench_work_conversation(
        &row.actor_id,
        workbench_id,
        &row.request_id,
    )))
}

/// Active queue entries and recent failures, or a specific receipt regardless
/// of state. This view never loads the frozen input or any of its attachments.
pub async fn receipts(
    db: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
    request_id: Option<&str>,
) -> Result<Vec<MainRequestReceipt>, AssistantStoreError> {
    use request::Column;
    let query = request::Entity::find()
        .select_only()
        .columns([
            Column::RequestId,
            Column::ClientMessageId,
            Column::State,
            Column::Failure,
            Column::InputRevision,
            Column::AcceptedAtMs,
        ])
        .filter(Column::ActorId.eq(actor_id))
        .filter(Column::ConversationId.eq(conversation_id));
    type ReceiptRow = (String, String, String, Option<String>, Option<i64>, i64);
    let rows: Vec<ReceiptRow> = match request_id {
        Some(id) => {
            query
                .filter(Column::RequestId.eq(id))
                .limit(1)
                .into_tuple()
                .all(db)
                .await?
        }
        None => {
            let mut rows: Vec<ReceiptRow> = query
                .clone()
                .filter(recoverable_condition())
                .order_by_asc(Column::Id)
                .limit(PENDING_PER_CONVERSATION + 1)
                .into_tuple()
                .all(db)
                .await?;
            rows.extend(
                query
                    .filter(Column::State.eq(FAILED))
                    .order_by_desc(Column::Id)
                    .limit(8)
                    .into_tuple::<ReceiptRow>()
                    .all(db)
                    .await?,
            );
            rows
        }
    };
    rows.into_iter()
        .map(
            |(request_id, client_message_id, state, failure, input_revision, accepted_at_ms)| {
                let state = match state.as_str() {
                    QUEUED => MainRequestState::Queued,
                    ACTIVATED => MainRequestState::Activated,
                    CANCELLED => MainRequestState::Cancelled,
                    SUPERSEDED => MainRequestState::Superseded,
                    FAILED => MainRequestState::Failed,
                    _ => {
                        return Err(AssistantStoreError::Invalid(
                            "Invalid assistant request state",
                        ));
                    }
                };
                let failure = match failure.as_deref() {
                    None => None,
                    Some("source_unavailable") => Some(MainRequestFailure::SourceUnavailable),
                    Some("destination_changed") => Some(MainRequestFailure::DestinationChanged),
                    Some("coordination_exhausted") => {
                        Some(MainRequestFailure::CoordinationExhausted)
                    }
                    Some(_) => {
                        return Err(AssistantStoreError::Invalid(
                            "Invalid assistant request failure",
                        ));
                    }
                };
                Ok(MainRequestReceipt {
                    request_id,
                    client_message_id,
                    state,
                    failure,
                    input_revision,
                    accepted_at_ms,
                })
            },
        )
        .collect()
}

/// Compare against the live queued state. A retry of a successful cancellation
/// returns the same receipt; activation and cancellation cannot both commit.
pub async fn cancel_owner_request(
    txn: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
    request_id: &str,
    now_ms: i64,
) -> Result<MainRequestReceipt, AssistantStoreError> {
    clear_payload(request::Entity::update_many(), now_ms)
        .col_expr(request::Column::State, Expr::value(CANCELLED))
        .filter(request::Column::ActorId.eq(actor_id))
        .filter(request::Column::ConversationId.eq(conversation_id))
        .filter(request::Column::RequestId.eq(request_id))
        .filter(request::Column::State.eq(QUEUED))
        .exec(txn)
        .await?;
    let Some(receipt) = receipts(txn, actor_id, conversation_id, Some(request_id))
        .await?
        .pop()
    else {
        return Err(AssistantStoreError::Inaccessible);
    };
    if receipt.state != MainRequestState::Cancelled {
        return Err(AssistantStoreError::Stale {
            current_revision: receipt.input_revision.unwrap_or_default() as u64,
        });
    }
    Ok(receipt)
}

/// The caller freezes the input after authenticating it. A transport retry
/// matches the submitted intent and never replaces the first capture.
pub async fn enqueue(
    txn: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
    input: &FrozenOwnerInput,
    reply_connection_id: Option<&str>,
    pending_limit: u64,
    now_ms: i64,
) -> Result<request::Model, AssistantStoreError> {
    let message_id = &input.ask.client_message_id;
    let request_id = owner_request_id(actor_id, conversation_id, message_id);
    let submission_sha256 = input.submission_sha256()?;
    if let Some(existing) =
        replay(txn, actor_id, conversation_id, &input.ask, &input.device_id).await?
    {
        return Ok(existing);
    }
    if pending_for(actor_id, conversation_id).count(txn).await? >= pending_limit {
        return Err(AssistantStoreError::Capacity);
    }
    Ok(request::Entity::insert(request::ActiveModel {
        request_id: Set(request_id),
        actor_id: Set(actor_id.into()),
        conversation_id: Set(conversation_id.into()),
        client_message_id: Set(message_id.clone()),
        submission_sha256: Set(submission_sha256),
        state: Set(QUEUED.into()),
        failure: Set(None),
        coordinated: Set(false),
        coordination_attempts: Set(0),
        retry_at_ms: Set(None),
        pending_input_json: Set(Some(serde_json::to_string(input).map_err(|_| {
            AssistantStoreError::Invalid("Could not encode assistant request")
        })?)),
        reply_connection_id: Set(reply_connection_id
            .filter(|id| !id.is_empty() && id.len() <= 64)
            .map(str::to_owned)),
        input_revision: Set(None),
        accepted_at_ms: Set(now_ms),
        updated_at_ms: Set(now_ms),
        ..Default::default()
    })
    .exec_with_returning(txn)
    .await?)
}

pub async fn replay(
    txn: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
    ask: &AiAssistantAsk,
    device_id: &str,
) -> Result<Option<request::Model>, AssistantStoreError> {
    let message_id = &ask.client_message_id;
    let id = owner_request_id(actor_id, conversation_id, message_id);
    let existing = load(txn, actor_id, conversation_id, &id).await?;
    if let Some(row) = &existing
        && (row.client_message_id != *message_id
            || row.submission_sha256 != submission_sha256(ask, device_id)?)
    {
        return Err(AssistantStoreError::Invalid(
            "Assistant request was already submitted with different input",
        ));
    }
    Ok(existing)
}

fn pending_for(actor_id: &str, conversation_id: &str) -> sea_orm::Select<request::Entity> {
    request::Entity::find()
        .filter(request::Column::ActorId.eq(actor_id))
        .filter(request::Column::ConversationId.eq(conversation_id))
        .filter(request::Column::State.eq(QUEUED))
}

fn queued_and_due(now_ms: i64) -> Condition {
    Condition::all().add(request::Column::State.eq(QUEUED)).add(
        Condition::any()
            .add(request::Column::RetryAtMs.is_null())
            .add(request::Column::RetryAtMs.lte(now_ms)),
    )
}

fn eligible_pending(
    actor_id: &str,
    conversation_id: &str,
    now_ms: i64,
) -> sea_orm::Select<request::Entity> {
    request::Entity::find()
        .filter(request::Column::ActorId.eq(actor_id))
        .filter(request::Column::ConversationId.eq(conversation_id))
        .filter(queued_and_due(now_ms))
}

pub async fn first_pending(
    txn: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
) -> Result<Option<request::Model>, AssistantStoreError> {
    Ok(eligible_pending(
        actor_id,
        conversation_id,
        chrono::Utc::now().timestamp_millis(),
    )
    .order_by_asc(request::Column::Id)
    .one(txn)
    .await?)
}

/// Call in the original input transaction after it assigns the native revision.
pub async fn activate(
    txn: &impl ConnectionTrait,
    row: &request::Model,
    input_revision: u64,
    now_ms: i64,
) -> Result<bool, AssistantStoreError> {
    activate_with_dispatch(txn, row, input_revision, now_ms, None).await
}

/// The caller commits this receipt, the native input and the Worker in one transaction.
pub async fn activate_with_dispatch(
    txn: &impl ConnectionTrait,
    row: &request::Model,
    input_revision: u64,
    now_ms: i64,
    worker_request: Option<&str>,
) -> Result<bool, AssistantStoreError> {
    if input_revision == 0
        || worker_request
            .is_some_and(|id| row.first_dispatch_json.is_none() || !native_ids::is(Kind::Task, id))
    {
        return Err(AssistantStoreError::Invalid("Invalid assistant activation"));
    }
    let update = pending_row_update(row)
        .filter(
            Condition::any()
                .add(request::Column::Coordinated.eq(true))
                .add(request::Column::FirstDispatchJson.is_not_null())
                .add(request::Column::CoordinationAttempts.lt(coordination::MAX_ATTEMPTS)),
        )
        .filter(request::Column::FirstDispatchJson.eq(row.first_dispatch_json.clone()))
        .col_expr(request::Column::State, Expr::value(ACTIVATED))
        .col_expr(
            request::Column::InputRevision,
            Expr::value(signed(input_revision)?),
        )
        .col_expr(request::Column::UpdatedAtMs, Expr::value(now_ms));
    let update = if row.first_dispatch_json.is_some() {
        let update = update.col_expr(request::Column::Coordinated, Expr::value(true));
        if let Some(worker_request) = worker_request {
            clear_payload(update, now_ms).col_expr(
                request::Column::FirstWorkerRequestId,
                Expr::value(worker_request),
            )
        } else {
            update.col_expr(
                request::Column::FirstDispatchJson,
                Expr::value(None::<String>),
            )
        }
    } else {
        update
    };
    Ok(update.exec(txn).await?.rows_affected == 1)
}

/// Remove the recovery copy when the first native claim durably owns this input.
/// Call in the same transaction as that claim's session CAS.
pub async fn mark_claimed(
    txn: &impl ConnectionTrait,
    request_id: &str,
    session: &PersistedAgentSession,
    message_id: &str,
    now_ms: i64,
) -> Result<bool, AssistantStoreError> {
    if session
        .agent_role
        .workbench_request_id()
        .is_some_and(|expected| expected != request_id)
    {
        return Ok(false);
    }
    Ok(clear_payload(request::Entity::update_many(), now_ms)
        .filter(request::Column::RequestId.eq(request_id))
        .filter(request::Column::ConversationId.eq(session.queue_conversation_id()))
        .filter(request::Column::ActorId.eq(&session.actor_id))
        .filter(request::Column::ClientMessageId.eq(message_id))
        .filter(request::Column::InputRevision.eq(signed(session.input_revision)?))
        .filter(request::Column::State.eq(ACTIVATED))
        .filter(request::Column::PendingInputJson.is_not_null())
        .exec(txn)
        .await?
        .rows_affected
        == 1)
}

fn pending_row_update(row: &request::Model) -> sea_orm::UpdateMany<request::Entity> {
    request::Entity::update_many()
        .filter(request::Column::Id.eq(row.id))
        .filter(request::Column::RequestId.eq(&row.request_id))
        .filter(request::Column::State.eq(QUEUED))
}

fn clear_payload(
    update: sea_orm::UpdateMany<request::Entity>,
    now_ms: i64,
) -> sea_orm::UpdateMany<request::Entity> {
    update
        .col_expr(
            request::Column::FirstDispatchJson,
            Expr::value(None::<String>),
        )
        .col_expr(
            request::Column::PendingInputJson,
            Expr::value(None::<String>),
        )
        .col_expr(
            request::Column::ReplyConnectionId,
            Expr::value(None::<String>),
        )
        .col_expr(request::Column::UpdatedAtMs, Expr::value(now_ms))
}

fn fail_payload(
    update: sea_orm::UpdateMany<request::Entity>,
    now_ms: i64,
    reason: MainRequestFailure,
) -> sea_orm::UpdateMany<request::Entity> {
    clear_payload(update, now_ms)
        .col_expr(request::Column::State, Expr::value(FAILED))
        .col_expr(request::Column::Failure, Expr::value(failure_code(reason)))
        .col_expr(request::Column::RetryAtMs, Expr::value(None::<i64>))
}
