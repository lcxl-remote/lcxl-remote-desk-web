//! Commit an AI draft and its original tool-result receipt under one turn fence.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_session;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ToolCall},
    model_observability::{
        InputIssue, PermissionOutcome, Stage, StageOutcome, tool::ToolObservation,
    },
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
impl ScheduleStore {
    pub(crate) async fn manage_from_session(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
        observation: &ToolObservation,
    ) -> Result<String, ScheduleStoreError> {
        let owner: i32 = session
            .actor_id
            .parse()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let action =
            desk_diagnose_core::schedule::management_tools::parse(session, call, observation)
                .map_err(|message| match message {
                    "schedule management is unavailable" | "schedule proposal is unavailable" => {
                        ScheduleStoreError::Invalid
                    }
                    message => ScheduleStoreError::ToolInput(message),
                })?;
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        if owner != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID {
            return Err(ScheduleStoreError::NotFound);
        }
        let target = super::model_management::lock_target(&txn, owner, session, &action).await?;
        let locked =
            agent_session::Entity::update_many()
                .col_expr(
                    agent_session::Column::Version,
                    sea_orm::sea_query::Expr::col(agent_session::Column::Version),
                )
                .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
                .filter(agent_session::Column::ActorId.eq(&session.actor_id))
                .filter(agent_session::Column::DeviceId.eq(&session.device_id))
                .filter(agent_session::Column::Version.eq(session.version))
                .filter(agent_session::Column::LeaseToken.eq(
                    i64::try_from(session.lease_token).map_err(|_| ScheduleStoreError::Invalid)?,
                ))
                .exec(&txn)
                .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let now = desk_assistant_data_store::schedule::authority_now(&txn).await?;
        if row.state_json
            != session
                .encode_json_for_storage()
                .map_err(|_| ScheduleStoreError::Invalid)?
            || row
                .lease_deadline
                .is_none_or(|deadline| deadline.timestamp_millis() <= now)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let parents: Vec<_> = session
            .conversation
            .iter()
            .filter(|message| {
                message.role == ChatRole::Assistant
                    && message.tool_calls.iter().any(|tool| {
                        tool.id == call.id
                            && tool.name == call.name
                            && tool.arguments_json == call.arguments_json
                    })
            })
            .collect();
        if parents.len() != 1 || !session.unclosed_tool_call_ids().contains(&call.id) {
            return Err(ScheduleStoreError::Conflict);
        }
        if desk_diagnose_core::model_controls::is_control(&call.name) {
            crate::model_egress_store::SignalModelEgressStore::validate_control_on(
                &txn, session, call,
            )
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        let parent = parents[0]
            .data_envelope
            .as_ref()
            .ok_or(ScheduleStoreError::Invalid)?;
        use desk_diagnose_core::schedule::management_tools::Action;
        let mut awaiting_review = None;
        let (content, source) = match action {
            Action::Create(draft) => {
                desk_diagnose_core::schedule::validate_publication(
                    &draft.spec,
                    now,
                    draft.kind
                        == desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume,
                )
                .map_err(|_| {
                    observation.reject(Stage::Preflight, InputIssue::Precondition);
                    ScheduleStoreError::ToolInput(
                        desk_diagnose_core::schedule::proposal::NO_FUTURE_OCCURRENCE,
                    )
                })?;
                if draft.kind
                    == desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume
                {
                    super::check_resume_retention(&txn, &draft.spec, now)
                        .await
                        .inspect_err(|error| {
                            if matches!(error, ScheduleStoreError::ExceedsSessionRetention { .. }) {
                                observation.reject(Stage::Preflight, InputIssue::Precondition);
                            }
                        })?;
                }
                observation.stage(Stage::Preflight, StageOutcome::Passed);
                let mut task = desk_diagnose_core::future::send_phase(|| {
                    Self::create_draft_on(&txn, owner, &draft, now, Some(session))
                })
                .await?;
                desk_diagnose_core::future::send_phase(|| {
                    Self::record_prompt_retention_on(&txn, &mut task, session, parents[0])
                })
                .await?;
                let message = if task.kind == "conversation_resume" {
                    // A continuation needs no approval: enable it in this turn's
                    // transaction without writing to the running session.
                    task = Self::activate_on(&txn, owner, &task, false).await?;
                    "The conversation timer is enabled. It grants no tool permissions; its run approves device actions one at a time."
                } else {
                    awaiting_review = Some(task.schedule_id.clone());
                    "The AI reviewer or the owner decides this proposal in the application review dialog, and this turn waits for that decision. Do not report success before it. No extra chat confirmation is needed. No tool permission is granted; every run approves device actions one at a time."
                };
                (serde_json::json!({"schedule_id":task.schedule_id,"kind":task.kind,"state":task.status,
                    "next_run_at_utc_ms":task.next_run_at,"awaiting_decision":awaiting_review.is_some(),
                    "message":message}).to_string(), "schedule_proposal")
            }
            Action::List { after, limit } => {
                observation.stage(Stage::Preflight, StageOutcome::Passed);
                (
                    super::model_management::list(&txn, owner, session, after, limit, now).await?,
                    "schedule_query",
                )
            }
            Action::Cancel {
                expected_revision, ..
            } => (
                super::model_management::cancel(&txn, target, expected_revision, now, observation)
                    .await?,
                "schedule_cancellation",
            ),
        };
        let envelope = desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            Some(parent),
            &call.id,
            &content,
            source,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
        let mut next = session.clone();
        let mut row = row;
        if awaiting_review.is_some()
            && crate::agent_approval_store::open_for_schedule_proposal_on(
                &txn,
                &row,
                &mut next,
                u64::try_from(now).map_err(|_| ScheduleStoreError::Invalid)?,
            )
            .await?
        {
            // The audit event advanced the row; the receipt follows it.
            row = agent_session::Entity::find_by_id(row.id)
                .one(&txn)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
        }
        next.pending_schedule_review = awaiting_review;
        let mut message =
            ChatMessage::tool_result(format!("schedule-proposal:{}", call.id), &call.id, content);
        message.turn_id = next.current_turn_id.clone();
        message.data_envelope = envelope;
        next.conversation.push(message);
        next.version = next
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        next.updated_at = timestamp.to_rfc3339();
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                version: Set(next.version),
                state_json: Set(next
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        txn.commit().await?;
        *session = next;
        if session.pending_schedule_review.is_some() {
            observation.permission(PermissionOutcome::Waiting);
        }
        Ok(format!("schedule-proposal:{}", call.id))
    }
}
