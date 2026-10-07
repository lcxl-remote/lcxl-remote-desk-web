//! Owner-authorized deletion fences the model turn before removing its history.
use super::*;
use crate::entity::{
    agent_action_item, agent_attachment, agent_capability_dispatch_outbox, agent_schedule,
};
use desk_diagnose_core::goal::GoalRemovalReason;
use sea_orm::{ExprTrait, QuerySelect, QueryTrait};

/// Work that provably never left the server; deletion cancels it.
const UNDISPATCHED_ACTION_STATES: [&str; 4] = [
    crate::agent_action_store::STATUS_AWAITING_APPROVAL,
    crate::agent_action_store::STATUS_APPROVED,
    crate::agent_action_store::STATUS_CLAIMED,
    crate::capability_grant_store::CAPABILITY_WORK_PREPARED,
];

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
                Expr::col(agent_session::Column::Version),
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
            .check_surface(AgentSessionSurface::AiAssistant)
            .map_err(|_| internal("Conversation not accessible"))?;
        if !session.agent_role.is_main() {
            return Err(internal("Only a main conversation can be deleted"));
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        crate::agent_subagent_store::close_root_on(&txn, &session, now_ms)
            .await
            .map_err(save_backend)?;
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
        agent_attachment::Entity::delete_many()
            .filter(agent_attachment::Column::ConversationId.eq(id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        // Do not cascade authorization or execution lineage. Late native and
        // provider receipts still need their original immutable bindings.
        let now = chrono::Utc::now();
        // End every goal of this conversation; a running segment's worker gets
        // an idempotent cancellation when it settles.
        crate::agent_goal_store::cancel_conversation_goals_on(
            &txn,
            id,
            GoalRemovalReason::ConversationRemoved,
            u64::try_from(now_ms).map_err(|_| internal("invalid clock"))?,
        )
        .await
        .map_err(save_backend)?;
        // Undispatched work becomes terminal and its queued outbox entries are
        // dropped; work that may already be on the device keeps its evidence
        // and records the cancellation request. Nothing is reported as "did
        // not run" unless it provably never left the server.
        let conversation_work = || {
            agent_action_item::Entity::find()
                .select_only()
                .column(agent_action_item::Column::Id)
                .filter(agent_action_item::Column::ConversationId.eq(id))
                .into_query()
        };
        agent_capability_dispatch_outbox::Entity::delete_many()
            .filter(
                agent_capability_dispatch_outbox::Column::WorkId.in_subquery(conversation_work()),
            )
            .filter(
                agent_capability_dispatch_outbox::Column::State
                    .eq(crate::capability_grant_store::DISPATCH_OUTBOX_PENDING),
            )
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        agent_capability_dispatch_outbox::Entity::update_many()
            .col_expr(
                agent_capability_dispatch_outbox::Column::State,
                Expr::value(crate::capability_grant_store::DISPATCH_OUTBOX_OUTCOME_UNKNOWN),
            )
            .col_expr(
                agent_capability_dispatch_outbox::Column::UpdatedAt,
                Expr::value(now),
            )
            .filter(
                agent_capability_dispatch_outbox::Column::WorkId.in_subquery(conversation_work()),
            )
            .filter(
                agent_capability_dispatch_outbox::Column::State
                    .eq(crate::capability_grant_store::DISPATCH_OUTBOX_SENDING),
            )
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(crate::agent_action_store::STATUS_CANCELLED),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::ConversationId.eq(id))
            .filter(agent_action_item::Column::Status.is_in(UNDISPATCHED_ACTION_STATES))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::CancelRequestedAt,
                Expr::col(agent_action_item::Column::CancelRequestedAt).if_null(now),
            )
            .col_expr(
                agent_action_item::Column::CancelRequestedBy,
                Expr::value(actor),
            )
            .filter(agent_action_item::Column::ConversationId.eq(id))
            .filter(
                agent_action_item::Column::Status
                    .is_in(crate::usage_retention::UNRESOLVED_ACTION_STATES),
            )
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
