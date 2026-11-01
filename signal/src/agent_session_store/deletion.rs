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
            if locked.rows_affected != 0 {
                return Err(internal("Conversation not found or not accessible"));
            }
            let now_ms = chrono::Utc::now().timestamp_millis();
            let removed = desk_diagnose_core::future::boxed(|| {
                crate::assistant_data_store::delete_retained_root_on(
                    &txn, actor, device, id, now_ms,
                )
            })
            .await
            .map_err(|error| error.into_agent_error())?;
            if !removed {
                return Err(internal("Conversation not found or not accessible"));
            }
            txn.commit().await.map_err(save_backend)?;
            self.delete_trace_later(id, actor);
            return Ok(None);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(id))
            .one(&txn)
            .await
            .map_err(save_backend)?
            .ok_or_else(|| internal("Conversation not found"))?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (permission_ends, current_request_id) = if row.conversation_schema_version
            == Some(i32::from(
                desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
            )) {
            let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|error| {
                internal(format!("Invalid current conversation snapshot: {error}"))
            })?;
            session
                .check_subject(actor, device)
                .map_err(|_| internal("Conversation not accessible"))?;
            session
                .check_surface(AgentSessionSurface::AiAssistant)
                .map_err(|_| internal("Conversation not accessible"))?;
            if session.conversation_id != id || !session.agent_role.is_main() {
                return Err(internal("Only a main conversation can be deleted"));
            }
            (
                crate::agent_subagent_store::close_root_on(&txn, &session, now_ms)
                    .await
                    .map_err(save_backend)?,
                session.current_request_id,
            )
        } else {
            let metadata =
                desk_diagnose_core::session::management::SessionManagementMetadata::decode(
                    &row.state_json,
                )
                .map_err(|_| internal("Invalid conversation metadata"))?;
            if !metadata.matches_subject(id, actor, device)
                || !metadata.is_main()
                || metadata.surface != AgentSessionSurface::AiAssistant
            {
                return Err(internal("Conversation not accessible"));
            }
            if crate::entity::agent_subagent_run::Entity::find()
                .filter(crate::entity::agent_subagent_run::Column::ChildConversationId.eq(id))
                .one(&txn)
                .await
                .map_err(save_backend)?
                .is_some()
            {
                return Err(internal("Only a main conversation can be deleted"));
            }
            (
                crate::agent_subagent_store::close_root_for_subject_on(
                    &txn, id, actor, device, now_ms,
                )
                .await
                .map_err(save_backend)?,
                metadata.current_request_id,
            )
        };
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
        stop_removed_work_on(&txn, id, actor, now_ms)
            .await
            .map_err(save_backend)?;
        agent_session::Entity::delete_many()
            .filter(agent_session::Column::Id.eq(row.id))
            .exec(&txn)
            .await
            .map_err(save_backend)?;
        desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::mark_manual_delete_on(&txn, actor, device, id, now_ms)
        })
        .await
        .map_err(|error| error.into_agent_error())?;
        txn.commit().await.map_err(save_backend)?;
        self.delete_trace_later(id, actor);
        permission_ends.submit(now_ms, crate::model_metrics::runtime::submit);
        Ok(current_request_id)
    }

    fn delete_trace_later(&self, id: &str, actor: &str) {
        let trace_store = crate::approval_trace::Store::new(self.db.clone());
        let deleted = id.to_owned();
        let owner = actor.to_owned();
        tokio::spawn(async move {
            let _ = trace_store.delete_for_session(&deleted, &owner).await;
        });
    }
}

fn save_backend(error: sea_orm::DbErr) -> AgentError {
    internal(format!("delete conversation: {error}"))
}

/// Reuse the original removal rules: unsent work ends, sent work keeps its receipts.
pub(super) async fn stop_removed_work_on(
    txn: &crate::config::connection::DatabaseTransaction,
    id: &str,
    actor: &str,
    now_ms: i64,
) -> Result<(), sea_orm::DbErr> {
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(|| sea_orm::DbErr::Custom("invalid clock".into()))?;
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
        .filter(agent_capability_dispatch_outbox::Column::WorkId.in_subquery(conversation_work()))
        .filter(
            agent_capability_dispatch_outbox::Column::State
                .eq(crate::capability_grant_store::DISPATCH_OUTBOX_PENDING),
        )
        .exec(txn)
        .await?;
    agent_capability_dispatch_outbox::Entity::update_many()
        .col_expr(
            agent_capability_dispatch_outbox::Column::State,
            Expr::value(crate::capability_grant_store::DISPATCH_OUTBOX_OUTCOME_UNKNOWN),
        )
        .col_expr(
            agent_capability_dispatch_outbox::Column::UpdatedAt,
            Expr::value(now),
        )
        .filter(agent_capability_dispatch_outbox::Column::WorkId.in_subquery(conversation_work()))
        .filter(
            agent_capability_dispatch_outbox::Column::State
                .eq(crate::capability_grant_store::DISPATCH_OUTBOX_SENDING),
        )
        .exec(txn)
        .await?;
    agent_action_item::Entity::update_many()
        .col_expr(
            agent_action_item::Column::Status,
            Expr::value(crate::agent_action_store::STATUS_CANCELLED),
        )
        .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
        .filter(agent_action_item::Column::ConversationId.eq(id))
        .filter(agent_action_item::Column::Status.is_in(UNDISPATCHED_ACTION_STATES))
        .exec(txn)
        .await?;
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
        .exec(txn)
        .await?;

    Ok(())
}
