//! OSS reads the durable session, using the authenticated host's audience.
use super::*;
use desk_agent_protocol::computer_turn::{ComputerActionTurnQuery, ComputerActionTurnState};
use sea_orm::QuerySelect;
use sea_orm::sea_query::ExprTrait;

impl SignalCapabilityGrantStore {
    pub async fn computer_turn_state(
        &self,
        device_id: &str,
        query: &ComputerActionTurnQuery,
    ) -> Result<ComputerActionTurnState, DbErr> {
        query
            .validate()
            .map_err(|_| DbErr::Custom("invalid turn query".into()))?;
        let row = crate::entity::agent_session::Entity::find()
            .filter(
                crate::entity::agent_session::Column::ConversationId
                    .eq(&query.scope.conversation_id),
            )
            .one(&self.db)
            .await?;
        let session = row
            .map(|row| {
                desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                    .map_err(|_| DbErr::Custom("invalid persisted turn state".into()))
            })
            .transpose()?;
        let state = desk_diagnose_core::computer_turn::classify(query, device_id, session.as_ref());
        if state != ComputerActionTurnState::Current {
            return Ok(state);
        }
        // Use the database clock shared by all central instances. A current
        // reply never renews this lease or the device's local control deadline.
        use sea_orm::sea_query::{Alias, Expr, Func, SimpleExpr};
        let deadline: SimpleExpr = Expr::col(agent_session::Column::LeaseDeadline).into();
        let clock = Expr::current_timestamp();
        // SQLite stores timestamps as text; normalize both RFC3339 and SQL
        // encodings before comparison instead of comparing their separators.
        let live_deadline = if self.db.get_database_backend() == sea_orm::DatabaseBackend::Sqlite {
            let normalized: SimpleExpr = Func::cust(Alias::new("julianday")).arg(deadline).into();
            normalized.gt(Func::cust(Alias::new("julianday")).arg(clock))
        } else {
            deadline.gt(clock)
        };
        let live = agent_session::Entity::find()
            .select_only()
            .column(agent_session::Column::Id)
            .filter(agent_session::Column::ConversationId.eq(&query.scope.conversation_id))
            .filter(agent_session::Column::LeaseToken.eq(query.scope.lease_token as i64))
            .filter(live_deadline)
            .into_tuple::<i64>()
            .one(&self.db)
            .await?
            .is_some();
        Ok(if live {
            state
        } else {
            ComputerActionTurnState::Revoked
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::agent_session;
    use chrono::Utc;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::{
        action_turn_fence::AssistantTurnFence,
        session::{AgentSessionSurface, PersistedAgentSession, TurnState},
    };
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Database, Schema, Set};

    #[tokio::test]
    async fn persisted_state_is_shared_and_corruption_is_not_revocation() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        let schema = Schema::new(backend);
        db.execute(&schema.create_table_from_entity(agent_session::Entity))
            .await
            .unwrap();
        let now = Utc::now();

        let mut session = PersistedAgentSession::new(
            "conversation",
            "7",
            "device",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "now",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session.begin_focus_epoch(1, []).unwrap();
        session
            .begin_turn("turn", None, None, 1, session.scope_snapshot.clone(), "now")
            .unwrap();
        let query = ComputerActionTurnQuery {
            actor_id: "7".into(),
            scope: AssistantTurnFence::from_session(&session)
                .unwrap()
                .unwrap()
                .computer_action_scope()
                .unwrap(),
        };
        let store = SignalCapabilityGrantStore::new(db.clone());
        assert_eq!(
            store.computer_turn_state("device", &query).await.unwrap(),
            ComputerActionTurnState::Revoked
        );
        agent_session::ActiveModel {
            conversation_id: Set(session.conversation_id.clone()),
            actor_id: Set(session.actor_id.clone()),
            device_id: Set(session.device_id.clone()),
            state_json: Set(session.encode_json_for_storage().unwrap()),
            version: Set(session.version),
            lease_token: Set(session.lease_token as i64),
            lease_deadline: Set(Some(now + chrono::Duration::minutes(1))),
            created_at: Set(now),
            updated_at: Set(now),

            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        assert_eq!(
            store.computer_turn_state("device", &query).await.unwrap(),
            ComputerActionTurnState::Current
        );
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::LeaseDeadline,
                sea_orm::sea_query::Expr::value(now - chrono::Duration::minutes(1)),
            )
            .exec(&db)
            .await
            .unwrap();
        assert_eq!(
            store.computer_turn_state("device", &query).await.unwrap(),
            ComputerActionTurnState::Revoked
        );
        session.finish_turn(TurnState::Cancelled, "later");
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                sea_orm::sea_query::Expr::value(session.encode_json_for_storage().unwrap()),
            )
            .exec(&db)
            .await
            .unwrap();
        let other_instance = SignalCapabilityGrantStore::new(db.clone());
        assert_eq!(
            other_instance
                .computer_turn_state("device", &query)
                .await
                .unwrap(),
            ComputerActionTurnState::Revoked
        );
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                sea_orm::sea_query::Expr::value("invalid-json"),
            )
            .exec(&db)
            .await
            .unwrap();
        assert!(
            other_instance
                .computer_turn_state("device", &query)
                .await
                .is_err()
        );
    }
}
