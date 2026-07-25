//! SQLite-backed agent sessions for the single-node OSS signal central brain.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::seam::{ClaimError, ClaimTurnParams, SessionSeam};
use desk_diagnose_core::session::{
    PendingAutoTrigger, PersistedAgentSession, RecoveryVerdict, TurnClaimError,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::entity::{agent_exec_task, agent_session};

const LEASE_TTL_SECS: i64 = 90;
const CLAIM_ATTEMPTS: usize = 5;

#[derive(Clone)]
pub struct SignalAgentSessionStore {
    db: DatabaseConnection,
}

impl SignalAgentSessionStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
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
        let session: PersistedAgentSession = serde_json::from_str(&row.state_json)
            .map_err(|e| internal(format!("decode agent session snapshot: {e}")))?;
        let active_execution_generation = session
            .execution_state
            .waitable_task()
            .map(|(_, execution_id, _)| execution_id.to_string());
        Ok(Some(SessionSnapshot {
            seq: row.version,
            active: session.turn_state.is_active(),
            request_id: session.current_request_id,
            active_execution_generation,
            messages: session.conversation,
        }))
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
        let now_dt = now_from(now);
        for _ in 0..CLAIM_ATTEMPTS {
            let Some(row) = find(&self.db, conversation_id)
                .await
                .map_err(|e| internal(format!("load completion session: {e}")))?
            else {
                return Ok(EventAppend::AlreadyPresent);
            };
            let mut session: PersistedAgentSession = serde_json::from_str(&row.state_json)
                .map_err(|e| internal(format!("decode completion session: {e}")))?;
            session.version = row.version;
            if session.turn_state.is_active()
                && row
                    .lease_deadline
                    .is_some_and(|deadline| deadline >= now_dt)
            {
                return Ok(EventAppend::Busy);
            }
            if !session.apply_completion(
                event_id,
                execution_id,
                tool_call_id,
                background_task_id,
                result_text,
                now,
            ) {
                return Ok(EventAppend::AlreadyPresent);
            }
            session.add_pending_auto_trigger(PendingAutoTrigger {
                work_id,
                execution_id: execution_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
                event_id: event_id.to_string(),
                chain_id: session.chain_id.clone(),
                resolution_org_id: None,
                since: now.to_string(),
            });
            let new_version = row.version + 1;
            session.version = new_version;
            let state_json = serde_json::to_string(&session)
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
        let mut session: PersistedAgentSession = serde_json::from_str(&row.state_json)
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
            let mut session: PersistedAgentSession = serde_json::from_str(&row.state_json)
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
            let state_json = serde_json::to_string(&session)
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
            let mut session: PersistedAgentSession = serde_json::from_str(&row.state_json)
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
            let state_json = serde_json::to_string(&session)
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
    pub messages: Vec<desk_diagnose_core::chat::ChatMessage>,
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
                    let mut session: PersistedAgentSession = serde_json::from_str(&row.state_json)
                        .map_err(|e| {
                            ClaimError::Backend(internal(format!(
                                "decode agent session state: {e}"
                            )))
                        })?;
                    session.version = row.version;
                    session
                        .check_subject(&params.actor_id, &params.device_id)
                        .map_err(ClaimError::Subject)?;

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
                        let task = if unclosed.len() == 1 {
                            agent_exec_task::Entity::find()
                                .filter(
                                    agent_exec_task::Column::ConversationId
                                        .eq(&session.conversation_id),
                                )
                                .filter(agent_exec_task::Column::ToolCallId.eq(unclosed[0].clone()))
                                .order_by_desc(agent_exec_task::Column::Id)
                                .one(&self.db)
                                .await
                                .map_err(|e| {
                                    ClaimError::Backend(internal(format!(
                                        "load interrupted agent execution: {e}"
                                    )))
                                })?
                        } else {
                            None
                        };
                        match task {
                            Some(task) => {
                                session.recover_session(
                                    RecoveryVerdict::OutcomeUnknown {
                                        work_id: task.id,
                                        execution_id: task.execution_generation.clone(),
                                        exec_request_id: task.exec_request_id.clone(),
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
                            None => session.recover_session(
                                RecoveryVerdict::InterruptedUnknown,
                                params.now.clone(),
                            ),
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
                    let state_json = serde_json::to_string(&session).map_err(|e| {
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
                    let _ = session.begin_turn(
                        params.turn_id.clone(),
                        params.request_id.clone(),
                        params.connection_id.clone(),
                        params.policy_revision,
                        params.current_pdp_scope.clone(),
                        params.now.clone(),
                    );
                    session.adopt_trigger(params.trigger_origin, &params.turn_id);
                    let state_json = serde_json::to_string(&session).map_err(|e| {
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
        let mut stored = session.clone();
        stored.version = new_version;
        let state_json = serde_json::to_string(&stored)
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

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::session::{ExecutionState, TriggerOrigin, TurnState};
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn store() -> SignalAgentSessionStore {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(agent_session::Entity))
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
        session
            .conversation
            .push(ChatMessage::text("u1", ChatRole::User, "question"));
        session.execution_state = ExecutionState::Executing {
            work_id: 7,
            execution_id: "generation-bg-1".into(),
            exec_request_id: "task-bg-1".into(),
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

        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        store.save(&mut session).await.unwrap();
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
    async fn a_live_turn_is_busy() {
        let store = store().await;
        let _first = store.claim_turn(claim("turn-1")).await.unwrap();
        assert!(matches!(
            store.claim_turn(claim("turn-2")).await,
            Err(ClaimError::Busy)
        ));
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
    async fn settled_completion_is_applied_once_and_clears_execution() {
        let store = store().await;
        let mut session = store.claim_turn(claim("turn-1")).await.unwrap();
        session.execution_state = ExecutionState::Executing {
            work_id: 7,
            execution_id: "generation-1".into(),
            exec_request_id: "task-1".into(),
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
