//! Claims use agent_session's existing lease; task rows have no executor lease.
use super::*;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::{
    seam::ClaimTurnParams,
    session::TriggerOrigin,
    subagent::{creation::TaskCreationEnvelope, state::PlanningFence},
};

#[derive(Debug, Clone)]
pub struct ClaimedSubAgentTurn {
    pub session: PersistedAgentSession,
    pub creation: TaskCreationEnvelope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentClaimBlock {
    NotReady,
    SourcePaused,
    RecoveryRequired,
    DeadlineReached,
}

pub enum SubAgentClaimOutcome {
    Claimed(Box<ClaimedSubAgentTurn>),
    Blocked(SubAgentClaimBlock),
}

impl SubAgentStore {
    /// Params are authored by the ordinary owner/device runtime after PDP and
    /// model preflight. The child never inherits a main input's selected scopes.
    pub async fn claim_child(
        &self,
        params: &ClaimTurnParams,
        task_id: &str,
        expected: PlanningFence,
        model_destination: &DestinationIdentity,
    ) -> Result<SubAgentClaimOutcome, DbErr> {
        if params.trigger_origin != TriggerOrigin::DelegatedTask
            || !desk_diagnose_core::subagent::valid_id(&params.turn_id)
            || !desk_diagnose_core::subagent::valid_id(task_id)
        {
            return Err(invalid());
        }
        let now = chrono::DateTime::parse_from_rfc3339(&params.now)
            .map_err(|_| invalid())?
            .with_timezone(&chrono::Utc);
        // A selector read determines lock order only. Authority is re-read after
        // owner, root and child control locks (or the SQLite writer lock).
        let selected = desk_diagnose_core::future::boxed(|| async {
            run_row::Entity::find()
                .filter(run_row::Column::TaskId.eq(task_id))
                .filter(run_row::Column::ChildConversationId.eq(&params.conversation_id))
                .filter(run_row::Column::ActorId.eq(&params.actor_id))
                .filter(run_row::Column::DeviceId.eq(&params.device_id))
                .one(&self.db)
                .await
        })
        .await?
        .ok_or_else(|| invalid())?;
        let txn = desk_diagnose_core::future::boxed(|| async {
            crate::db::begin_write(&self.db, session_row::Entity).await
        })
        .await?;

        desk_diagnose_core::future::boxed(|| async {
            parent_on(
                &txn,
                &selected.root_conversation_id,
                &params.actor_id,
                &params.device_id,
            )
            .await
        })
        .await?;
        let history = desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::native_scope_on(
                &txn,
                &params.actor_id,
                &params.device_id,
                &selected.root_conversation_id,
            )
        })
        .await?;
        let row = desk_diagnose_core::future::boxed(|| async {
            run_row::Entity::find()
                .filter(run_row::Column::TaskId.eq(task_id))
                .filter(run_row::Column::RootConversationId.eq(&selected.root_conversation_id))
                .filter(run_row::Column::ChildConversationId.eq(&params.conversation_id))
                .filter(run_row::Column::ActorId.eq(&params.actor_id))
                .filter(run_row::Column::DeviceId.eq(&params.device_id))
                .one(&txn)
                .await
        })
        .await?
        .ok_or_else(|| invalid())?;
        let mut run = decode_run(&row)?;
        let source = desk_diagnose_core::future::boxed(|| async {
            group_row::Entity::find()
                .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
                .filter(group_row::Column::RootConversationId.eq(&run.binding.root_conversation_id))
                .filter(group_row::Column::ActorId.eq(&params.actor_id))
                .filter(group_row::Column::DeviceId.eq(&params.device_id))
                .one(&txn)
                .await
        })
        .await?
        .ok_or_else(|| invalid())?;
        let group = current_group_on(&txn, &source).await?;
        let creation: TaskCreationEnvelope =
            serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
        creation
            .validate_task(&run.binding)
            .map_err(|_| invalid())?;
        if creation.source != decode_creation(&source)?
            || creation.source.model_destination != *model_destination
            || group.source != run.binding.source
            || group.source_epoch != run.binding.source_epoch
            || run.fence() != expected
            || group.source_admission == SourceAdmission::Closed
        {
            return Err(invalid());
        }
        if run.state.is_terminal() || run.state == SubAgentState::Cancelling {
            return Ok(SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady));
        }
        if group.source_admission == SourceAdmission::Paused || run.source_paused {
            return Ok(SubAgentClaimOutcome::Blocked(
                SubAgentClaimBlock::SourcePaused,
            ));
        }
        desk_diagnose_core::future::boxed(|| async {
            super::scheduled_source::current_scheduled_source_on(&txn, &source).await
        })
        .await?;
        if !desk_diagnose_core::future::boxed(|| super::candidates::is_worker_head_on(&txn, &row))
            .await?
        {
            return Ok(SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady));
        }
        desk_diagnose_core::future::send_phase(|| async {
            let session_row = desk_diagnose_core::future::boxed(|| async {
                session_row::Entity::find()
                    .filter(session_row::Column::ConversationId.eq(&params.conversation_id))
                    .filter(session_row::Column::ActorId.eq(&params.actor_id))
                    .filter(session_row::Column::DeviceId.eq(&params.device_id))
                    .one(&txn)
                    .await
            })
            .await?
            .ok_or_else(|| invalid())?;
            let mut session = PersistedAgentSession::decode_json(&session_row.state_json)
                .map_err(|_| invalid())?;
            run.validate_session(&session).map_err(|_| invalid())?;
            match crate::issue_connections::check_work_on(
                &txn,
                &session,
                false,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            {
                Ok(()) => {}
                Err(desk_assistant_data_store::AssistantStoreError::Inaccessible) => {
                    return Ok(SubAgentClaimOutcome::Blocked(
                        SubAgentClaimBlock::SourcePaused,
                    ));
                }
                Err(_) => return Err(invalid()),
            }

            if session.version != session_row.version
                || i64::try_from(session.lease_token).ok() != Some(session_row.lease_token)
                || session.response_locale != creation.response_locale
                || session.delegated_owner_requirement.as_deref()
                    != Some(&creation.source.owner_requirement)
            {
                return Err(invalid());
            }
            if now.timestamp_millis() >= run.binding.deadline_ms
                || now.timestamp_millis() >= group.limits.deadline_ms
            {
                run.fail("delegation_deadline_reached", &params.now)
                    .map_err(|_| invalid())?;
                desk_diagnose_core::future::boxed(|| async {
                    replace_run_on(&txn, &row, &run, now.timestamp_millis()).await
                })
                .await?;
                let permission_ends = desk_diagnose_core::future::boxed(|| async {
                    synchronize_control_on(&txn, &run, now.timestamp_millis(), true).await
                })
                .await?;
                desk_diagnose_core::future::boxed(|| async {
                    append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await
                })
                .await?;
                if let Some(history) = &history {
                    desk_diagnose_core::future::boxed(|| history.capture_on(&txn)).await?;
                }
                desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
                permission_ends.submit(
                    now.timestamp_millis(),
                    crate::model_metrics::runtime::submit,
                );
                return Ok(SubAgentClaimOutcome::Blocked(
                    SubAgentClaimBlock::DeadlineReached,
                ));
            }
            if session.turn_state.is_active()
                || run.state != SubAgentState::Queued
                || !run.dependencies.is_empty()
            {
                return Ok(SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady));
            }
            // Explicit adjustment/resume may leave original receipt reconciliation
            // work behind. It cannot be replaced by another planning turn.
            if !session.unclosed_tool_call_ids().is_empty()
                || !session.execution_state.states().is_empty()
            {
                return Ok(SubAgentClaimOutcome::Blocked(
                    SubAgentClaimBlock::RecoveryRequired,
                ));
            }
            let charged = group
                .budget
                .charged
                .checked_add(group.budget.outstanding)
                .map_err(|_| invalid())?;
            let child_charged = group
                .budget
                .child_charged
                .checked_add(group.budget.child_outstanding)
                .map_err(|_| invalid())?;
            if !group.limits.total.has_model_capacity(charged)
                || !group
                    .limits
                    .child_ceiling()
                    .has_model_capacity(child_charged)
            {
                return Err(invalid());
            }
            if desk_diagnose_core::future::boxed(|| async {
                super::source_context::child_pending_decision_on(&txn, &session).await
            })
            .await?
                || session
                    .permission_requests
                    .iter()
                    .any(|request| !request.state.is_terminal())
                || session
                    .pending_auto_triggers
                    .iter()
                    .any(|trigger| trigger.chain_id == session.chain_id)
            {
                return Ok(SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady));
            }
            if row.workbench_id.is_some() && row.workbench_claimed_at_ms.is_none() {
                let policy =
                    desk_diagnose_core::future::boxed(|| crate::subagent_policy::read(&txn))
                        .await?;
                if !desk_diagnose_core::future::boxed(|| {
                    desk_assistant_data_store::worker_request::workbench_has_slot_on(
                        &txn,
                        &row,
                        policy.limits.max_unfinished_per_root,
                    )
                })
                .await?
                {
                    return Ok(SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady));
                }
            }
            session
                .lease_token
                .checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| invalid())?;
            session
                .begin_turn(
                    &params.turn_id,
                    params.request_id.clone(),
                    params.connection_id.clone(),
                    params.policy_revision,
                    params.current_pdp_scope.clone(),
                    &params.now,
                )
                .map_err(|_| invalid())?;
            session.adopt_trigger(TriggerOrigin::DelegatedTask, &params.turn_id);
            run.claim_planning(expected, now.timestamp_millis(), &params.now)
                .map_err(|_| invalid())?;
            desk_diagnose_core::future::boxed(|| async {
                replace_run_on(&txn, &row, &run, now.timestamp_millis()).await
            })
            .await?;
            let version = session_row
                .version
                .checked_add(1)
                .ok_or_else(|| invalid())?;
            session.version = version;
            let changed = desk_diagnose_core::future::boxed(|| async {
                session_row::Entity::update_many()
                .set(session_row::ActiveModel {
                    state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
                    version: Set(version),
                    lease_token: Set(i64::try_from(session.lease_token).map_err(|_| invalid())?),
                    lease_deadline: Set(Some(now + chrono::Duration::seconds(90))),
                    updated_at: Set(now),

                    ..Default::default()
                })
                .filter(session_row::Column::Id.eq(session_row.id))
                .filter(session_row::Column::Version.eq(session_row.version))
                .filter(session_row::Column::LeaseToken.eq(session_row.lease_token))
                .exec(&txn)
                .await
            })
            .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
            desk_diagnose_core::future::boxed(|| async {
                append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await
            })
            .await?;
            if let Some(history) = &history {
                desk_diagnose_core::future::boxed(|| history.capture_on(&txn)).await?;
            }
            desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
            Ok(SubAgentClaimOutcome::Claimed(Box::new(
                ClaimedSubAgentTurn { session, creation },
            )))
        })
        .await
    }
}
