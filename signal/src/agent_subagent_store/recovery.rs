//! Reconcile original facts without executing tools or claiming another turn.
use super::*;
use crate::entity::agent_permission_resume as agent_permission;
use desk_diagnose_core::{
    session::{TurnState, WorkKind},
    subagent::state::TaskDependency,
};

impl SubAgentStore {
    pub async fn recovery_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run_row::Model>, DbErr> {
        if after_id < 0 || !(1..=32).contains(&limit) {
            return Err(invalid());
        }
        run_row::Entity::find()
            .filter(run_row::Column::Id.gt(after_id))
            .filter(run_row::Column::State.is_in([
                "queued",
                "running",
                "waiting_approval",
                "waiting_work",
                "waiting_source",
            ]))
            .order_by_asc(run_row::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
    }

    /// Cleanup remains available after owner eligibility changes. Every future
    /// planning claim still uses the ordinary owner, policy and source gates.
    pub async fn reconcile_task(&self, task_id: &str) -> Result<bool, DbErr> {
        let selected = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)?;
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::Id.eq(selected.id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut run = decode_run(&row)?;
        if run.state.is_terminal()
            || matches!(
                run.state,
                SubAgentState::Cancelling | SubAgentState::WaitingResource
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
        let stored = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
            .filter(session_row::Column::ActorId.eq(&run.actor_id))
            .filter(session_row::Column::DeviceId.eq(&run.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut session =
            PersistedAgentSession::decode_json(&stored.state_json).map_err(|_| invalid())?;
        run.validate_session(&session).map_err(|_| invalid())?;
        if session.version != stored.version
            || i64::try_from(session.lease_token).ok() != Some(stored.lease_token)
        {
            return Err(invalid());
        }
        task_context_on(&txn, &session).await?.ok_or_else(invalid)?;
        let now = chrono::Utc::now();
        let now_ms = now.timestamp_millis();
        if session.turn_state.is_active()
            && stored
                .lease_deadline
                .is_some_and(|deadline| deadline >= now)
        {
            return Ok(false);
        }
        let original_session = session.clone();
        let previous_revision = run.state_revision;
        crate::capability_grant_store::scheduled_recovery::reconcile_child_on(
            &txn,
            &mut session,
            u64::try_from(now_ms).map_err(|_| invalid())?,
        )
        .await?;
        let facts = super::facts::runtime_facts_on(&txn, &session, now_ms).await?;
        let mut dependencies = facts.dependencies;
        // An unclaimed decision belongs to the permission coordinator. A plain
        // delegated-task claim must not skip its exact continuation bridge.
        let decisions = agent_permission::Entity::find()
            .filter(agent_permission::Column::RunId.eq(&session.conversation_id))
            .filter(agent_permission::Column::ActorId.eq(&session.actor_id))
            .filter(agent_permission::Column::DeviceId.eq(&session.device_id))
            .filter(
                agent_permission::Column::InputRevision
                    .eq(i64::try_from(session.input_revision).map_err(|_| invalid())?),
            )
            .filter(agent_permission::Column::State.eq("pending"))
            .limit(65)
            .all(&txn)
            .await?;
        for decision in decisions {
            let dependency = TaskDependency::Approval {
                permission_request_id: decision.request_id,
            };
            if !dependencies.contains(&dependency) {
                dependencies.push(dependency);
            }
        }
        // Native Computer Action facts resume the finite task directly. Exact
        // command output first belongs to its existing tool-free interpreter.
        session.pending_auto_triggers.retain(|trigger| {
            trigger.kind == WorkKind::AgentExec && trigger.chain_id == session.chain_id
        });
        for trigger in &session.pending_auto_triggers {
            let dependency = TaskDependency::Work {
                work_id: format!("completion:{}", trigger.event_id),
            };
            if !dependencies.contains(&dependency) {
                dependencies.push(dependency);
            }
        }
        if session.turn_state.is_active() {
            session.lease_token = session
                .lease_token
                .checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(invalid)?;
            session.finish_turn(TurnState::Idle, now.to_rfc3339());
        }
        if now_ms >= run.binding.deadline_ms || now_ms >= group.limits.deadline_ms {
            run.fail("delegation_deadline_reached", &now.to_rfc3339())
                .map_err(|_| invalid())?;
            session.finish_turn(TurnState::Failed, now.to_rfc3339());
            session.pending_auto_triggers.clear();
        } else if group.source_admission == SourceAdmission::Closed {
            run.fail("delegation_source_closed", &now.to_rfc3339())
                .map_err(|_| invalid())?;
            session.finish_turn(TurnState::Failed, now.to_rfc3339());
            session.pending_auto_triggers.clear();
        } else {
            run.synchronize_dependencies(dependencies, false, &now.to_rfc3339())
                .map_err(|_| invalid())?;
        }
        if run.state.is_terminal() {
            let operation = format!(
                "terminal:{}:{}",
                run.binding.task_id, run.binding.control_revision
            );
            super::native_cancel::cancel_native_actions_on(
                &txn,
                &original_session,
                &operation,
                now_ms,
            )
            .await?;
        }
        // The session trigger and native delivery are one durable obligation.
        // Repair a lost publisher hint only for the exact original done command;
        // interpreted, cancelled or unrelated results cannot be rearmed.
        let mut delivery_rearmed = false;
        for pending in &session.pending_auto_triggers {
            use crate::{agent_exec_store, entity::agent_exec_task};
            let changed = agent_exec_task::Entity::update_many()
                .set(agent_exec_task::ActiveModel {
                    delivery_state: Set(agent_exec_store::DELIVERY_PENDING.into()),
                    ..Default::default()
                })
                .filter(agent_exec_task::Column::Id.eq(pending.work_id))
                .filter(agent_exec_task::Column::ConversationId.eq(&session.conversation_id))
                .filter(agent_exec_task::Column::ExecutionGeneration.eq(&pending.execution_id))
                .filter(agent_exec_task::Column::ToolCallId.eq(&pending.tool_call_id))
                .filter(agent_exec_task::Column::EventId.eq(&pending.event_id))
                .filter(agent_exec_task::Column::Status.eq(agent_exec_store::STATUS_DONE))
                .filter(
                    agent_exec_task::Column::DeliveryState.eq(agent_exec_store::DELIVERY_CONSUMED),
                )
                .exec(&txn)
                .await?;
            delivery_rearmed |= changed.rows_affected != 0;
        }
        if run.state_revision == previous_revision && session == original_session {
            txn.commit().await?;
            return Ok(delivery_rearmed);
        }
        if run.state_revision != previous_revision {
            replace_run_on(&txn, &row, &run, now_ms).await?;
            append_state_event_on(&txn, &group, &run, now_ms).await?;
        }
        // Rotating a lapsed lease and saving its facts is one CAS. No provider or
        // device I/O occurs while these control locks are held.
        session.version = stored.version.checked_add(1).ok_or_else(invalid)?;
        let mut update = session_row::Entity::update_many()
            .set(session_row::ActiveModel {
                state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
                version: Set(session.version),
                lease_token: Set(i64::try_from(session.lease_token).map_err(|_| invalid())?),
                lease_deadline: Set(None),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(session_row::Column::Id.eq(stored.id))
            .filter(session_row::Column::Version.eq(stored.version))
            .filter(session_row::Column::LeaseToken.eq(stored.lease_token));
        if original_session.turn_state.is_active() {
            update = update.filter(
                sea_orm::Condition::any()
                    .add(session_row::Column::LeaseDeadline.is_null())
                    .add(session_row::Column::LeaseDeadline.lt(now)),
            );
        }
        let changed = update.exec(&txn).await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        txn.commit().await?;
        Ok(true)
    }
}
