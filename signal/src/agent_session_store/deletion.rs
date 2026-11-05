//! Owner-authorized deletion fences the model turn before removing its history.
use super::*;
use crate::entity::{
    agent_action_item, agent_attachment, agent_capability_dispatch_outbox, agent_schedule,
};
use desk_assistant_data_store::main_request;
use desk_diagnose_core::goal::GoalRemovalReason;
use desk_diagnose_core::model_observability::permission::PendingEnds;
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
        if device.is_empty()
            && actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        {
            return Err(internal("Conversation not found or not accessible"));
        }
        let txn = crate::db::begin_write(&self.db, agent_session::Entity)
            .await
            .map_err(save_backend)?;

        let row = lock_deletion_row_on(&txn, id, actor, device).await?;
        let Some(row) = row else {
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
            self.delete_traces_later(vec![id.into()], actor);
            return Ok(None);
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let current = current_deletion_session(&row)?;
        let mut removed = vec![id.to_owned()];
        let mut permission_ends = PendingEnds::default();
        if let Some(root) = current
            .as_deref()
            .filter(|session| main_request::is_workbench_root(session))
        {
            for work_id in main_request::activated_workbench_conversations_on(&txn, root)
                .await
                .map_err(save_backend)?
            {
                let work = lock_deletion_row_on(&txn, &work_id, actor, "").await?;
                if let Some(work) = work {
                    let session = current_deletion_session(&work)?
                        .filter(|session| session.agent_role.workbench_id() == Some(id))
                        .filter(|session| {
                            session
                                .agent_role
                                .workbench_request_id()
                                .is_some_and(|request| {
                                    desk_diagnose_core::native_ids::workbench_work_conversation(
                                        actor, id, request,
                                    ) == work_id
                                })
                        })
                        .ok_or_else(|| internal("Invalid workbench work binding"))?;
                    let (ends, _) =
                        delete_live_session_on(&txn, &work, Some(&session), now_ms).await?;
                    permission_ends.extend(ends);
                    removed.push(work_id);
                } else if crate::assistant_data_store::delete_retained_root_on(
                    &txn, actor, "", &work_id, now_ms,
                )
                .await
                .map_err(|error| error.into_agent_error())?
                {
                    removed.push(work_id);
                }
            }
        }
        let (ends, current_request_id) =
            delete_live_session_on(&txn, &row, current.as_deref(), now_ms).await?;
        permission_ends.extend(ends);
        txn.commit().await.map_err(save_backend)?;
        self.delete_traces_later(removed, actor);
        permission_ends.submit(now_ms, crate::model_metrics::runtime::submit);
        Ok(current_request_id)
    }

    fn delete_traces_later(&self, conversations: Vec<String>, actor: &str) {
        let trace_store = crate::approval_trace::Store::new(self.db.clone());
        let owner = actor.to_owned();
        tokio::spawn(async move {
            for conversation in conversations {
                let _ = trace_store.delete_for_session(&conversation, &owner).await;
            }
        });
    }
}

async fn lock_deletion_row_on(
    txn: &crate::config::connection::DatabaseTransaction,
    id: &str,
    actor: &str,
    device: &str,
) -> Result<Option<agent_session::Model>, AgentError> {
    // Take the write lock before reading; stale workers cannot save after deletion.
    let locked = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::Version,
            Expr::col(agent_session::Column::Version),
        )
        .filter(agent_session::Column::ConversationId.eq(id))
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::DeviceId.eq(device))
        .exec(txn)
        .await
        .map_err(save_backend)?;
    match locked.rows_affected {
        0 => Ok(None),
        1 => agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(id))
            .one(txn)
            .await
            .map_err(save_backend)?
            .map(Some)
            .ok_or_else(|| internal("Conversation not found")),
        _ => Err(internal("Conversation not found or not accessible")),
    }
}

fn current_deletion_session(
    row: &agent_session::Model,
) -> Result<Option<Box<PersistedAgentSession>>, AgentError> {
    if row.conversation_schema_version
        != Some(i32::from(
            desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
        ))
    {
        return Ok(None);
    }
    let session = PersistedAgentSession::decode_boxed_json(&row.state_json)
        .map_err(|error| internal(format!("Invalid current conversation snapshot: {error}")))?;
    session
        .check_subject(&row.actor_id, &row.device_id)
        .map_err(|_| internal("Conversation not accessible"))?;
    session
        .check_surface(AgentSessionSurface::AiAssistant)
        .map_err(|_| internal("Conversation not accessible"))?;
    if session.conversation_id != row.conversation_id
        || !session.agent_role.is_main()
        || (row.device_id.is_empty()
            && !session.has_admitted_personal_input()
            && !main_request::is_workbench_root(&session))
    {
        return Err(internal("Only a main conversation can be deleted"));
    }
    Ok(Some(session))
}

async fn delete_live_session_on(
    txn: &crate::config::connection::DatabaseTransaction,
    row: &agent_session::Model,
    session: Option<&PersistedAgentSession>,
    now_ms: i64,
) -> Result<(PendingEnds, Option<String>), AgentError> {
    let id = row.conversation_id.as_str();
    let actor = row.actor_id.as_str();
    let device = row.device_id.as_str();
    let (permission_ends, current_request_id) = if let Some(session) = session {
        (
            crate::agent_subagent_store::close_root_on(txn, session, now_ms)
                .await
                .map_err(save_backend)?,
            session.current_request_id.clone().or_else(|| {
                session
                    .main_coordination
                    .as_ref()
                    .map(|lease| lease.batch_id.clone())
            }),
        )
    } else {
        let metadata = desk_diagnose_core::session::management::SessionManagementMetadata::decode(
            &row.state_json,
        )
        .map_err(|_| internal("Invalid conversation metadata"))?;
        if device.is_empty()
            || !metadata.matches_subject(id, actor, device)
            || !metadata.is_main()
            || metadata.surface != AgentSessionSurface::AiAssistant
        {
            return Err(internal("Conversation not accessible"));
        }
        if crate::entity::agent_subagent_run::Entity::find()
            .filter(crate::entity::agent_subagent_run::Column::ChildConversationId.eq(id))
            .one(txn)
            .await
            .map_err(save_backend)?
            .is_some()
        {
            return Err(internal("Only a main conversation can be deleted"));
        }
        (
            crate::agent_subagent_store::close_root_for_subject_on(txn, id, actor, device, now_ms)
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
        .exec(txn)
        .await
        .map_err(save_backend)?;
    agent_attachment::Entity::delete_many()
        .filter(agent_attachment::Column::ConversationId.eq(id))
        .exec(txn)
        .await
        .map_err(save_backend)?;
    // Do not cascade authorization or execution lineage. Late native and
    // provider receipts still need their original immutable bindings.
    // End every goal of this conversation; a running segment's worker gets
    // an idempotent cancellation when it settles.
    crate::agent_goal_store::cancel_conversation_goals_on(
        txn,
        id,
        GoalRemovalReason::ConversationRemoved,
        u64::try_from(now_ms).map_err(|_| internal("invalid clock"))?,
    )
    .await
    .map_err(save_backend)?;
    stop_removed_work_on(txn, id, actor, now_ms)
        .await
        .map_err(save_backend)?;
    agent_session::Entity::delete_many()
        .filter(agent_session::Column::Id.eq(row.id))
        .exec(txn)
        .await
        .map_err(save_backend)?;
    desk_diagnose_core::future::boxed(|| {
        crate::assistant_data_store::mark_manual_delete_on(txn, actor, device, id, now_ms)
    })
    .await
    .map_err(|error| error.into_agent_error())?;
    Ok((permission_ends, current_request_id))
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
