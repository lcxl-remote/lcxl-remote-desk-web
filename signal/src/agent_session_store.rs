//! SQLite-backed agent sessions for the single-node OSS signal central brain.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::capability_grant::CAPABILITY_GRANT_SCHEMA_VERSION;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope};
use desk_diagnose_core::context_attachment::{
    AttachmentRuntimeBinding, AttachmentStaleReason, AttachmentState, ContextAttachment,
    ContextAttachmentKind,
};
use desk_diagnose_core::dynamic_run::{
    AGENT_RUN_EVENT_SCHEMA_VERSION, PermissionRequestedEvent, TaskStatusUpdatedEvent,
};
pub use desk_diagnose_core::permission_grant::PermissionGrantIssuanceContext;
use desk_diagnose_core::permission_grant::build_permission_grants;
use desk_diagnose_core::seam::{ClaimError, ClaimTurnParams, SessionSeam};
use desk_diagnose_core::session::{
    ActionIdentity, AgentSessionSurface, ExecutionState, PendingAutoTrigger, PersistedAgentSession,
    RecoveryVerdict, TurnClaimError, TurnState, WorkKind,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set, TransactionTrait,
};
use sha2::{Digest, Sha256};

use crate::entity::{agent_capability_grant, agent_exec_task, agent_run_event, agent_session};

const LEASE_TTL_SECS: i64 = 90;
const CLAIM_ATTEMPTS: usize = 5;

#[derive(Clone)]
pub struct SignalAgentSessionStore {
    db: DatabaseConnection,
    client_conversation_id: Option<String>,
    surface: AgentSessionSurface,
    context_selection: Option<ContextSelectionClaim>,
}

/// Server-authoritative context selection frozen for one Device Assistant
/// claim. Candidate attachment identities are minted once by the orchestrator;
/// SQLite CAS retries reuse the exact same identities instead of duplicating
/// refs. Runtime bindings include every currently-ready context capability so
/// deselection is distinguishable from Provider unavailability.
#[derive(Debug, Clone)]
pub struct ContextSelectionClaim {
    pub selected_capability_ids: Vec<String>,
    pub runtime_bindings: Vec<AttachmentRuntimeBinding>,
    pub candidates: Vec<ContextAttachment>,
    pub now_unix_ms: u64,
}

#[derive(Debug, Clone)]
pub enum ObjectContextMutation {
    Attach(ContextAttachment),
    Detach {
        attachment_id: String,
    },
    Refresh {
        stale_attachment_id: String,
        replacement: ContextAttachment,
    },
}

impl SignalAgentSessionStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            client_conversation_id: None,
            surface: AgentSessionSurface::Unknown,
            context_selection: None,
        }
    }

    pub fn with_client_metadata(
        mut self,
        client_conversation_id: Option<String>,
        surface: AgentSessionSurface,
    ) -> Self {
        self.client_conversation_id = client_conversation_id;
        self.surface = surface;
        self
    }

    pub fn with_context_selection(mut self, selection: ContextSelectionClaim) -> Self {
        self.context_selection = Some(selection);
        self
    }

    fn reconcile_context_selection(
        &self,
        session: &mut PersistedAgentSession,
    ) -> Result<bool, AgentError> {
        let Some(selection) = &self.context_selection else {
            return Ok(false);
        };
        let mut changed = false;
        // CurrentScreen is deliberately ephemeral. Older builds briefly wrote
        // attachment metadata for it; remove that metadata on the next claim as
        // well as preventing new entries in the orchestrator.
        let attachment_count = session.context_attachments.len();
        session
            .context_attachments
            .retain(|attachment| attachment.kind != ContextAttachmentKind::CurrentScreen);
        changed |= session.context_attachments.len() != attachment_count;
        for attachment in session.context_attachments.iter_mut().filter(|attachment| {
            attachment.kind == ContextAttachmentKind::InteractiveSession
                && matches!(attachment.state, AttachmentState::Active)
        }) {
            if let Some(reason) =
                attachment.stale_reason_against(selection.now_unix_ms, &selection.runtime_bindings)
            {
                attachment.mark_stale(reason);
                changed = true;
            }
        }

        let selected = selection
            .selected_capability_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let detach = session
            .context_attachments
            .iter()
            .filter(|attachment| {
                matches!(attachment.state, AttachmentState::Active)
                    && attachment.kind == ContextAttachmentKind::InteractiveSession
                    && !selected.contains(attachment.object_ref.source_capability_id.as_str())
            })
            .map(|attachment| attachment.attachment_id.clone())
            .collect::<Vec<_>>();
        for attachment_id in detach {
            changed |= session.detach_context(&attachment_id);
        }

        for candidate in &selection.candidates {
            let already_active = session.context_attachments.iter().any(|attachment| {
                matches!(attachment.state, AttachmentState::Active)
                    && attachment.object_ref.source_provider_id
                        == candidate.object_ref.source_provider_id
                    && attachment.object_ref.source_capability_id
                        == candidate.object_ref.source_capability_id
                    && attachment.object_ref.object_incarnation
                        == candidate.object_ref.object_incarnation
            });
            if !already_active {
                changed |= session.attach_context(candidate.clone()).map_err(|error| {
                    internal(format!("reconcile Device Assistant context: {error}"))
                })?;
            }
        }
        Ok(changed)
    }

    /// Reconcile context independently from a model turn. This is the durable
    /// add/remove event path used by the selector. It never takes a model lease
    /// and refuses to race a live turn; the client may retry with the same
    /// request identity after that turn settles.
    pub async fn update_context_selection(
        &self,
        conversation_id: &str,
        actor_id: &str,
        device_id: &str,
        current_scope: AgentScope,
        now: &str,
    ) -> Result<bool, AgentError> {
        let now_dt = now_from(now);
        for _ in 0..CLAIM_ATTEMPTS {
            match find(&self.db, conversation_id)
                .await
                .map_err(|error| internal(format!("load context selection session: {error}")))?
            {
                Some(row) => {
                    let mut session =
                        PersistedAgentSession::decode_json(&row.state_json).map_err(|error| {
                            internal(format!("decode context selection session: {error}"))
                        })?;
                    session.version = row.version;
                    session
                        .check_subject(actor_id, device_id)
                        .map_err(|error| {
                            internal(format!("context selection subject: {error:?}"))
                        })?;
                    session.check_surface(self.surface).map_err(|error| {
                        internal(format!("context selection surface: {error:?}"))
                    })?;
                    if session.turn_state.is_active()
                        && row
                            .lease_deadline
                            .is_some_and(|deadline| deadline >= now_dt)
                    {
                        return Err(transport("Device Assistant context is busy"));
                    }
                    session.adopt_client_metadata(
                        self.client_conversation_id.as_deref(),
                        self.surface,
                    );
                    let changed = self.reconcile_context_selection(&mut session)?;
                    if !changed {
                        return Ok(false);
                    }
                    let new_version = row.version + 1;
                    session.version = new_version;
                    let state_json = session.encode_json_for_storage().map_err(|error| {
                        internal(format!("encode context selection session: {error}"))
                    })?;
                    let result = agent_session::Entity::update_many()
                        .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                        .col_expr(agent_session::Column::Version, Expr::value(new_version))
                        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_dt))
                        .filter(agent_session::Column::Id.eq(row.id))
                        .filter(agent_session::Column::Version.eq(row.version))
                        .exec(&self.db)
                        .await
                        .map_err(|error| {
                            internal(format!("save context selection session: {error}"))
                        })?;
                    if result.rows_affected == 1 {
                        return Ok(true);
                    }
                }
                None => {
                    let mut session = PersistedAgentSession::new(
                        conversation_id.to_string(),
                        actor_id.to_string(),
                        device_id.to_string(),
                        0,
                        current_scope.clone(),
                        now.to_string(),
                    );
                    session.adopt_client_metadata(
                        self.client_conversation_id.as_deref(),
                        self.surface,
                    );
                    let changed = self.reconcile_context_selection(&mut session)?;
                    let state_json = session.encode_json_for_storage().map_err(|error| {
                        internal(format!("encode new context selection session: {error}"))
                    })?;
                    let inserted = agent_session::ActiveModel {
                        conversation_id: Set(session.conversation_id.clone()),
                        actor_id: Set(session.actor_id.clone()),
                        device_id: Set(session.device_id.clone()),
                        state_json: Set(state_json),
                        version: Set(0),
                        lease_token: Set(0),
                        lease_deadline: Set(None),
                        created_at: Set(now_dt),
                        updated_at: Set(now_dt),
                        ..Default::default()
                    }
                    .insert(&self.db)
                    .await;
                    match inserted {
                        Ok(_) => return Ok(changed),
                        Err(_)
                            if find(&self.db, conversation_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some() =>
                        {
                            continue;
                        }
                        Err(error) => {
                            return Err(internal(format!(
                                "create context selection session: {error}"
                            )));
                        }
                    }
                }
            }
        }
        Err(transport("Device Assistant context update conflicted"))
    }

    pub async fn update_object_context(
        &self,
        conversation_id: &str,
        actor_id: &str,
        device_id: &str,
        current_scope: AgentScope,
        mutation: &ObjectContextMutation,
        now: &str,
    ) -> Result<bool, AgentError> {
        let now_dt = now_from(now);
        for _ in 0..CLAIM_ATTEMPTS {
            match find(&self.db, conversation_id)
                .await
                .map_err(|error| internal(format!("load object context session: {error}")))?
            {
                Some(row) => {
                    let mut session =
                        PersistedAgentSession::decode_json(&row.state_json).map_err(|error| {
                            internal(format!("decode object context session: {error}"))
                        })?;
                    session.version = row.version;
                    session
                        .check_subject(actor_id, device_id)
                        .map_err(|error| internal(format!("object context subject: {error:?}")))?;
                    session
                        .check_surface(self.surface)
                        .map_err(|error| internal(format!("object context surface: {error:?}")))?;
                    if session.turn_state.is_active()
                        && row
                            .lease_deadline
                            .is_some_and(|deadline| deadline >= now_dt)
                    {
                        return Err(transport("Device Assistant context is busy"));
                    }
                    let changed = apply_object_mutation(&mut session, mutation)?;
                    if !changed {
                        return Ok(false);
                    }
                    let new_version = row.version + 1;
                    session.version = new_version;
                    let state_json = session.encode_json_for_storage().map_err(|error| {
                        internal(format!("encode object context session: {error}"))
                    })?;
                    let result = agent_session::Entity::update_many()
                        .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                        .col_expr(agent_session::Column::Version, Expr::value(new_version))
                        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_dt))
                        .filter(agent_session::Column::Id.eq(row.id))
                        .filter(agent_session::Column::Version.eq(row.version))
                        .exec(&self.db)
                        .await
                        .map_err(|error| {
                            internal(format!("save object context session: {error}"))
                        })?;
                    if result.rows_affected == 1 {
                        return Ok(true);
                    }
                }
                None => {
                    if !matches!(mutation, ObjectContextMutation::Attach(_)) {
                        return Err(transport("Device Assistant attachment does not exist"));
                    }
                    let mut session = PersistedAgentSession::new(
                        conversation_id.to_string(),
                        actor_id.to_string(),
                        device_id.to_string(),
                        0,
                        current_scope.clone(),
                        now.to_string(),
                    );
                    session.adopt_client_metadata(
                        self.client_conversation_id.as_deref(),
                        self.surface,
                    );
                    let changed = apply_object_mutation(&mut session, mutation)?;
                    let state_json = session.encode_json_for_storage().map_err(|error| {
                        internal(format!("encode new object context session: {error}"))
                    })?;
                    let inserted = agent_session::ActiveModel {
                        conversation_id: Set(session.conversation_id.clone()),
                        actor_id: Set(session.actor_id.clone()),
                        device_id: Set(session.device_id.clone()),
                        state_json: Set(state_json),
                        version: Set(0),
                        lease_token: Set(0),
                        lease_deadline: Set(None),
                        created_at: Set(now_dt),
                        updated_at: Set(now_dt),
                        ..Default::default()
                    }
                    .insert(&self.db)
                    .await;
                    match inserted {
                        Ok(_) => return Ok(changed),
                        Err(_)
                            if find(&self.db, conversation_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some() =>
                        {
                            continue;
                        }
                        Err(error) => {
                            return Err(internal(format!(
                                "create object context session: {error}"
                            )));
                        }
                    }
                }
            }
        }
        Err(transport(
            "Device Assistant object context update conflicted",
        ))
    }

    /// Read the persisted conversation for the browser's recoverable view.
    ///
    /// `seq` is the SQLite row version, so polling clients can ignore stale
    /// snapshots. `active` prevents a client from rendering an in-progress
    /// conversation as settled.
    pub async fn read_snapshot(
        &self,
        conversation_id: &str,
    ) -> Result<Option<SessionSnapshot>, AgentError> {
        let Some(row) = find(&self.db, conversation_id)
            .await
            .map_err(|e| internal(format!("load agent session snapshot: {e}")))?
        else {
            return Ok(None);
        };
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|e| internal(format!("decode agent session snapshot: {e}")))?;
        let active_execution_generation = session
            .execution_state
            .waitable_task()
            .map(|action| action.execution_id.clone());
        let unresolved_action = match &session.execution_state {
            ExecutionState::OutcomeUnknown { action, .. } => Some(action.clone()),
            _ => None,
        };
        Ok(Some(SessionSnapshot {
            seq: row.version,
            active: session.turn_state.is_active(),
            request_id: session.current_request_id,
            active_execution_generation,
            unresolved_action,
            latest_input_seq: session.latest_input_seq,
            input_revision: session.input_revision,
            handled_input_seq: session.handled_input_seq,
            scope_snapshot: session.scope_snapshot,
            task_status_projection: session.task_status_projection,
            permission_requests: session.permission_requests,
            messages: session.conversation,
            context_notices: session.context_notices,
            context_attachments: session.context_attachments,
        }))
    }

    /// Record a complete owner decision and mint each approved scoped grant in
    /// the same transaction. A decision never reserves a use or dispatches a tool.
    pub async fn decide_permission_request(
        &self,
        conversation_id: &str,
        actor_id: &str,
        device_id: &str,
        request_id: &str,
        decisions: Vec<desk_diagnose_core::dynamic_run::PermissionDecisionItem>,
        grant_context: PermissionGrantIssuanceContext<'_>,
        now: &str,
    ) -> Result<desk_diagnose_core::dynamic_run::PermissionRequestState, AgentError> {
        for _ in 0..CLAIM_ATTEMPTS {
            let txn = self
                .db
                .begin()
                .await
                .map_err(|error| internal(format!("begin permission decision: {error}")))?;
            let Some(row) = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(conversation_id))
                .one(&txn)
                .await
                .map_err(|error| internal(format!("load permission decision run: {error}")))?
            else {
                txn.rollback().await.ok();
                return Err(internal("permission request run was not found"));
            };
            let mut session = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|error| internal(format!("decode permission decision run: {error}")))?;
            session.version = row.version;
            session
                .check_subject(actor_id, device_id)
                .map_err(|error| internal(format!("permission decision subject: {error:?}")))?;
            if session.turn_state.is_active() {
                txn.rollback().await.ok();
                return Err(transport(
                    "permission request is still being committed; retry",
                ));
            }
            let request_index = session
                .permission_requests
                .iter()
                .position(|request| request.request_id == request_id)
                .ok_or_else(|| internal("permission request was not found"))?;
            let requested = session.permission_requests[request_index].clone();
            let request = &mut session.permission_requests[request_index];
            if request.input_revision != session.input_revision {
                txn.rollback().await.ok();
                return Err(internal("permission request needs revalidation"));
            }
            let request_input_revision = request.input_revision;
            let resulting_state = request
                .apply_user_decision(&decisions)
                .map_err(|error| internal(format!("invalid permission decision: {error}")))?;
            let grants = build_permission_grants(&session, &requested, &decisions, &grant_context)?;
            session.last_event_seq = session
                .last_event_seq
                .checked_add(1)
                .ok_or_else(|| internal("permission decision event sequence exhausted"))?;
            session.updated_at = now.to_string();
            let event = desk_diagnose_core::dynamic_run::PermissionDecidedEvent {
                event: desk_diagnose_core::dynamic_run::AgentRunEvent {
                    schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                    event_id: stable_event_id(
                        "permission-decision",
                        &format!(
                            "{}:{}:{}",
                            session.conversation_id, session.last_event_seq, request_id
                        ),
                    ),
                    run_id: session.conversation_id.clone(),
                    event_seq: session.last_event_seq,
                    input_revision: request_input_revision,
                    kind: desk_diagnose_core::dynamic_run::AgentRunEventKind::PermissionDecided,
                    correlation_id: Some(request_id.to_string()),
                    source_envelope_ids: Vec::new(),
                    result_envelope_ids: Vec::new(),
                    created_at: now.to_string(),
                },
                request_id: request_id.to_string(),
                request_input_revision,
                resulting_state,
                items: decisions.clone(),
            };
            event
                .validate()
                .map_err(|error| internal(format!("invalid permission decision event: {error}")))?;

            let old_version = row.version;
            let new_version = old_version + 1;
            session.version = new_version;
            let state_json = session
                .encode_json_for_storage()
                .map_err(|error| internal(format!("encode permission decision run: {error}")))?;
            let result = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                .col_expr(agent_session::Column::Version, Expr::value(new_version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_from(now)))
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(old_version))
                .exec(&txn)
                .await
                .map_err(|error| internal(format!("save permission decision: {error}")))?;
            if result.rows_affected != 1 {
                txn.rollback().await.ok();
                continue;
            }
            agent_run_event::ActiveModel {
                event_id: Set(event.event.event_id.clone()),
                run_id: Set(event.event.run_id.clone()),
                event_seq: Set(i64::try_from(event.event.event_seq)
                    .map_err(|_| internal("permission decision sequence exceeds SQLite range"))?),
                input_revision: Set(i64::try_from(event.event.input_revision)
                    .map_err(|_| internal("permission decision revision exceeds SQLite range"))?),
                kind: Set(event.event.kind.as_str().into()),
                correlation_id: Set(event.event.correlation_id.clone()),
                input_seq: Set(None),
                actor_id: Set(Some(actor_id.to_string())),
                source_envelope_ids_json: Set("[]".into()),
                result_envelope_ids_json: Set("[]".into()),
                payload_json: Set(serde_json::to_string(&event).map_err(|error| {
                    internal(format!("encode permission decision event: {error}"))
                })?),
                payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
                created_at: Set(now_from(now)),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .map_err(|error| internal(format!("append permission decision event: {error}")))?;
            for grant in grants {
                grant
                    .validate()
                    .map_err(|error| internal(format!("invalid issued grant: {error}")))?;
                agent_capability_grant::ActiveModel {
                    grant_id: Set(grant.grant_id.clone()),
                    actor_id: Set(grant.actor_id.clone()),
                    run_id: Set(grant.run_id.clone()),
                    provider_id: Set(grant.provider_id.clone()),
                    tool_name: Set(grant.tool_name.clone()),
                    status: Set(crate::capability_grant_store::GRANT_STATUS_ACTIVE.into()),
                    remaining_uses: Set(i32::try_from(grant.remaining_uses)
                        .map_err(|_| internal("grant uses exceed SQLite range"))?),
                    payload_json: Set(serde_json::to_string(&grant)
                        .map_err(|error| internal(format!("encode issued grant: {error}")))?),
                    payload_schema_version: Set(i32::from(CAPABILITY_GRANT_SCHEMA_VERSION)),
                    version: Set(1),
                    created_at: Set(now_from(now)),
                    updated_at: Set(now_from(now)),
                    ..Default::default()
                }
                .insert(&txn)
                .await
                .map_err(|error| internal(format!("insert issued grant: {error}")))?;
            }
            txn.commit()
                .await
                .map_err(|error| internal(format!("commit permission decision: {error}")))?;
            return Ok(resulting_state);
        }
        Err(transport("permission decision conflicted; retry"))
    }

    pub async fn read_snapshot_for_subject(
        &self,
        conversation_id: &str,
        actor_id: &str,
        device_id: &str,
    ) -> Result<Option<SessionSnapshot>, AgentError> {
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(conversation_id))
            .filter(agent_session::Column::ActorId.eq(actor_id))
            .filter(agent_session::Column::DeviceId.eq(device_id))
            .one(&self.db)
            .await
            .map_err(|e| internal(format!("load agent session snapshot: {e}")))?;
        row.map(snapshot_from_row).transpose()
    }

    pub async fn list_device_assistant_sessions(
        &self,
        actor_id: &str,
        device_id: &str,
        limit: u64,
    ) -> Result<Vec<SessionSummary>, AgentError> {
        let rows = agent_session::Entity::find()
            .filter(agent_session::Column::ActorId.eq(actor_id))
            .filter(agent_session::Column::DeviceId.eq(device_id))
            .order_by_desc(agent_session::Column::UpdatedAt)
            .limit(limit.saturating_mul(4).max(limit))
            .all(&self.db)
            .await
            .map_err(|e| internal(format!("list agent sessions: {e}")))?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let session = match PersistedAgentSession::decode_json(&row.state_json) {
                    Ok(session) => session,
                    Err(error) => {
                        log::warn!(
                            "Skipping undecodable agent session history row id={}: {error}",
                            row.id
                        );
                        return None;
                    }
                };
                if session.surface != AgentSessionSurface::DeviceAssistant {
                    return None;
                }
                let first_question = session
                    .conversation
                    .iter()
                    .find(|message| message.role == desk_diagnose_core::chat::ChatRole::User)
                    .map(|message| message.text.clone());
                Some(SessionSummary {
                    session_id: row.conversation_id,
                    client_conversation_id: session.client_conversation_id,
                    first_question,
                    created_at: row.created_at.to_rfc3339(),
                    updated_at: row.updated_at.to_rfc3339(),
                    active: session.turn_state.is_active(),
                    message_count: session.conversation.len(),
                })
            })
            .take(limit as usize)
            .collect())
    }

    /// Recover one active session whose lease has expired without claiming a new
    /// turn. The retention sweep uses this before age deletion so a process crash
    /// cannot leave a row permanently protected as active.
    pub async fn settle_lapsed_session(
        &self,
        row: &agent_session::Model,
        now: DateTime<Utc>,
    ) -> Result<bool, AgentError> {
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|e| internal(format!("decode lapsed agent session: {e}")))?;
        if !session.turn_state.is_active()
            || row.lease_deadline.is_some_and(|deadline| deadline >= now)
        {
            return Ok(false);
        }

        let unclosed = session.unclosed_tool_call_ids();
        let task = find_recovery_task(&self.db, &session.conversation_id, &unclosed)
            .await
            .map_err(|e| internal(format!("load lapsed agent execution: {e}")))?;
        let now_text = now.to_rfc3339();
        match task {
            Some(task) => {
                session.recover_session(
                    RecoveryVerdict::OutcomeUnknown {
                        tool_call_id: task.tool_call_id.clone(),
                        action: ActionIdentity::agent_exec(
                            task.id,
                            task.exec_request_id.clone(),
                            task.execution_generation.clone(),
                        ),
                    },
                    now_text.clone(),
                );
                if task.status == crate::agent_exec_store::STATUS_DONE
                    && let Some(result_text) = task.result_text
                {
                    session.apply_completion(
                        &task.event_id,
                        &task.execution_generation,
                        &task.tool_call_id,
                        &task.exec_request_id,
                        result_text,
                        now_text.clone(),
                    );
                }
            }
            // No durable task means no mutating command was dispatched. This is
            // the normal crash window for a read-only tool between the persisted
            // assistant call and its result, so it is safe to close as not-run.
            None => session.recover_session(RecoveryVerdict::NotExecuted, now_text),
        }
        let new_version = row.version + 1;
        session.version = new_version;
        let state_json = session
            .encode_json_for_storage()
            .map_err(|e| internal(format!("encode lapsed agent session: {e}")))?;
        let result = agent_session::Entity::update_many()
            .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
            .col_expr(agent_session::Column::Version, Expr::value(new_version))
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Option::<DateTime<Utc>>::None),
            )
            .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .exec(&self.db)
            .await
            .map_err(|e| internal(format!("settle lapsed agent session: {e}")))?;
        Ok(result.rows_affected == 1)
    }

    /// Append a host execution result without taking over an active model turn.
    /// The task publisher retries `Busy`; `AlreadyPresent` makes crash replay
    /// idempotent through the stable event id.
    pub async fn deliver_completion(
        &self,
        conversation_id: &str,
        work_id: i64,
        event_id: &str,
        execution_id: &str,
        tool_call_id: &str,
        background_task_id: &str,
        result_text: &str,
        now: &str,
    ) -> Result<EventAppend, AgentError> {
        self.deliver_work_completion(
            conversation_id,
            work_id,
            WorkKind::AgentExec,
            event_id,
            execution_id,
            tool_call_id,
            background_task_id,
            result_text,
            now,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn deliver_work_completion(
        &self,
        conversation_id: &str,
        work_id: i64,
        work_kind: WorkKind,
        event_id: &str,
        execution_id: &str,
        tool_call_id: &str,
        background_task_id: &str,
        result_text: &str,
        now: &str,
    ) -> Result<EventAppend, AgentError> {
        self.deliver_work_completion_with_envelope(
            conversation_id,
            work_id,
            work_kind,
            event_id,
            execution_id,
            tool_call_id,
            background_task_id,
            result_text,
            None,
            now,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn deliver_work_completion_with_envelope(
        &self,
        conversation_id: &str,
        work_id: i64,
        work_kind: WorkKind,
        event_id: &str,
        execution_id: &str,
        tool_call_id: &str,
        background_task_id: &str,
        result_text: &str,
        result_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
        now: &str,
    ) -> Result<EventAppend, AgentError> {
        let now_dt = now_from(now);
        for _ in 0..CLAIM_ATTEMPTS {
            let Some(row) = find(&self.db, conversation_id)
                .await
                .map_err(|e| internal(format!("load completion session: {e}")))?
            else {
                return Ok(EventAppend::AlreadyPresent);
            };
            let mut session = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|e| internal(format!("decode completion session: {e}")))?;
            session.version = row.version;
            if session.turn_state.is_active()
                && row
                    .lease_deadline
                    .is_some_and(|deadline| deadline >= now_dt)
            {
                return Ok(EventAppend::Busy);
            }
            if !session.apply_completion_with_envelope(
                event_id,
                execution_id,
                tool_call_id,
                background_task_id,
                result_text,
                result_envelope.clone(),
                now,
            ) {
                return Ok(EventAppend::AlreadyPresent);
            }
            session.add_pending_auto_trigger(PendingAutoTrigger {
                work_id,
                kind: work_kind,
                execution_id: execution_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
                event_id: event_id.to_string(),
                chain_id: session.chain_id.clone(),
                resolution_org_id: None,
                since: now.to_string(),
            });
            let new_version = row.version + 1;
            session.version = new_version;
            let state_json = session
                .encode_json_for_storage()
                .map_err(|e| internal(format!("encode completion session: {e}")))?;
            let result = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                .col_expr(agent_session::Column::Version, Expr::value(new_version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_dt))
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(row.version))
                .exec(&self.db)
                .await
                .map_err(|e| internal(format!("save completion session: {e}")))?;
            if result.rows_affected == 1 {
                return Ok(EventAppend::Appended);
            }
        }
        Ok(EventAppend::Busy)
    }

    /// Load a session only while a particular completion is still waiting for an
    /// automatic model follow-up. The completion publisher keeps its durable task
    /// delivery pending until this entry is drained by a reacting turn.
    pub async fn pending_auto_trigger(
        &self,
        conversation_id: &str,
        event_id: &str,
    ) -> Result<Option<PersistedAgentSession>, AgentError> {
        let Some(row) = find(&self.db, conversation_id)
            .await
            .map_err(|e| internal(format!("load pending auto-follow-up: {e}")))?
        else {
            return Ok(None);
        };
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|e| internal(format!("decode pending auto-follow-up: {e}")))?;
        session.version = row.version;
        Ok(session
            .pending_auto_triggers
            .iter()
            .any(|pending| pending.event_id == event_id)
            .then_some(session))
    }

    /// Remove one pending automatic follow-up under the session version CAS.
    /// Active turns retain ownership; the publisher retries after they settle.
    pub async fn prune_auto_trigger(
        &self,
        conversation_id: &str,
        event_id: &str,
        now: &str,
    ) -> Result<EventAppend, AgentError> {
        let now_dt = now_from(now);
        for _ in 0..CLAIM_ATTEMPTS {
            let Some(row) = find(&self.db, conversation_id)
                .await
                .map_err(|e| internal(format!("load auto-follow-up prune: {e}")))?
            else {
                return Ok(EventAppend::AlreadyPresent);
            };
            let mut session = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|e| internal(format!("decode auto-follow-up prune: {e}")))?;
            session.version = row.version;
            if session.turn_state.is_active()
                && row
                    .lease_deadline
                    .is_some_and(|deadline| deadline >= now_dt)
            {
                return Ok(EventAppend::Busy);
            }
            if !session.remove_pending_auto_trigger(event_id) {
                return Ok(EventAppend::AlreadyPresent);
            }
            let new_version = row.version + 1;
            session.version = new_version;
            let state_json = session
                .encode_json_for_storage()
                .map_err(|e| internal(format!("encode auto-follow-up prune: {e}")))?;
            let result = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                .col_expr(agent_session::Column::Version, Expr::value(new_version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_dt))
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(row.version))
                .exec(&self.db)
                .await
                .map_err(|e| internal(format!("save auto-follow-up prune: {e}")))?;
            if result.rows_affected == 1 {
                return Ok(EventAppend::Appended);
            }
        }
        Ok(EventAppend::Busy)
    }

    /// Move a stranded running task to the core's recoverable unknown state.
    pub async fn mark_execution_unknown(
        &self,
        conversation_id: &str,
        execution_id: &str,
        tool_call_id: &str,
        now: &str,
    ) -> Result<EventAppend, AgentError> {
        let now_dt = now_from(now);
        for _ in 0..CLAIM_ATTEMPTS {
            let Some(row) = find(&self.db, conversation_id)
                .await
                .map_err(|e| internal(format!("load unknown execution session: {e}")))?
            else {
                return Ok(EventAppend::AlreadyPresent);
            };
            let mut session = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|e| internal(format!("decode unknown execution session: {e}")))?;
            session.version = row.version;
            if session.turn_state.is_active()
                && row
                    .lease_deadline
                    .is_some_and(|deadline| deadline >= now_dt)
            {
                return Ok(EventAppend::Busy);
            }
            if !session.mark_execution_unknown(execution_id, tool_call_id, now) {
                return Ok(EventAppend::AlreadyPresent);
            }
            let new_version = row.version + 1;
            session.version = new_version;
            let state_json = session
                .encode_json_for_storage()
                .map_err(|e| internal(format!("encode unknown execution session: {e}")))?;
            let result = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                .col_expr(agent_session::Column::Version, Expr::value(new_version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_dt))
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(row.version))
                .exec(&self.db)
                .await
                .map_err(|e| internal(format!("save unknown execution session: {e}")))?;
            if result.rows_affected == 1 {
                return Ok(EventAppend::Appended);
            }
        }
        Ok(EventAppend::Busy)
    }

    /// Clear the exact recoverable unknown state after its durable action row has
    /// recorded an owner disposition. Subject filters and the work/execution pair
    /// are repeated under the session version CAS; this never retries the action.
    pub async fn manually_dispose_unknown_for_subject(
        &self,
        conversation_id: &str,
        actor_id: &str,
        device_id: &str,
        work_id: i64,
        execution_id: &str,
        now: &str,
    ) -> Result<EventAppend, AgentError> {
        let now_dt = now_from(now);
        for _ in 0..CLAIM_ATTEMPTS {
            let Some(row) = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(conversation_id))
                .filter(agent_session::Column::ActorId.eq(actor_id))
                .filter(agent_session::Column::DeviceId.eq(device_id))
                .one(&self.db)
                .await
                .map_err(|error| internal(format!("load manual disposition session: {error}")))?
            else {
                return Ok(EventAppend::AlreadyPresent);
            };
            let mut session = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|error| internal(format!("decode manual disposition session: {error}")))?;
            session.version = row.version;
            if session.turn_state.is_active()
                && row
                    .lease_deadline
                    .is_some_and(|deadline| deadline >= now_dt)
            {
                return Ok(EventAppend::Busy);
            }
            if !session.manually_dispose_unknown(work_id, execution_id, now) {
                return Ok(EventAppend::AlreadyPresent);
            }
            let new_version = row.version + 1;
            session.version = new_version;
            let state_json = session
                .encode_json_for_storage()
                .map_err(|error| internal(format!("encode manual disposition session: {error}")))?;
            let result = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                .col_expr(agent_session::Column::Version, Expr::value(new_version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_dt))
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(row.version))
                .exec(&self.db)
                .await
                .map_err(|error| internal(format!("save manual disposition session: {error}")))?;
            if result.rows_affected == 1 {
                return Ok(EventAppend::Appended);
            }
        }
        Ok(EventAppend::Busy)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAppend {
    Appended,
    AlreadyPresent,
    Busy,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub seq: i64,
    pub active: bool,
    pub request_id: Option<String>,
    pub active_execution_generation: Option<String>,
    pub unresolved_action: Option<ActionIdentity>,
    pub latest_input_seq: u64,
    pub input_revision: u64,
    pub handled_input_seq: u64,
    pub scope_snapshot: AgentScope,
    pub task_status_projection: Option<desk_diagnose_core::dynamic_run::TaskStatusProjection>,
    pub permission_requests: Vec<desk_diagnose_core::dynamic_run::PermissionRequest>,
    pub messages: Vec<desk_diagnose_core::chat::ChatMessage>,
    pub context_notices: Vec<desk_diagnose_core::model_context::ContextNotice>,
    pub context_attachments: Vec<ContextAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub client_conversation_id: Option<String>,
    pub first_question: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub active: bool,
    pub message_count: usize,
}

fn snapshot_from_row(row: agent_session::Model) -> Result<SessionSnapshot, AgentError> {
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|e| internal(format!("decode agent session snapshot: {e}")))?;
    let active_execution_generation = session
        .execution_state
        .waitable_task()
        .map(|action| action.execution_id.clone());
    let unresolved_action = match &session.execution_state {
        ExecutionState::OutcomeUnknown { action, .. } => Some(action.clone()),
        _ => None,
    };
    Ok(SessionSnapshot {
        seq: row.version,
        active: session.turn_state.is_active(),
        request_id: session.current_request_id,
        active_execution_generation,
        unresolved_action,
        latest_input_seq: session.latest_input_seq,
        input_revision: session.input_revision,
        handled_input_seq: session.handled_input_seq,
        scope_snapshot: session.scope_snapshot,
        task_status_projection: session.task_status_projection,
        permission_requests: session.permission_requests,
        messages: session.conversation,
        context_notices: session.context_notices,
        context_attachments: session.context_attachments,
    })
}

fn apply_object_mutation(
    session: &mut PersistedAgentSession,
    mutation: &ObjectContextMutation,
) -> Result<bool, AgentError> {
    match mutation {
        ObjectContextMutation::Attach(attachment) => {
            if session.context_attachments.iter().any(|existing| {
                matches!(existing.state, AttachmentState::Active)
                    && existing.object_ref.source_provider_id
                        == attachment.object_ref.source_provider_id
                    && existing.object_ref.source_capability_id
                        == attachment.object_ref.source_capability_id
                    && existing.object_ref.object_incarnation
                        == attachment.object_ref.object_incarnation
            }) {
                return Ok(false);
            }
            session
                .attach_context(attachment.clone())
                .map_err(|error| internal(format!("attach Device Assistant object: {error}")))
        }
        ObjectContextMutation::Detach { attachment_id } => {
            if !session
                .context_attachments
                .iter()
                .any(|attachment| attachment.attachment_id == *attachment_id)
            {
                return Err(transport("Device Assistant attachment does not exist"));
            }
            Ok(session.detach_context(attachment_id))
        }
        ObjectContextMutation::Refresh {
            stale_attachment_id,
            replacement,
        } => session
            .refresh_context(
                stale_attachment_id,
                AttachmentStaleReason::ObjectChanged,
                replacement.clone(),
            )
            .map_err(|error| internal(format!("refresh Device Assistant object: {error}"))),
    }
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

fn now_from(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|v| v.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

async fn find(
    db: &DatabaseConnection,
    conversation_id: &str,
) -> Result<Option<agent_session::Model>, sea_orm::DbErr> {
    agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .one(db)
        .await
}

/// Find the newest durable execution correlated to any still-unclosed call. The
/// loop persists each completed read-only result before advancing, so at most one
/// of these calls can have an in-flight mutating task; the explicit tool-call id
/// carried into recovery prevents binding its identity to a different call.
async fn find_recovery_task(
    db: &DatabaseConnection,
    conversation_id: &str,
    unclosed_tool_call_ids: &[String],
) -> Result<Option<agent_exec_task::Model>, sea_orm::DbErr> {
    if unclosed_tool_call_ids.is_empty() {
        return Ok(None);
    }
    agent_exec_task::Entity::find()
        .filter(agent_exec_task::Column::ConversationId.eq(conversation_id))
        .filter(agent_exec_task::Column::ToolCallId.is_in(unclosed_tool_call_ids.to_vec()))
        .order_by_desc(agent_exec_task::Column::Id)
        .one(db)
        .await
}

#[async_trait(?Send)]
impl SessionSeam for SignalAgentSessionStore {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let now = now_from(&params.now);
        for _ in 0..CLAIM_ATTEMPTS {
            match find(&self.db, &params.conversation_id)
                .await
                .map_err(|e| ClaimError::Backend(internal(format!("load agent session: {e}"))))?
            {
                Some(row) => {
                    let mut session =
                        PersistedAgentSession::decode_json(&row.state_json).map_err(|e| {
                            ClaimError::Backend(internal(format!(
                                "decode agent session state: {e}"
                            )))
                        })?;
                    session.version = row.version;
                    session
                        .check_subject(&params.actor_id, &params.device_id)
                        .map_err(ClaimError::Subject)?;
                    session
                        .check_surface(self.surface)
                        .map_err(ClaimError::Subject)?;
                    session.adopt_client_metadata(
                        self.client_conversation_id.as_deref(),
                        self.surface,
                    );
                    self.reconcile_context_selection(&mut session)
                        .map_err(ClaimError::Backend)?;

                    if session.turn_state.is_active() {
                        let lease_live = row.lease_deadline.is_some_and(|d| d >= now);
                        if lease_live {
                            return Err(ClaimError::Busy);
                        }
                        // Correlate an interrupted mutating call with Signal's
                        // durable task row. A task identity keeps the outcome
                        // reconcilable across a process restart; a terminal result
                        // already in SQLite can be applied immediately.
                        let unclosed = session.unclosed_tool_call_ids();
                        let task =
                            find_recovery_task(&self.db, &session.conversation_id, &unclosed)
                                .await
                                .map_err(|e| {
                                    ClaimError::Backend(internal(format!(
                                        "load interrupted agent execution: {e}"
                                    )))
                                })?;
                        match task {
                            Some(task) => {
                                session.recover_session(
                                    RecoveryVerdict::OutcomeUnknown {
                                        tool_call_id: task.tool_call_id.clone(),
                                        action: ActionIdentity::agent_exec(
                                            task.id,
                                            task.exec_request_id.clone(),
                                            task.execution_generation.clone(),
                                        ),
                                    },
                                    params.now.clone(),
                                );
                                if task.status == crate::agent_exec_store::STATUS_DONE
                                    && let Some(result_text) = task.result_text
                                {
                                    session.apply_completion(
                                        &task.event_id,
                                        &task.execution_generation,
                                        &task.tool_call_id,
                                        &task.exec_request_id,
                                        result_text,
                                        params.now.clone(),
                                    );
                                }
                            }
                            None => session
                                .recover_session(RecoveryVerdict::NotExecuted, params.now.clone()),
                        }
                    }
                    if matches!(
                        session.begin_turn(
                            params.turn_id.clone(),
                            params.request_id.clone(),
                            params.connection_id.clone(),
                            params.policy_revision,
                            params.current_pdp_scope.clone(),
                            params.now.clone(),
                        ),
                        Err(TurnClaimError::Busy)
                    ) {
                        return Err(ClaimError::Busy);
                    }
                    session.adopt_trigger(params.trigger_origin, &params.turn_id);
                    let old_version = row.version;
                    let new_version = old_version + 1;
                    session.version = new_version;
                    let state_json = session.encode_json_for_storage().map_err(|e| {
                        ClaimError::Backend(internal(format!("encode agent session state: {e}")))
                    })?;
                    let result = agent_session::Entity::update_many()
                        .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                        .col_expr(agent_session::Column::Version, Expr::value(new_version))
                        .col_expr(
                            agent_session::Column::LeaseToken,
                            Expr::value(session.lease_token as i64),
                        )
                        .col_expr(
                            agent_session::Column::LeaseDeadline,
                            Expr::value(Some(now + Duration::seconds(LEASE_TTL_SECS))),
                        )
                        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
                        .filter(agent_session::Column::Id.eq(row.id))
                        .filter(agent_session::Column::Version.eq(old_version))
                        .exec(&self.db)
                        .await
                        .map_err(|e| {
                            ClaimError::Backend(internal(format!("claim agent session: {e}")))
                        })?;
                    if result.rows_affected == 1 {
                        return Ok(session);
                    }
                }
                None => {
                    let mut session = PersistedAgentSession::new(
                        params.conversation_id.clone(),
                        params.actor_id.clone(),
                        params.device_id.clone(),
                        params.policy_revision,
                        params.current_pdp_scope.clone(),
                        params.now.clone(),
                    );
                    session.adopt_client_metadata(
                        self.client_conversation_id.as_deref(),
                        self.surface,
                    );
                    self.reconcile_context_selection(&mut session)
                        .map_err(ClaimError::Backend)?;
                    let _ = session.begin_turn(
                        params.turn_id.clone(),
                        params.request_id.clone(),
                        params.connection_id.clone(),
                        params.policy_revision,
                        params.current_pdp_scope.clone(),
                        params.now.clone(),
                    );
                    session.adopt_trigger(params.trigger_origin, &params.turn_id);
                    let state_json = session.encode_json_for_storage().map_err(|e| {
                        ClaimError::Backend(internal(format!("encode agent session state: {e}")))
                    })?;
                    let inserted = agent_session::ActiveModel {
                        conversation_id: Set(session.conversation_id.clone()),
                        actor_id: Set(session.actor_id.clone()),
                        device_id: Set(session.device_id.clone()),
                        state_json: Set(state_json),
                        version: Set(0),
                        lease_token: Set(session.lease_token as i64),
                        lease_deadline: Set(Some(now + Duration::seconds(LEASE_TTL_SECS))),
                        created_at: Set(now),
                        updated_at: Set(now),
                        ..Default::default()
                    }
                    .insert(&self.db)
                    .await;
                    match inserted {
                        Ok(_) => return Ok(session),
                        Err(_)
                            if find(&self.db, &params.conversation_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some() =>
                        {
                            continue;
                        }
                        Err(e) => {
                            return Err(ClaimError::Backend(internal(format!(
                                "create agent session: {e}"
                            ))));
                        }
                    }
                }
            }
        }
        Err(ClaimError::Busy)
    }

    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        let now = Utc::now();
        let old_version = session.version;
        let new_version = old_version + 1;
        desk_diagnose_core::image_input::retain_latest_session_image(&mut session.conversation)
            .map_err(|error| internal(format!("invalid session image: {error}")))?;
        let mut stored = session.clone();
        // Images are a one-turn model-egress projection, never durable session
        // state. Keep the validated image in the live in-memory session so the
        // next model step can consume it, but persist only the image-free view.
        desk_diagnose_core::image_input::strip_session_images(&mut stored.conversation);
        stored.version = new_version;
        let state_json = stored
            .encode_json_for_storage()
            .map_err(|e| internal(format!("encode agent session state: {e}")))?;
        let lease_deadline = session
            .turn_state
            .is_active()
            .then_some(now + Duration::seconds(LEASE_TTL_SECS));
        let result = agent_session::Entity::update_many()
            .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
            .col_expr(agent_session::Column::Version, Expr::value(new_version))
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(lease_deadline),
            )
            .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .filter(agent_session::Column::Version.eq(old_version))
            .filter(agent_session::Column::LeaseToken.eq(session.lease_token as i64))
            .exec(&self.db)
            .await
            .map_err(|e| internal(format!("save agent session: {e}")))?;
        if result.rows_affected != 1 {
            return Err(internal("agent session lease or version was lost"));
        }
        session.version = new_version;
        Ok(())
    }

    async fn save_task_status_update(
        &self,
        session: &mut PersistedAgentSession,
        update: &TaskStatusUpdatedEvent,
    ) -> Result<(), AgentError> {
        update
            .validate()
            .map_err(|error| internal(format!("invalid task-status event: {error}")))?;
        if update.event.run_id != session.conversation_id
            || update.event.event_seq != session.last_event_seq
            || update.event.input_revision != session.input_revision
            || session.task_status_projection.as_ref() != Some(&update.projection)
        {
            return Err(internal("task-status event does not match session state"));
        }

        let txn = self
            .db
            .begin()
            .await
            .map_err(|error| internal(format!("begin task-status transaction: {error}")))?;
        let now = Utc::now();
        let old_version = session.version;
        let new_version = old_version + 1;
        desk_diagnose_core::image_input::retain_latest_session_image(&mut session.conversation)
            .map_err(|error| internal(format!("invalid session image: {error}")))?;
        let mut stored = session.clone();
        desk_diagnose_core::image_input::strip_session_images(&mut stored.conversation);
        stored.version = new_version;
        let state_json = stored
            .encode_json_for_storage()
            .map_err(|error| internal(format!("encode agent session state: {error}")))?;
        let lease_deadline = session
            .turn_state
            .is_active()
            .then_some(now + Duration::seconds(LEASE_TTL_SECS));
        let result = agent_session::Entity::update_many()
            .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
            .col_expr(agent_session::Column::Version, Expr::value(new_version))
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(lease_deadline),
            )
            .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .filter(agent_session::Column::Version.eq(old_version))
            .filter(agent_session::Column::LeaseToken.eq(session.lease_token as i64))
            .exec(&txn)
            .await
            .map_err(|error| internal(format!("save task-status session: {error}")))?;
        if result.rows_affected != 1 {
            txn.rollback().await.ok();
            return Err(internal("agent session lease or version was lost"));
        }

        let event_seq = i64::try_from(update.event.event_seq)
            .map_err(|_| internal("task-status event sequence exceeds SQLite range"))?;
        let input_revision = i64::try_from(update.event.input_revision)
            .map_err(|_| internal("task-status input revision exceeds SQLite range"))?;
        let payload_json = serde_json::to_string(update)
            .map_err(|error| internal(format!("encode task-status event: {error}")))?;
        agent_run_event::ActiveModel {
            event_id: Set(update.event.event_id.clone()),
            run_id: Set(update.event.run_id.clone()),
            event_seq: Set(event_seq),
            input_revision: Set(input_revision),
            kind: Set(update.event.kind.as_str().into()),
            correlation_id: Set(update.event.correlation_id.clone()),
            input_seq: Set(None),
            actor_id: Set(Some(session.actor_id.clone())),
            source_envelope_ids_json: Set(serde_json::to_string(&update.event.source_envelope_ids)
                .map_err(|error| {
                    internal(format!("encode task-status source envelopes: {error}"))
                })?),
            result_envelope_ids_json: Set(serde_json::to_string(&update.event.result_envelope_ids)
                .map_err(|error| {
                    internal(format!("encode task-status result envelopes: {error}"))
                })?),
            payload_json: Set(payload_json),
            payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
            created_at: Set(now_from(&update.event.created_at)),
            ..Default::default()
        }
        .insert(&txn)
        .await
        .map_err(|error| internal(format!("append task-status event: {error}")))?;
        txn.commit()
            .await
            .map_err(|error| internal(format!("commit task-status transaction: {error}")))?;
        session.version = new_version;
        Ok(())
    }

    async fn save_permission_request(
        &self,
        session: &mut PersistedAgentSession,
        update: &PermissionRequestedEvent,
    ) -> Result<(), AgentError> {
        update
            .validate()
            .map_err(|error| internal(format!("invalid permission event: {error}")))?;
        if update.event.run_id != session.conversation_id
            || update.event.event_seq != session.last_event_seq
            || update.event.input_revision != session.input_revision
            || !session
                .permission_requests
                .iter()
                .any(|request| request == &update.request)
        {
            return Err(internal("permission event does not match session state"));
        }

        let txn = self
            .db
            .begin()
            .await
            .map_err(|error| internal(format!("begin permission transaction: {error}")))?;
        let now = Utc::now();
        let old_version = session.version;
        let new_version = old_version + 1;
        desk_diagnose_core::image_input::retain_latest_session_image(&mut session.conversation)
            .map_err(|error| internal(format!("invalid session image: {error}")))?;
        let mut stored = session.clone();
        desk_diagnose_core::image_input::strip_session_images(&mut stored.conversation);
        stored.version = new_version;
        let state_json = stored
            .encode_json_for_storage()
            .map_err(|error| internal(format!("encode agent session state: {error}")))?;
        let lease_deadline = session
            .turn_state
            .is_active()
            .then_some(now + Duration::seconds(LEASE_TTL_SECS));
        let result = agent_session::Entity::update_many()
            .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
            .col_expr(agent_session::Column::Version, Expr::value(new_version))
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(lease_deadline),
            )
            .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .filter(agent_session::Column::Version.eq(old_version))
            .filter(agent_session::Column::LeaseToken.eq(session.lease_token as i64))
            .exec(&txn)
            .await
            .map_err(|error| internal(format!("save permission session: {error}")))?;
        if result.rows_affected != 1 {
            txn.rollback().await.ok();
            return Err(internal("agent session lease or version was lost"));
        }

        agent_run_event::ActiveModel {
            event_id: Set(update.event.event_id.clone()),
            run_id: Set(update.event.run_id.clone()),
            event_seq: Set(i64::try_from(update.event.event_seq)
                .map_err(|_| internal("permission event sequence exceeds SQLite range"))?),
            input_revision: Set(i64::try_from(update.event.input_revision)
                .map_err(|_| internal("permission input revision exceeds SQLite range"))?),
            kind: Set(update.event.kind.as_str().into()),
            correlation_id: Set(update.event.correlation_id.clone()),
            input_seq: Set(None),
            actor_id: Set(Some(session.actor_id.clone())),
            source_envelope_ids_json: Set(serde_json::to_string(&update.event.source_envelope_ids)
                .map_err(|error| {
                    internal(format!("encode permission source envelopes: {error}"))
                })?),
            result_envelope_ids_json: Set(serde_json::to_string(&update.event.result_envelope_ids)
                .map_err(|error| {
                    internal(format!("encode permission result envelopes: {error}"))
                })?),
            payload_json: Set(serde_json::to_string(update)
                .map_err(|error| internal(format!("encode permission event: {error}")))?),
            payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
            created_at: Set(now_from(&update.event.created_at)),
            ..Default::default()
        }
        .insert(&txn)
        .await
        .map_err(|error| internal(format!("append permission event: {error}")))?;
        txn.commit()
            .await
            .map_err(|error| internal(format!("commit permission transaction: {error}")))?;
        session.version = new_version;
        Ok(())
    }

    async fn latest_input_revision(
        &self,
        conversation_id: &str,
    ) -> Result<Option<u64>, AgentError> {
        let Some(row) = find(&self.db, conversation_id)
            .await
            .map_err(|error| internal(format!("load current input revision: {error}")))?
        else {
            return Ok(None);
        };
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|error| internal(format!("decode current input revision: {error}")))?;
        Ok(Some(session.input_revision))
    }

    async fn settle_superseded(
        &self,
        stale_session: &PersistedAgentSession,
        now: &str,
    ) -> Result<bool, AgentError> {
        for _ in 0..CLAIM_ATTEMPTS {
            let txn = self
                .db
                .begin()
                .await
                .map_err(|error| internal(format!("begin superseded transaction: {error}")))?;
            let Some(row) = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&stale_session.conversation_id))
                .one(&txn)
                .await
                .map_err(|error| internal(format!("load superseded session: {error}")))?
            else {
                txn.rollback().await.ok();
                return Ok(false);
            };
            let mut current = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|error| internal(format!("decode superseded session: {error}")))?;
            current.version = row.version;
            if current.input_revision <= stale_session.input_revision
                || row.lease_token != stale_session.lease_token as i64
                || !current.turn_state.is_active()
            {
                txn.rollback().await.ok();
                return Ok(false);
            }
            if current.surface != AgentSessionSurface::DeviceAssistant
                || !matches!(current.execution_state, ExecutionState::None)
            {
                txn.rollback().await.ok();
                return Err(internal(
                    "only read-only Device Assistant turns may be superseded in Stage 2",
                ));
            }

            // New user messages were appended to the authoritative row while the
            // old owner was running. Temporarily remove them so any completed read
            // results can close their assistant tool calls before the new input.
            let stale_ids = stale_session
                .conversation
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let mut pending_user_messages = Vec::new();
            current.conversation.retain(|message| {
                let pending = message.role == desk_diagnose_core::chat::ChatRole::User
                    && !stale_ids.contains(message.message_id.as_str());
                if pending {
                    pending_user_messages.push(message.clone());
                }
                !pending
            });

            let known_call_ids = current
                .conversation
                .iter()
                .flat_map(|message| message.tool_calls.iter().map(|call| call.id.as_str()))
                .collect::<std::collections::BTreeSet<_>>();
            let current_message_ids = current
                .conversation
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let mut merged_results = stale_session
                .conversation
                .iter()
                .filter(|message| {
                    message.role == desk_diagnose_core::chat::ChatRole::Tool
                        && !current_message_ids.contains(message.message_id.as_str())
                        && message
                            .tool_call_id
                            .as_deref()
                            .is_some_and(|call_id| known_call_ids.contains(call_id))
                })
                .cloned()
                .collect::<Vec<_>>();
            current.conversation.append(&mut merged_results);

            let open_calls = current.unclosed_tool_call_ids();
            for call_id in open_calls {
                let parent = current
                    .conversation
                    .iter()
                    .find(|message| message.tool_calls.iter().any(|call| call.id == call_id))
                    .and_then(|message| message.data_envelope.clone());
                current
                    .conversation
                    .push(superseded_tool_result(&call_id, parent.as_ref())?);
            }
            current.conversation.append(&mut pending_user_messages);
            current.last_event_seq = current
                .last_event_seq
                .checked_add(1)
                .ok_or_else(|| internal("superseded event sequence exhausted"))?;
            current.finish_turn(TurnState::Idle, now.to_string());
            let source_envelope_ids = current
                .conversation
                .iter()
                .filter(|message| {
                    message.role == desk_diagnose_core::chat::ChatRole::User
                        && !stale_ids.contains(message.message_id.as_str())
                })
                .filter_map(|message| {
                    message
                        .data_envelope
                        .as_ref()
                        .map(|envelope| envelope.envelope_id.clone())
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let superseded = desk_diagnose_core::dynamic_run::AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: stable_event_id(
                    "superseded",
                    &format!(
                        "{}:{}:{}",
                        current.conversation_id, current.last_event_seq, stale_session.lease_token
                    ),
                ),
                run_id: current.conversation_id.clone(),
                event_seq: current.last_event_seq,
                input_revision: current.input_revision,
                kind: desk_diagnose_core::dynamic_run::AgentRunEventKind::Superseded,
                correlation_id: stale_session.current_turn_id.clone(),
                source_envelope_ids,
                result_envelope_ids: Vec::new(),
                created_at: now.to_string(),
            };
            superseded
                .validate()
                .map_err(|error| internal(format!("invalid superseded event: {error}")))?;

            let old_version = row.version;
            let new_version = old_version + 1;
            let mut stored = current;
            desk_diagnose_core::image_input::strip_session_images(&mut stored.conversation);
            stored.version = new_version;
            let state_json = stored
                .encode_json_for_storage()
                .map_err(|error| internal(format!("encode superseded session: {error}")))?;
            let result = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
                .col_expr(agent_session::Column::Version, Expr::value(new_version))
                .col_expr(
                    agent_session::Column::LeaseDeadline,
                    Expr::value(None::<DateTime<Utc>>),
                )
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now_from(now)))
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(old_version))
                .filter(agent_session::Column::LeaseToken.eq(stale_session.lease_token as i64))
                .exec(&txn)
                .await
                .map_err(|error| internal(format!("save superseded session: {error}")))?;
            if result.rows_affected != 1 {
                txn.rollback().await.ok();
                continue;
            }
            agent_run_event::ActiveModel {
                event_id: Set(superseded.event_id.clone()),
                run_id: Set(superseded.run_id.clone()),
                event_seq: Set(i64::try_from(superseded.event_seq)
                    .map_err(|_| internal("superseded event sequence exceeds SQLite range"))?),
                input_revision: Set(i64::try_from(superseded.input_revision)
                    .map_err(|_| internal("superseded input revision exceeds SQLite range"))?),
                kind: Set(superseded.kind.as_str().into()),
                correlation_id: Set(superseded.correlation_id.clone()),
                input_seq: Set(None),
                actor_id: Set(Some(stale_session.actor_id.clone())),
                source_envelope_ids_json: Set(serde_json::to_string(
                    &superseded.source_envelope_ids,
                )
                .map_err(|error| {
                    internal(format!("encode superseded source envelopes: {error}"))
                })?),
                result_envelope_ids_json: Set("[]".into()),
                payload_json: Set(serde_json::to_string(&superseded)
                    .map_err(|error| internal(format!("encode superseded event: {error}")))?),
                payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
                created_at: Set(now_from(now)),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .map_err(|error| internal(format!("append superseded event: {error}")))?;
            txn.commit()
                .await
                .map_err(|error| internal(format!("commit superseded transaction: {error}")))?;
            return Ok(true);
        }
        Err(transport("superseded turn conflicted; retry"))
    }

    async fn heartbeat(
        &self,
        conversation_id: &str,
        lease_token: u64,
        now: &str,
    ) -> Result<(), AgentError> {
        let deadline = now_from(now) + Duration::seconds(LEASE_TTL_SECS);
        let result = agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Some(deadline)),
            )
            .filter(agent_session::Column::ConversationId.eq(conversation_id))
            .filter(agent_session::Column::LeaseToken.eq(lease_token as i64))
            .exec(&self.db)
            .await
            .map_err(|e| internal(format!("renew agent session: {e}")))?;
        if result.rows_affected != 1 {
            return Err(internal("agent session lease was lost"));
        }
        Ok(())
    }
}

fn stable_event_id(prefix: &str, value: &str) -> String {
    format!("{prefix}-{:x}", Sha256::digest(value.as_bytes()))
}

fn superseded_tool_result(
    call_id: &str,
    parent: Option<&desk_agent_protocol::data_lineage::DataEnvelope>,
) -> Result<desk_diagnose_core::chat::ChatMessage, AgentError> {
    const CONTENT: &str = "not executed: superseded by newer user input";
    let mut message = desk_diagnose_core::chat::ChatMessage::tool_result(
        stable_event_id("superseded-result-message", call_id),
        call_id,
        CONTENT,
    );
    let Some(parent) = parent else {
        return Ok(message);
    };
    let digest = format!("{:x}", Sha256::digest(CONTENT.as_bytes()));
    let envelope = desk_agent_protocol::data_lineage::DataEnvelope {
        schema_version: desk_agent_protocol::data_lineage::DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: stable_event_id(
            "superseded-result",
            &format!("{}:{call_id}:{digest}", parent.envelope_id),
        ),
        content: desk_agent_protocol::data_lineage::ContentRef::ImmutableBlob {
            blob_id: stable_event_id("superseded-content", &digest),
            sha256: digest.clone(),
            size_bytes: CONTENT.len() as u64,
            media_type: "text/plain".into(),
        },
        provenance: desk_agent_protocol::data_lineage::DataProvenance {
            source_provider_id: "assistant.run_control".into(),
            source_tool_name: "supersede_tool_call".into(),
            source_object_id: None,
            source_envelope_ids: vec![parent.envelope_id.clone()],
        },
        digest_sha256: digest,
        sensitivity: parent.sensitivity,
        allowed_destinations: parent.allowed_destinations.clone(),
        retention: parent.retention,
    };
    envelope
        .validate()
        .map_err(|error| internal(format!("invalid superseded result envelope: {error}")))?;
    message.data_envelope = Some(envelope);
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::capability_grant::{CapabilityGrant, CapabilityGrantUsePolicy};
    use desk_agent_protocol::capability_provider::{CapabilityEffect, ProductSurface};
    use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    };
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::capability_availability::CapabilityAvailability;
    use desk_diagnose_core::context_attachment::{
        AttachmentBounds, AttachmentObjectRef, AttachmentStaleReason,
        CONTEXT_ATTACHMENT_SCHEMA_VERSION, ContextAttachmentKind,
    };
    use desk_diagnose_core::dynamic_run::{
        AgentRunEvent, AgentRunEventKind, GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION,
        PermissionDecisionItem, PermissionItemDecision, PermissionRequest, PermissionRequestState,
        PermissionRequestedEvent, TASK_STATUS_PROJECTION_SCHEMA_VERSION, TaskStatus,
        TaskStatusItem, TaskStatusProjection,
    };
    use desk_diagnose_core::session::{ExecutionState, TriggerOrigin, TurnState};
    use sea_orm::{ConnectionTrait, Database, Schema};

    use crate::agent_run_event_store::{AppendUserFollowupParams, SignalAgentRunEventStore};

    async fn store() -> SignalAgentSessionStore {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(agent_session::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(agent_exec_task::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(agent_run_event::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(agent_capability_grant::Entity))
            .await
            .unwrap();
        SignalAgentSessionStore::new(db)
    }

    fn claim(turn_id: &str) -> ClaimTurnParams {
        ClaimTurnParams {
            conversation_id: "conversation-1".into(),
            actor_id: "1".into(),
            device_id: "device-1".into(),
            policy_revision: 0,
            current_pdp_scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            turn_id: turn_id.into(),
            request_id: Some(format!("request-{turn_id}")),
            connection_id: Some("browser-1".into()),
            trigger_origin: TriggerOrigin::User,
            now: Utc::now().to_rfc3339(),
        }
    }

    fn followup(event_id: &str, message_id: &str, text: &str) -> AppendUserFollowupParams {
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{message_id}"),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("message-{message_id}"),
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
        let mut message = desk_diagnose_core::chat::ChatMessage::text(
            message_id,
            desk_diagnose_core::chat::ChatRole::User,
            text,
        );
        message.data_envelope = Some(envelope);
        AppendUserFollowupParams {
            event_id: event_id.into(),
            run_id: "conversation-1".into(),
            client_conversation_id: Some("client-conversation-1".into()),
            actor_id: "1".into(),
            device_id: "device-1".into(),
            surface: AgentSessionSurface::DeviceAssistant,
            policy_revision: 0,
            current_scope: claim("scope").current_pdp_scope,
            message,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    #[tokio::test]
    async fn task_status_projection_and_event_commit_together() {
        let store = store().await;
        let mut session = store.claim_turn(claim("status-turn")).await.unwrap();
        session.input_revision = 1;
        session.last_event_seq = 1;
        let projection = TaskStatusProjection {
            schema_version: TASK_STATUS_PROJECTION_SCHEMA_VERSION,
            revision: 1,
            items: vec![TaskStatusItem {
                item_id: "inspect".into(),
                description: "Inspect the workbook".into(),
                status: TaskStatus::InProgress,
                note: None,
                last_updated_step_id: "step-1".into(),
            }],
            updated_at: "2026-08-26T00:00:00Z".into(),
        };
        session.task_status_projection = Some(projection.clone());
        let update = TaskStatusUpdatedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "status-event-1".into(),
                run_id: session.conversation_id.clone(),
                event_seq: 1,
                input_revision: 1,
                kind: AgentRunEventKind::TaskStatusUpdated,
                correlation_id: Some("call-1".into()),
                source_envelope_ids: Vec::new(),
                result_envelope_ids: Vec::new(),
                created_at: "2026-08-26T00:00:00Z".into(),
            },
            projection: projection.clone(),
        };

        store
            .save_task_status_update(&mut session, &update)
            .await
            .unwrap();

        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq("conversation-1"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let stored = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        assert_eq!(stored.task_status_projection, Some(projection));
        let events = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("conversation-1"))
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "task_status_updated");
        assert_eq!(events[0].event_seq, 1);
    }

    #[tokio::test]
    async fn permission_request_event_is_atomic_and_new_input_fences_approval() {
        let store = store().await.with_client_metadata(
            Some("client-conversation-1".into()),
            AgentSessionSurface::DeviceAssistant,
        );
        let events = SignalAgentRunEventStore::new(store.db.clone());
        events
            .append_user_followup(followup("followup-1", "user-1", "first"))
            .await
            .unwrap();
        let mut session = store.claim_turn(claim("permission-turn")).await.unwrap();
        let request = PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "permission-1".into(),
            input_revision: 1,
            state: PermissionRequestState::Pending,
            items: vec![GrantRequestItem {
                item_id: "inspect".into(),
                provider_id: "desktop.session".into(),
                tool_name: "inspect_desktop_session".into(),
                expected_effect: CapabilityEffect::ReadDevice,
                resource_scope: vec!["target:device-1".into()],
                operation_scope: Vec::new(),
                export_destinations: Vec::new(),
                canonical_input_json: None,
                canonical_input_digest_sha256: None,
                suggested_ttl_seconds: 300,
                suggested_max_uses: 1,
                reason: "Inspect the requested target".into(),
            }],
            created_at: "2026-08-26T00:00:00Z".into(),
        };
        session.add_permission_request(request.clone()).unwrap();
        session.last_event_seq = 2;
        let update = PermissionRequestedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "permission-event-1".into(),
                run_id: session.conversation_id.clone(),
                event_seq: 2,
                input_revision: 1,
                kind: AgentRunEventKind::PermissionRequested,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: Vec::new(),
                result_envelope_ids: Vec::new(),
                created_at: "2026-08-26T00:00:00Z".into(),
            },
            request,
        };
        store
            .save_permission_request(&mut session, &update)
            .await
            .unwrap();

        let second = events
            .append_user_followup(followup("followup-2", "user-2", "correction"))
            .await
            .unwrap();
        assert_eq!(second.input_revision, 2);
        let snapshot = store
            .read_snapshot("conversation-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.permission_requests[0].state,
            PermissionRequestState::NeedsRevalidation
        );
        assert!(!snapshot.permission_requests[0].state.can_user_decide());
        let rows = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("conversation-1"))
            .order_by_asc(agent_run_event::Column::EventSeq)
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.kind.as_str()).collect::<Vec<_>>(),
            vec!["user_followup", "permission_requested", "user_followup"]
        );
    }

    #[tokio::test]
    async fn permission_decision_mints_scoped_grant_but_never_dispatches() {
        let store = store().await.with_client_metadata(
            Some("client-conversation-1".into()),
            AgentSessionSurface::DeviceAssistant,
        );
        let events = SignalAgentRunEventStore::new(store.db.clone());
        events
            .append_user_followup(followup("followup-1", "user-1", "inspect the target"))
            .await
            .unwrap();
        let mut session = store.claim_turn(claim("permission-turn")).await.unwrap();
        session.policy_revision = 1;
        let request = PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "permission-1".into(),
            input_revision: 1,
            state: PermissionRequestState::Pending,
            items: vec![GrantRequestItem {
                item_id: "inspect".into(),
                provider_id: "desktop.session".into(),
                tool_name: "inspect_desktop_session".into(),
                expected_effect: CapabilityEffect::ReadDevice,
                resource_scope: vec!["target:device-1".into()],
                operation_scope: vec!["observe".into()],
                export_destinations: Vec::new(),
                canonical_input_json: None,
                canonical_input_digest_sha256: None,
                suggested_ttl_seconds: 300,
                suggested_max_uses: 1,
                reason: "Inspect the requested target".into(),
            }],
            created_at: "2026-08-26T00:00:00Z".into(),
        };
        session.add_permission_request(request.clone()).unwrap();
        session.last_event_seq = 2;
        let run_id = session.conversation_id.clone();
        store
            .save_permission_request(
                &mut session,
                &PermissionRequestedEvent {
                    event: AgentRunEvent {
                        schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                        event_id: "permission-event-1".into(),
                        run_id,
                        event_seq: 2,
                        input_revision: 1,
                        kind: AgentRunEventKind::PermissionRequested,
                        correlation_id: Some(request.request_id.clone()),
                        source_envelope_ids: Vec::new(),
                        result_envelope_ids: Vec::new(),
                        created_at: "2026-08-26T00:00:00Z".into(),
                    },
                    request,
                },
            )
            .await
            .unwrap();
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        store.save(&mut session).await.unwrap();

        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let inventory = vec![CapabilityAvailability {
            provider_id: "desktop.session".into(),
            capability_id: "desktop.session.inspect".into(),
            tool_name: "inspect_desktop_session".into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        let resulting_state = store
            .decide_permission_request(
                "conversation-1",
                "1",
                "device-1",
                "permission-1",
                vec![PermissionDecisionItem {
                    item_id: "inspect".into(),
                    decision: PermissionItemDecision::Approve {
                        resource_scope: vec!["target:device-1".into()],
                        operation_scope: vec!["observe".into()],
                        export_destinations: Vec::new(),
                        ttl_seconds: 120,
                        max_uses: 1,
                    },
                }],
                PermissionGrantIssuanceContext {
                    surface: ProductSurface::OssPersonalOwner,
                    registry: &registry,
                    inventory: &inventory,
                    readiness_revision: 3,
                    now_unix_ms: 1_000,
                    implicit_fresh_object_refs: &[],
                },
                "2026-08-26T00:01:00Z",
            )
            .await
            .unwrap();
        assert_eq!(resulting_state, PermissionRequestState::Approved);

        let snapshot = store
            .read_snapshot("conversation-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.permission_requests[0].state,
            PermissionRequestState::Approved
        );
        let rows = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("conversation-1"))
            .order_by_asc(agent_run_event::Column::EventSeq)
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.kind.as_str()).collect::<Vec<_>>(),
            vec![
                "user_followup",
                "permission_requested",
                "permission_decided"
            ]
        );
        assert!(
            rows[2]
                .payload_json
                .contains("\"resulting_state\":\"approved\"")
        );
        assert!(!rows[2].payload_json.contains("dispatch"));
        let grants = agent_capability_grant::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(grants.len(), 1);
        let grant: CapabilityGrant = serde_json::from_str(&grants[0].payload_json).unwrap();
        assert_eq!(grant.provider_id, "desktop.session");
        assert_eq!(grant.capability_id, "desktop.session.inspect");
        assert_eq!(grant.resource_scope, vec!["target:device-1"]);
        assert_eq!(grant.operation_scope, vec!["observe"]);
        assert_eq!(grant.remaining_uses, 1);
        assert_eq!(grant.expires_at_unix_ms, 121_000);
        assert_eq!(
            grant.risk_tier,
            desk_agent_protocol::capability_grant::CapabilityRiskTier::R0
        );
    }

    #[tokio::test]
    async fn semantic_ui_grant_is_one_shot_and_server_binds_exact_authority() {
        let store = store().await;
        let mut session = store.claim_turn(claim("ui-action-turn")).await.unwrap();
        session.input_revision = 1;
        session.policy_revision = 1;
        let target = ObjectRef {
            token: "signed-ui-element-token".into(),
            snapshot_id: "snapshot-1".into(),
            object_kind: ObjectKind::UiElement,
            expires_at: "2026-08-26T00:05:00Z".into(),
        };
        let canonical_input_json = serde_json::json!({
            "target": target,
            "action": {"kind": "focus"},
        })
        .to_string();
        let canonical_input_digest_sha256 =
            format!("{:x}", Sha256::digest(canonical_input_json.as_bytes()));
        let exact_resource_scope =
            desk_diagnose_core::capability_grant::fresh_object_resource_scope(&[target]);
        let request = PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "permission-ui-action".into(),
            input_revision: 1,
            state: PermissionRequestState::Pending,
            items: vec![GrantRequestItem {
                item_id: "ui-action".into(),
                provider_id: desk_diagnose_core::device_assistant::DESKTOP_UI_ACTION_PROVIDER_ID
                    .into(),
                tool_name: desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_UI_ACTION_TOOL
                    .into(),
                expected_effect: CapabilityEffect::MutateApplication,
                resource_scope: exact_resource_scope.clone(),
                operation_scope: vec!["use_selected_object".into()],
                export_destinations: Vec::new(),
                canonical_input_json: Some(canonical_input_json),
                canonical_input_digest_sha256: Some(canonical_input_digest_sha256.clone()),
                suggested_ttl_seconds: 120,
                suggested_max_uses: 1,
                reason: "Focus the selected UI element".into(),
            }],
            created_at: "2026-08-26T00:00:00Z".into(),
        };
        let mut decisions = vec![PermissionDecisionItem {
            item_id: "ui-action".into(),
            decision: PermissionItemDecision::Approve {
                resource_scope: vec!["display-label-cannot-authorize".into()],
                operation_scope: vec!["widened-operation".into()],
                export_destinations: vec![DestinationIdentity::EmailAccount {
                    account_id: "must-not-survive".into(),
                }],
                ttl_seconds: 120,
                max_uses: 1,
            },
        }];
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let inventory = vec![CapabilityAvailability {
            provider_id: desk_diagnose_core::device_assistant::DESKTOP_UI_ACTION_PROVIDER_ID.into(),
            capability_id: desk_diagnose_core::device_assistant::DESKTOP_UI_ACTION_CAPABILITY_ID
                .into(),
            tool_name: desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_UI_ACTION_TOOL
                .into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];

        let context = PermissionGrantIssuanceContext {
            surface: ProductSurface::OssPersonalOwner,
            registry: &registry,
            inventory: &inventory,
            readiness_revision: 7,
            now_unix_ms: 1_000,
            implicit_fresh_object_refs: &[],
        };
        assert!(build_permission_grants(&session, &request, &decisions, &context).is_err());
        decisions[0].decision = PermissionItemDecision::Approve {
            resource_scope: exact_resource_scope.clone(),
            operation_scope: request.items[0].operation_scope.clone(),
            export_destinations: Vec::new(),
            ttl_seconds: 120,
            max_uses: 1,
        };
        let grants = build_permission_grants(&session, &request, &decisions, &context).unwrap();

        assert_eq!(grants.len(), 1);
        let grant = &grants[0];
        assert_eq!(grant.resource_scope, exact_resource_scope);
        assert_eq!(grant.operation_scope, vec!["use_selected_object"]);
        assert!(grant.export_destinations.is_empty());
        assert_eq!(grant.remaining_uses, 1);
        assert_eq!(grant.limits.max_calls, 1);
        assert_eq!(grant.use_policy, CapabilityGrantUsePolicy::OneShotExact);
        assert_eq!(
            grant.canonical_input_digest_sha256.as_deref(),
            Some(canonical_input_digest_sha256.as_str())
        );
        decisions[0].decision = PermissionItemDecision::Approve {
            resource_scope: Vec::new(),
            operation_scope: Vec::new(),
            export_destinations: Vec::new(),
            ttl_seconds: 120,
            max_uses: 1,
        };
        let narrowed = build_permission_grants(&session, &request, &decisions, &context).unwrap();
        assert!(narrowed[0].resource_scope.is_empty());
        assert!(narrowed[0].operation_scope.is_empty());
    }

    #[tokio::test]
    async fn newer_followup_supersedes_read_turn_and_preserves_tool_order() {
        use desk_diagnose_core::chat::{ChatMessage, ChatRole, ToolCallRef};

        let store = store().await.with_client_metadata(
            Some("client-conversation-1".into()),
            AgentSessionSurface::DeviceAssistant,
        );
        let events = SignalAgentRunEventStore::new(store.db.clone());
        events
            .append_user_followup(followup("followup-1", "user-1", "first"))
            .await
            .unwrap();
        let mut stale = store.claim_turn(claim("turn-1")).await.unwrap();
        stale.conversation.push(ChatMessage::assistant_tool_calls(
            "assistant-tools",
            String::new(),
            vec![ToolCallRef {
                id: "read-call-1".into(),
                name: "inspect_system".into(),
                arguments_json: "{}".into(),
            }],
        ));
        store.save(&mut stale).await.unwrap();

        let second = events
            .append_user_followup(followup("followup-2", "user-2", "newer"))
            .await
            .unwrap();
        assert_eq!((second.input_seq, second.input_revision), (2, 2));
        stale.conversation.push(ChatMessage::tool_result(
            "read-result-1",
            "read-call-1",
            "completed read result",
        ));

        assert!(
            store
                .settle_superseded(&stale, &Utc::now().to_rfc3339())
                .await
                .unwrap()
        );
        let snapshot = store
            .read_snapshot("conversation-1")
            .await
            .unwrap()
            .unwrap();
        assert!(!snapshot.active);
        assert_eq!(snapshot.input_revision, 2);
        assert_eq!(snapshot.handled_input_seq, 0);
        let ordered = snapshot
            .messages
            .iter()
            .map(|message| {
                (
                    message.role,
                    message.message_id.as_str(),
                    message.tool_call_id.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ordered,
            vec![
                (ChatRole::User, "user-1", None),
                (ChatRole::Assistant, "assistant-tools", None),
                (ChatRole::Tool, "read-result-1", Some("read-call-1")),
                (ChatRole::User, "user-2", None),
            ]
        );
        let ledger = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq("conversation-1"))
            .order_by_asc(agent_run_event::Column::EventSeq)
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(
            ledger
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["user_followup", "user_followup", "superseded"]
        );
        assert_eq!(ledger[2].input_revision, 2);
    }

    fn context_attachment(
        id: &str,
        client_request_id: &str,
        incarnation: &str,
        now_unix_ms: u64,
    ) -> ContextAttachment {
        ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: id.into(),
            client_request_id: client_request_id.into(),
            actor_id: "1".into(),
            device_id: "device-1".into(),
            surface: AgentSessionSurface::DeviceAssistant,
            kind: ContextAttachmentKind::InteractiveSession,
            object_ref: AttachmentObjectRef {
                opaque_token: format!("opaque-{id}"),
                object_incarnation: incarnation.into(),
                source_provider_id: "desktop.ui".into(),
                source_capability_id: "desktop.ui.inspect".into(),
            },
            bounds: AttachmentBounds {
                max_bytes: 1024,
                max_objects: 16,
            },
            display_summary: "desktop.ui.inspect on the current interactive session".into(),
            created_at_unix_ms: now_unix_ms,
            expires_at_unix_ms: now_unix_ms + 60_000,
            envelope: DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: format!("envelope-{id}"),
                content: ContentRef::EphemeralObservation {
                    observation_id: format!("observation-{id}"),
                    size_bytes: 1,
                    expires_at_unix_ms: now_unix_ms + 60_000,
                },
                provenance: DataProvenance {
                    source_provider_id: "desktop.ui".into(),
                    source_tool_name: "inspect_desktop_ui".into(),
                    source_object_id: Some(format!("opaque-{id}")),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: "a".repeat(64),
                sensitivity: Sensitivity::UserContent,
                allowed_destinations: Vec::new(),
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(now_unix_ms + 60_000),
                    delete_with_run: false,
                },
            },
            state: AttachmentState::Active,
        }
    }

    #[tokio::test]
    async fn save_keeps_turn_image_in_memory_but_never_in_sqlite_session_json() {
        let store = store().await;
        let mut session = store.claim_turn(claim("visual-turn")).await.unwrap();
        let image_data_url = "data:image/jpeg;base64,AQID";
        session.conversation.push(
            desk_diagnose_core::chat::ChatMessage::tool_result(
                "visual-result",
                "screen-call",
                "screen metadata",
            )
            .with_image(image_data_url),
        );

        store.save(&mut session).await.unwrap();
        assert_eq!(
            session
                .conversation
                .last()
                .unwrap()
                .image_data_url
                .as_deref(),
            Some(image_data_url)
        );

        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(!row.state_json.contains(image_data_url));
        let persisted = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        assert!(
            persisted
                .conversation
                .iter()
                .all(|message| message.image_data_url.is_none())
        );
    }

    #[tokio::test]
    async fn next_context_claim_purges_legacy_current_screen_metadata() {
        let base = store().await.with_client_metadata(
            Some("screen-client-conversation".into()),
            AgentSessionSurface::DeviceAssistant,
        );
        let now_unix_ms = u64::try_from(Utc::now().timestamp_millis()).unwrap();
        let mut session = base.claim_turn(claim("screen-turn-1")).await.unwrap();
        let mut legacy = context_attachment(
            "legacy-current-screen",
            "legacy-screen-request",
            "worker-1",
            now_unix_ms,
        );
        legacy.kind = ContextAttachmentKind::CurrentScreen;
        legacy.object_ref.source_provider_id = "screen.capture".into();
        legacy.object_ref.source_capability_id = "screen.capture.current".into();
        legacy.envelope.sensitivity = Sensitivity::Sensitive;
        session.context_attachments.push(legacy);
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        base.save(&mut session).await.unwrap();

        let reconciled = base
            .clone()
            .with_context_selection(context_selection(
                false,
                "unused",
                "worker-1",
                now_unix_ms + 1,
            ))
            .claim_turn(claim("screen-turn-2"))
            .await
            .unwrap();

        assert!(
            reconciled
                .context_attachments
                .iter()
                .all(|attachment| attachment.kind != ContextAttachmentKind::CurrentScreen)
        );
    }

    fn context_selection(
        selected: bool,
        candidate_id: &str,
        incarnation: &str,
        now_unix_ms: u64,
    ) -> ContextSelectionClaim {
        ContextSelectionClaim {
            selected_capability_ids: selected
                .then(|| vec!["desktop.ui.inspect".into()])
                .unwrap_or_default(),
            runtime_bindings: vec![AttachmentRuntimeBinding {
                source_provider_id: "desktop.ui".into(),
                source_capability_id: "desktop.ui.inspect".into(),
                object_incarnation: incarnation.into(),
            }],
            candidates: selected
                .then(|| {
                    vec![context_attachment(
                        candidate_id,
                        &format!("request-{candidate_id}"),
                        incarnation,
                        now_unix_ms,
                    )]
                })
                .unwrap_or_default(),
            now_unix_ms,
        }
    }

    fn file_attachment(
        id: &str,
        client_request_id: &str,
        incarnation: &str,
        now_unix_ms: u64,
    ) -> ContextAttachment {
        let mut attachment = context_attachment(id, client_request_id, incarnation, now_unix_ms);
        attachment.kind = ContextAttachmentKind::File;
        attachment.object_ref.source_provider_id = "file.workspace".into();
        attachment.object_ref.source_capability_id = "file.metadata.read".into();
        attachment.display_summary = "selected.txt".into();
        attachment
    }

    async fn create_file_store(path: &std::path::Path) -> SignalAgentSessionStore {
        let db = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(agent_session::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(agent_exec_task::Entity))
            .await
            .unwrap();
        SignalAgentSessionStore::new(db)
    }

    #[tokio::test]
    async fn settled_session_persists_and_continues_on_a_follow_up() {
        let store = store().await;
        let mut first = store.claim_turn(claim("turn-1")).await.unwrap();
        assert_eq!(first.turn_state, TurnState::Running);
        first.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        store.save(&mut first).await.unwrap();

        let second = store.claim_turn(claim("turn-2")).await.unwrap();
        assert_eq!(second.turn_state, TurnState::Running);
        assert_eq!(second.conversation_id, first.conversation_id);
        assert!(second.version > first.version);
    }

    #[tokio::test]
    async fn snapshot_reports_active_then_settled_and_advances() {
        use desk_diagnose_core::chat::{ChatMessage, ChatRole};

        let store = store().await;
        assert!(store.read_snapshot("missing").await.unwrap().is_none());

        let mut session = store.claim_turn(claim("turn-1")).await.unwrap();
        session.conversation.push(
            ChatMessage::text("u1", ChatRole::User, "question")
                .with_image("data:image/jpeg;base64,AQID"),
        );
        session.execution_state = ExecutionState::Executing {
            action: ActionIdentity::agent_exec(7, "task-bg-1", "generation-bg-1"),
        };
        store.save(&mut session).await.unwrap();
        let active = store
            .read_snapshot("conversation-1")
            .await
            .unwrap()
            .unwrap();
        assert!(active.active);
        assert_eq!(active.request_id.as_deref(), Some("request-turn-1"));
        assert_eq!(
            active.active_execution_generation.as_deref(),
            Some("generation-bg-1")
        );
        assert_eq!(active.messages.len(), 1);
        assert!(active.messages[0].image_data_url.is_none());
        assert!(session.conversation[0].image_data_url.is_some());

        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        store.save(&mut session).await.unwrap();
        assert!(session.conversation[0].image_data_url.is_some());
        let settled = store
            .read_snapshot("conversation-1")
            .await
            .unwrap()
            .unwrap();
        assert!(!settled.active);
        assert_eq!(
            settled.request_id.as_deref(),
            Some("request-turn-1"),
            "the settled snapshot keeps the request binding used for UI recovery"
        );
        assert_eq!(
            settled.active_execution_generation.as_deref(),
            Some("generation-bg-1"),
            "a settled model turn still exposes its running background command"
        );
        assert!(settled.seq > active.seq);
    }

    #[tokio::test]
    async fn standalone_context_update_creates_session_is_idempotent_and_detaches() {
        let base = store().await;
        let now_unix_ms = u64::try_from(Utc::now().timestamp_millis()).unwrap();
        let now = Utc::now().to_rfc3339();
        let selected = base
            .clone()
            .with_client_metadata(
                Some("assistant-context".into()),
                AgentSessionSurface::DeviceAssistant,
            )
            .with_context_selection(context_selection(
                true,
                "standalone-1",
                "worker-1",
                now_unix_ms,
            ));
        let scope = claim("unused").current_pdp_scope;

        assert!(
            selected
                .update_context_selection(
                    "context-conversation",
                    "1",
                    "device-1",
                    scope.clone(),
                    &now,
                )
                .await
                .unwrap()
        );
        assert!(
            !selected
                .update_context_selection(
                    "context-conversation",
                    "1",
                    "device-1",
                    scope.clone(),
                    &now,
                )
                .await
                .unwrap()
        );
        let attached = base
            .read_snapshot("context-conversation")
            .await
            .unwrap()
            .unwrap();
        assert!(!attached.active);
        assert_eq!(attached.context_attachments.len(), 1);
        assert!(matches!(
            attached.context_attachments[0].state,
            AttachmentState::Active
        ));

        let deselected = base
            .clone()
            .with_client_metadata(
                Some("assistant-context".into()),
                AgentSessionSurface::DeviceAssistant,
            )
            .with_context_selection(context_selection(
                false,
                "unused",
                "worker-1",
                now_unix_ms + 1,
            ));
        assert!(
            deselected
                .update_context_selection(
                    "context-conversation",
                    "1",
                    "device-1",
                    scope,
                    &Utc::now().to_rfc3339(),
                )
                .await
                .unwrap()
        );
        let detached = base
            .read_snapshot("context-conversation")
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            detached.context_attachments[0].state,
            AttachmentState::Stale {
                reason: AttachmentStaleReason::Detached
            }
        ));
    }

    #[tokio::test]
    async fn object_context_is_idempotent_and_capability_reconciliation_preserves_it() {
        let base = store().await;
        let now_unix_ms = u64::try_from(Utc::now().timestamp_millis()).unwrap();
        let now = Utc::now().to_rfc3339();
        let scoped = base.clone().with_client_metadata(
            Some("assistant-files".into()),
            AgentSessionSurface::DeviceAssistant,
        );
        let scope = claim("unused").current_pdp_scope;
        let attachment = file_attachment(
            "file-attachment-1",
            "file-request-1",
            "worker-1:file-7",
            now_unix_ms,
        );
        let attach = ObjectContextMutation::Attach(attachment);

        assert!(
            scoped
                .update_object_context(
                    "file-conversation",
                    "1",
                    "device-1",
                    scope.clone(),
                    &attach,
                    &now,
                )
                .await
                .unwrap()
        );
        assert!(
            !scoped
                .update_object_context(
                    "file-conversation",
                    "1",
                    "device-1",
                    scope.clone(),
                    &attach,
                    &now,
                )
                .await
                .unwrap()
        );

        let capability_update = scoped.clone().with_context_selection(context_selection(
            false,
            "unused",
            "worker-1",
            now_unix_ms + 1,
        ));
        assert!(
            !capability_update
                .update_context_selection(
                    "file-conversation",
                    "1",
                    "device-1",
                    scope.clone(),
                    &now,
                )
                .await
                .unwrap()
        );
        let attached = base
            .read_snapshot("file-conversation")
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            attached.context_attachments[0].state,
            AttachmentState::Active
        ));

        let detach = ObjectContextMutation::Detach {
            attachment_id: "file-attachment-1".into(),
        };
        assert!(
            scoped
                .update_object_context(
                    "file-conversation",
                    "1",
                    "device-1",
                    scope.clone(),
                    &detach,
                    &now,
                )
                .await
                .unwrap()
        );
        assert!(
            !scoped
                .update_object_context("file-conversation", "1", "device-1", scope, &detach, &now,)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn context_selection_survives_sqlite_reopen_and_worker_respawn_is_one_way() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("assistant-context.db");
        let now_unix_ms = u64::try_from(Utc::now().timestamp_millis()).unwrap();
        let first = create_file_store(&path)
            .await
            .with_client_metadata(
                Some("client-1".into()),
                AgentSessionSurface::DeviceAssistant,
            )
            .with_context_selection(context_selection(
                true,
                "attachment-1",
                "worker-1",
                now_unix_ms,
            ));
        let mut session = first.claim_turn(claim("turn-1")).await.unwrap();
        assert_eq!(session.active_context(now_unix_ms).len(), 1);
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        first.save(&mut session).await.unwrap();
        let db = first.db.clone();
        drop(first);
        db.close().await.unwrap();

        let reopened_db = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let reopened = SignalAgentSessionStore::new(reopened_db)
            .with_client_metadata(
                Some("client-1".into()),
                AgentSessionSurface::DeviceAssistant,
            )
            .with_context_selection(context_selection(
                true,
                "attachment-2",
                "worker-2",
                now_unix_ms + 1,
            ));
        let mut after_respawn = reopened.claim_turn(claim("turn-2")).await.unwrap();
        assert_eq!(after_respawn.context_attachments.len(), 2);
        assert!(matches!(
            after_respawn.context_attachments[0].state,
            AttachmentState::Stale {
                reason: AttachmentStaleReason::WorkerRespawned
            }
        ));
        assert_eq!(after_respawn.active_context(now_unix_ms + 1).len(), 1);
        assert_eq!(
            after_respawn.active_context(now_unix_ms + 1)[0].attachment_id,
            "attachment-2"
        );
        after_respawn.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        reopened.save(&mut after_respawn).await.unwrap();

        let deselected = SignalAgentSessionStore::new(reopened.db.clone())
            .with_client_metadata(
                Some("client-1".into()),
                AgentSessionSurface::DeviceAssistant,
            )
            .with_context_selection(context_selection(
                false,
                "unused",
                "worker-2",
                now_unix_ms + 2,
            ));
        let after_deselect = deselected.claim_turn(claim("turn-3")).await.unwrap();
        assert!(after_deselect.active_context(now_unix_ms + 2).is_empty());
        assert!(matches!(
            after_deselect.context_attachments[1].state,
            AttachmentState::Stale {
                reason: AttachmentStaleReason::Detached
            }
        ));
    }

    #[tokio::test]
    async fn stage5_committed_artifact_identity_and_lineage_survive_sqlite_wal_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stage5-artifact-reopen.db");
        let first = create_file_store(&path).await;
        let mut session = first.claim_turn(claim("artifact-turn")).await.unwrap();
        let artifact = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "docx-envelope-1".into(),
            content: ContentRef::Artifact {
                artifact_id: "docx-artifact-1".into(),
                sha256: "a".repeat(64),
                size_bytes: 7,
                media_type:
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "file.workspace".into(),
                source_tool_name: "create_word_report_from_merge_preview".into(),
                source_object_id: Some("device-1:docx-call-1".into()),
                source_envelope_ids: vec!["merge-preview-envelope-1".into()],
            },
            digest_sha256: "a".repeat(64),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        };
        artifact.validate().unwrap();
        let mut result = desk_diagnose_core::chat::ChatMessage::tool_result(
            "docx-result-1",
            "docx-call-1",
            "typed artifact created",
        );
        result.data_envelope = Some(artifact.clone());
        session.conversation.push(result);
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        first.save(&mut session).await.unwrap();
        let db = first.db.clone();
        drop(first);
        db.close().await.unwrap();

        let reopened_db = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let reopened = SignalAgentSessionStore::new(reopened_db);
        let resumed = reopened
            .claim_turn(claim("communication-turn"))
            .await
            .unwrap();
        let restored = resumed
            .conversation
            .iter()
            .filter_map(|message| message.data_envelope.as_ref())
            .find(|envelope| envelope.envelope_id == "docx-envelope-1")
            .unwrap();
        assert_eq!(restored, &artifact);
        assert!(matches!(
            restored.content,
            ContentRef::Artifact {
                ref artifact_id,
                ..
            } if artifact_id == "docx-artifact-1"
        ));
        assert_eq!(
            restored.provenance.source_envelope_ids,
            vec!["merge-preview-envelope-1"]
        );
        reopened.db.close().await.unwrap();
    }

    #[tokio::test]
    async fn history_is_subject_scoped_and_excludes_other_surfaces() {
        use desk_diagnose_core::chat::{ChatMessage, ChatRole};

        let base = store().await;
        let db = base.db.clone();
        let diagnose = SignalAgentSessionStore::new(db.clone()).with_client_metadata(
            Some("client-conv-1".into()),
            AgentSessionSurface::DeviceAssistant,
        );
        let mut session = diagnose.claim_turn(claim("turn-1")).await.unwrap();
        session
            .conversation
            .push(ChatMessage::text("u1", ChatRole::User, "why slow?"));
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        diagnose.save(&mut session).await.unwrap();

        let terminal = SignalAgentSessionStore::new(db).with_client_metadata(
            Some("terminal-conv-1".into()),
            AgentSessionSurface::TerminalCopilot,
        );
        let mut terminal_claim = claim("terminal-turn");
        terminal_claim.conversation_id = "terminal-session".into();
        let mut terminal_session = terminal.claim_turn(terminal_claim).await.unwrap();
        terminal_session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        terminal.save(&mut terminal_session).await.unwrap();

        let initial = diagnose
            .list_device_assistant_sessions("1", "device-1", 30)
            .await
            .unwrap();
        assert_eq!(initial.len(), 1);

        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value("{not valid json"),
            )
            .filter(agent_session::Column::ConversationId.eq("terminal-session"))
            .exec(&diagnose.db)
            .await
            .unwrap();
        let summaries = diagnose
            .list_device_assistant_sessions("1", "device-1", 30)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].client_conversation_id.as_deref(),
            Some("client-conv-1")
        );
        assert_eq!(summaries[0].first_question.as_deref(), Some("why slow?"));
        assert!(
            diagnose
                .read_snapshot_for_subject("conversation-1", "2", "device-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_live_turn_is_busy() {
        let store = store().await;
        let _first = store.claim_turn(claim("turn-1")).await.unwrap();
        assert!(matches!(
            store.claim_turn(claim("turn-2")).await,
            Err(ClaimError::Busy)
        ));
    }

    #[tokio::test]
    async fn expired_read_only_call_recovers_as_not_executed() {
        use desk_diagnose_core::chat::{ChatMessage, ToolCallRef};

        let store = store().await;
        let mut first = store.claim_turn(claim("turn-1")).await.unwrap();
        first.conversation.push(ChatMessage::assistant_tool_calls(
            "assistant-tools",
            String::new(),
            vec![ToolCallRef {
                id: "read-call-1".into(),
                name: "system_info".into(),
                arguments_json: "{}".into(),
            }],
        ));
        store.save(&mut first).await.unwrap();
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Some(Utc::now() - Duration::seconds(1))),
            )
            .filter(agent_session::Column::ConversationId.eq("conversation-1"))
            .exec(&store.db)
            .await
            .unwrap();

        let recovered = store.claim_turn(claim("turn-2")).await.unwrap();
        assert_eq!(recovered.execution_state, ExecutionState::None);
        let result = recovered
            .conversation
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("read-call-1"))
            .expect("recovery closes the dangling read call");
        assert!(result.text.contains("not executed"));
        assert!(
            !matches!(
                recovered.execution_state,
                ExecutionState::Interrupted { .. }
            ),
            "a read-only crash must not permanently disable future mutation"
        );
    }

    #[tokio::test]
    async fn completion_delivery_is_deferred_while_turn_is_live() {
        let store = store().await;
        let _session = store.claim_turn(claim("turn-1")).await.unwrap();
        assert_eq!(
            store
                .deliver_completion(
                    "conversation-1",
                    7,
                    "event-1",
                    "generation-1",
                    "call-1",
                    "task-1",
                    "done",
                    &Utc::now().to_rfc3339(),
                )
                .await
                .unwrap(),
            EventAppend::Busy
        );
    }

    #[tokio::test]
    async fn manual_unknown_disposition_is_subject_and_identity_fenced() {
        let store = store().await;
        let mut session = store.claim_turn(claim("turn-1")).await.unwrap();
        session
            .conversation
            .push(desk_diagnose_core::chat::ChatMessage::tool_result(
                "unknown-placeholder",
                "call-1",
                "outcome unknown",
            ));
        session.execution_state = ExecutionState::OutcomeUnknown {
            action: ActionIdentity::new(94, "action-94", "generation-94", WorkKind::ComputerAction),
            placeholder_message_id: "unknown-placeholder".into(),
            since: Utc::now().to_rfc3339(),
        };
        session.finish_turn(TurnState::Failed, Utc::now().to_rfc3339());
        store.save(&mut session).await.unwrap();

        assert_eq!(
            store
                .manually_dispose_unknown_for_subject(
                    "conversation-1",
                    "other-actor",
                    "device-1",
                    94,
                    "generation-94",
                    &Utc::now().to_rfc3339(),
                )
                .await
                .unwrap(),
            EventAppend::AlreadyPresent
        );
        assert_eq!(
            store
                .manually_dispose_unknown_for_subject(
                    "conversation-1",
                    "1",
                    "device-1",
                    94,
                    "wrong-generation",
                    &Utc::now().to_rfc3339(),
                )
                .await
                .unwrap(),
            EventAppend::AlreadyPresent
        );
        assert_eq!(
            store
                .manually_dispose_unknown_for_subject(
                    "conversation-1",
                    "1",
                    "device-1",
                    94,
                    "generation-94",
                    &Utc::now().to_rfc3339(),
                )
                .await
                .unwrap(),
            EventAppend::Appended
        );
        let snapshot = store
            .read_snapshot_for_subject("conversation-1", "1", "device-1")
            .await
            .unwrap()
            .unwrap();
        assert!(snapshot.unresolved_action.is_none());
        assert!(snapshot.active_execution_generation.is_none());
    }

    #[tokio::test]
    async fn settled_completion_is_applied_once_and_clears_execution() {
        let store = store().await;
        let mut session = store.claim_turn(claim("turn-1")).await.unwrap();
        session.execution_state = ExecutionState::Executing {
            action: ActionIdentity::agent_exec(7, "task-1", "generation-1"),
        };
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        store.save(&mut session).await.unwrap();

        let now = Utc::now().to_rfc3339();
        assert_eq!(
            store
                .deliver_completion(
                    "conversation-1",
                    7,
                    "event-1",
                    "generation-1",
                    "call-1",
                    "task-1",
                    "done",
                    &now,
                )
                .await
                .unwrap(),
            EventAppend::Appended
        );
        assert_eq!(
            store
                .deliver_completion(
                    "conversation-1",
                    7,
                    "event-1",
                    "generation-1",
                    "call-1",
                    "task-1",
                    "done",
                    &now,
                )
                .await
                .unwrap(),
            EventAppend::AlreadyPresent
        );
        let row = find(&store.db, "conversation-1").await.unwrap().unwrap();
        let saved: PersistedAgentSession = serde_json::from_str(&row.state_json).unwrap();
        assert!(matches!(saved.execution_state, ExecutionState::None));
        assert_eq!(saved.pending_auto_triggers.len(), 1);
        assert_eq!(saved.pending_auto_triggers[0].event_id, "event-1");
        assert!(
            saved
                .conversation
                .iter()
                .any(|message| message.message_id == "event-1"
                    && message.background_task_id.as_deref() == Some("task-1")
                    && message.text == "done")
        );

        assert_eq!(
            store
                .prune_auto_trigger("conversation-1", "event-1", &now)
                .await
                .unwrap(),
            EventAppend::Appended
        );
        assert!(
            store
                .pending_auto_trigger("conversation-1", "event-1")
                .await
                .unwrap()
                .is_none()
        );
    }
}
