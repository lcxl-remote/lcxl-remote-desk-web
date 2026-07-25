//! SQLite-backed agent sessions for the single-node OSS signal central brain.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::seam::{ClaimError, ClaimTurnParams, SessionSeam};
use desk_diagnose_core::session::{PersistedAgentSession, RecoveryVerdict, TurnClaimError};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::entity::agent_session;

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
                        // Signal has no durable work ledger for an interrupted
                        // synchronous exec. Recover conservatively and permanently
                        // bar further mutation in this conversation.
                        session.recover_session(
                            RecoveryVerdict::InterruptedUnknown,
                            params.now.clone(),
                        );
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
    use desk_diagnose_core::session::{TriggerOrigin, TurnState};
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
    async fn a_live_turn_is_busy() {
        let store = store().await;
        let _first = store.claim_turn(claim("turn-1")).await.unwrap();
        assert!(matches!(
            store.claim_turn(claim("turn-2")).await,
            Err(ClaimError::Busy)
        ));
    }
}
