//! Source-qualified resource waits and deadline settlement never replay actions.
use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_diagnose_core::subagent::{
    SubAgentWaitReason, runtime::RuntimeTurn, state::TaskDependency,
};

async fn begin_candidate_control(
    store: &SubAgentStore,
    selected: &run_row::Model,
    require_admission: bool,
) -> Result<DatabaseTransaction, DbErr> {
    let txn = crate::db::begin_write(&store.db, session_row::Entity).await?;
    if require_admission {
        parent_on(
            &txn,
            &selected.root_conversation_id,
            &selected.actor_id,
            &selected.device_id,
        )
        .await?;
    }
    Ok(txn)
}

impl SubAgentStore {
    pub async fn expired_task_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run_row::Model>, DbErr> {
        if !(1..=32).contains(&limit) {
            return Err(invalid());
        }
        run_row::Entity::find()
            .filter(run_row::Column::Id.gt(after_id))
            .filter(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
            .filter(run_row::Column::DeadlineMs.lte(chrono::Utc::now().timestamp_millis()))
            .order_by_asc(run_row::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
    }

    pub async fn expire_task(&self, task_id: &str) -> Result<bool, DbErr> {
        self.expire_task_at(task_id, chrono::Utc::now()).await
    }

    pub(crate) async fn expire_task_at(
        &self,
        task_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, DbErr> {
        let selected = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)?;
        let txn = begin_candidate_control(self, &selected, false).await?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::Id.eq(selected.id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut run = decode_run(&row)?;
        if run.state.is_terminal()
            || run.state == SubAgentState::Cancelling
            || now.timestamp_millis() < run.binding.deadline_ms
        {
            return Ok(false);
        }
        let source = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let group = decode_group(&source)?;
        run.fail("delegation_deadline_reached", &now.to_rfc3339())
            .map_err(|_| invalid())?;
        replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
        let permission_ends =
            synchronize_control_on(&txn, &run, now.timestamp_millis(), true).await?;
        append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
        txn.commit().await?;
        permission_ends.submit(
            now.timestamp_millis(),
            crate::model_metrics::runtime::submit,
        );
        Ok(true)
    }

    /// This pre-claim transition is conditional on the exact prepared snapshot.
    /// A racing claim, control, approval, pause or adjustment remains untouched.
    pub async fn defer_child_candidate(
        &self,
        candidate: &RuntimeTurn,
        resource: Option<SubAgentWaitReason>,
        terminal_reason: Option<&str>,
    ) -> Result<bool, DbErr> {
        let RuntimeTurn::Child {
            session,
            run: expected,
            ..
        } = candidate
        else {
            return Ok(false);
        };
        candidate.validate().map_err(|_| invalid())?;
        if resource.is_some() == terminal_reason.is_some()
            || terminal_reason.is_some_and(|reason| {
                !matches!(
                    reason,
                    "delegated_model_rejected" | "delegated_policy_rejected"
                )
            })
        {
            return Err(invalid());
        }
        let selected = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&expected.binding.task_id))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)?;
        let txn = begin_candidate_control(self, &selected, true).await?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::Id.eq(selected.id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut run = decode_run(&row)?;
        let source = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let group = decode_group(&source)?;
        let stored = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let current =
            PersistedAgentSession::decode_json(&stored.state_json).map_err(|_| invalid())?;
        if run.state != SubAgentState::Queued
            || run.fence() != expected.fence()
            || group.source_admission != SourceAdmission::Open
            || current != *session
            || current.turn_state.is_active()
            || !current.unclosed_tool_call_ids().is_empty()
            || !current.execution_state.states().is_empty()
            || current
                .permission_requests
                .iter()
                .any(|request| !request.state.is_terminal())
        {
            return Ok(false);
        }
        let now = chrono::Utc::now();
        if let Some(reason) = terminal_reason {
            run.fail(reason, &now.to_rfc3339()).map_err(|_| invalid())?;
        } else {
            run.set_dependencies(
                vec![TaskDependency::Resource {
                    reason: resource.ok_or_else(invalid)?,
                }],
                &now.to_rfc3339(),
            )
            .map_err(|_| invalid())?;
        }
        replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
        let mut permission_ends =
            desk_diagnose_core::model_observability::permission::PendingEnds::default();
        if resource.is_some() {
            run_row::Entity::update_many()
                .set(run_row::ActiveModel {
                    next_attempt_at_ms: Set(Some(
                        now.timestamp_millis()
                            .checked_add(30_000)
                            .ok_or_else(invalid)?,
                    )),
                    ..Default::default()
                })
                .filter(run_row::Column::Id.eq(row.id))
                .filter(run_row::Column::StateRevision.eq(run.state_revision as i64))
                .exec(&txn)
                .await?;
        } else {
            permission_ends =
                synchronize_control_on(&txn, &run, now.timestamp_millis(), false).await?;
        }
        append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
        txn.commit().await?;
        permission_ends.submit(
            now.timestamp_millis(),
            crate::model_metrics::runtime::submit,
        );
        Ok(true)
    }

    /// The caller checked current target readiness outside the writer. Model
    /// capacity is checked again only at the actual physical provider boundary.
    pub async fn retry_resource_task(
        &self,
        task_id: &str,
        device_available: bool,
    ) -> Result<bool, DbErr> {
        let selected = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)?;
        let txn = begin_candidate_control(self, &selected, true).await?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::Id.eq(selected.id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut run = decode_run(&row)?;
        let now = chrono::Utc::now();
        if run.state != SubAgentState::WaitingResource
            || row
                .next_attempt_at_ms
                .is_none_or(|due| due > now.timestamp_millis())
            || !device_available
            || now.timestamp_millis() >= run.binding.deadline_ms
            || !matches!(
                run.dependencies.as_slice(),
                [TaskDependency::Resource {
                    reason: SubAgentWaitReason::ModelCapacity
                        | SubAgentWaitReason::DeviceUnavailable
                        | SubAgentWaitReason::WriterCapacity
                }]
            )
        {
            return Ok(false);
        }
        let source = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let group = decode_group(&source)?;
        if group.source_admission != SourceAdmission::Open
            || run.source_paused
            || group.source_epoch != run.binding.source_epoch
        {
            return Ok(false);
        }
        let stored = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let session =
            PersistedAgentSession::decode_json(&stored.state_json).map_err(|_| invalid())?;
        run.validate_session(&session).map_err(|_| invalid())?;
        if session.turn_state.is_active()
            || !session.unclosed_tool_call_ids().is_empty()
            || !session.execution_state.states().is_empty()
            || session
                .permission_requests
                .iter()
                .any(|request| !request.state.is_terminal())
        {
            return Ok(false);
        }
        run.set_dependencies(Vec::new(), &now.to_rfc3339())
            .map_err(|_| invalid())?;
        replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
        append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
        txn.commit().await?;
        Ok(true)
    }
}
