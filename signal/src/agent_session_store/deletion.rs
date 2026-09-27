//! Owner-authorized deletion fences the model turn before removing its history.
use super::*;
use crate::entity::{
    agent_action_item, agent_approval_delegation, agent_attachment,
    agent_capability_dispatch_outbox, agent_goal_open_request, agent_schedule,
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
            .check_surface(AgentSessionSurface::AiAssistant)
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
        agent_attachment::Entity::delete_many()
            .filter(agent_attachment::Column::ConversationId.eq(id))
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
        let now = chrono::Utc::now();
        let now_ms = now.timestamp_millis();
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
        agent_goal_open_request::Entity::delete_many()
            .filter(agent_goal_open_request::Column::ConversationId.eq(id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        agent_approval_delegation::Entity::delete_many()
            .filter(agent_approval_delegation::Column::ConversationId.eq(id))
            .exec(&txn)
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
        // Persist before removing history: reconnect and restart must retain cleanup intent.
        crate::entity::agent_file_recovery_cleanup::ActiveModel {
            conversation_id: Set(id.to_owned()),
            actor_id: Set(actor.to_owned()),
            device_id: Set(device.to_owned()),
            created_at_unix_ms: Set(now_ms),
            next_attempt_at_unix_ms: Set(now_ms),
            attempts: Set(0),
            lease_id: Set(None),
            lease_until_unix_ms: Set(None),
            completed_at_unix_ms: Set(None),
            last_error: Set(None),
        }
        .insert(&txn)
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
