//! Conversation names are presentation metadata; writes never touch execution CAS.
use super::*;

impl SignalAgentSessionStore {
    pub(super) fn load_title_limit<'a>(
        &'a self,
        session: &'a PersistedAgentSession,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<usize>, AgentError>> + Send + 'a>,
    > {
        desk_diagnose_core::future::send_phase(|| async move {
            if self.surface != AgentSessionSurface::AiAssistant {
                return Ok(None);
            }
            let row = agent_session::Entity::find()
                .select_only()
                .column(agent_session::Column::Id)
                .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
                .filter(agent_session::Column::ActorId.eq(&session.actor_id))
                .filter(agent_session::Column::DeviceId.eq(&session.device_id))
                .filter(agent_session::Column::Title.is_null())
                .into_tuple::<i64>()
                .one(&self.db)
                .await
                .map_err(|error| internal(format!("conversation title storage: {error}")))?;
            if row.is_none() {
                return Ok(None);
            }
            Ok(Some(
                crate::config::ConfigConnection::config_read(&self.db)
                    .await
                    .assistant_data_policy
                    .conversation_title_max_chars as usize,
            ))
        })
    }

    pub(super) fn store_title<'a>(
        &'a self,
        session: &'a PersistedAgentSession,
        title: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AgentError>> + Send + 'a>>
    {
        desk_diagnose_core::future::send_phase(|| async move {
            let txn = crate::db::begin_write(&self.db, agent_session::Entity)
                .await
                .map_err(|error| internal(format!("conversation title storage: {error}")))?;
            agent_session::Entity::update_many()
                .col_expr(
                    agent_session::Column::Title,
                    Expr::value(Some(title.to_owned())),
                )
                .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
                .filter(agent_session::Column::ActorId.eq(&session.actor_id))
                .filter(agent_session::Column::DeviceId.eq(&session.device_id))
                .filter(agent_session::Column::Version.eq(session.version))
                .filter(agent_session::Column::LeaseToken.eq(session.lease_token as i64))
                .filter(agent_session::Column::LeaseDeadline.gt(Utc::now()))
                .filter(
                    agent_session::Column::ConversationSchemaVersion.eq(i32::from(
                        desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                    )),
                )
                .filter(agent_session::Column::Title.is_null())
                .exec(&txn)
                .await
                .map_err(|error| internal(format!("conversation title storage: {error}")))?;
            txn.commit()
                .await
                .map_err(|error| internal(format!("conversation title storage: {error}")))?;
            Ok(())
        })
    }
}
