//! Owner-authorized deletion fences the model turn before removing its history.
use super::*;
use crate::entity::{agent_image_attachment, agent_schedule};
use sea_orm::ExprTrait;

impl SignalAgentSessionStore {
    pub async fn delete_for_subject(
        &self,
        id: &str,
        actor: &str,
        device: &str,
    ) -> Result<Option<String>, AgentError> {
        let txn = crate::db::begin_write(&self.db, agent_session::Entity)
            .await
            .map_err(save_backend)?;

        // Take the write lock before reading; stale workers cannot save after deletion.
        let locked = agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::Version,
                Expr::col(agent_session::Column::Version).into(),
            )
            .filter(agent_session::Column::ConversationId.eq(id))
            .filter(agent_session::Column::ActorId.eq(actor))
            .filter(agent_session::Column::DeviceId.eq(device))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        if locked.rows_affected != 1 {
            return Err(internal("Conversation not found or not accessible"));
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(id))
            .one(&txn)
            .await
            .map_err(save_backend)?
            .ok_or_else(|| internal("Conversation not found"))?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| internal("Invalid conversation"))?;
        session
            .check_subject(actor, device)
            .map_err(|_| internal("Conversation not accessible"))?;
        session
            .check_surface(AgentSessionSurface::DeviceAssistant)
            .map_err(|_| internal("Conversation not accessible"))?;
        agent_schedule::Entity::update_many()
            .col_expr(agent_schedule::Column::Status, Expr::value("deleted"))
            .col_expr(
                agent_schedule::Column::Revision,
                Expr::col(agent_schedule::Column::Revision).add(1),
            )
            .col_expr(
                agent_schedule::Column::NextRunAt,
                Expr::value(Option::<i64>::None),
            )
            .filter(agent_schedule::Column::SourceConversationId.eq(id))
            .filter(agent_schedule::Column::Kind.eq("conversation_resume"))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        agent_image_attachment::Entity::update_many()
            .col_expr(agent_image_attachment::Column::Deleted, Expr::value(true))
            .filter(agent_image_attachment::Column::ConversationId.eq(id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        crate::entity::agent_grant_reservation::Entity::delete_many()
            .filter(crate::entity::agent_grant_reservation::Column::RunId.eq(id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        crate::entity::agent_capability_grant::Entity::delete_many()
            .filter(crate::entity::agent_capability_grant::Column::RunId.eq(id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        crate::entity::agent_run_event::Entity::delete_many()
            .filter(crate::entity::agent_run_event::Column::RunId.eq(id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        crate::entity::agent_permission_resume::Entity::delete_many()
            .filter(crate::entity::agent_permission_resume::Column::RunId.eq(id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        agent_session::Entity::delete_many()
            .filter(agent_session::Column::Id.eq(row.id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        txn.commit().await.map_err(save_backend)?;
        Ok(session.current_request_id)
    }
}

fn save_backend(error: sea_orm::DbErr) -> AgentError {
    internal(format!("delete conversation: {error}"))
}
