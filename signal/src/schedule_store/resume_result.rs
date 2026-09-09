//! Persist a committed loop result from this exact continuation without device replay.
use super::{ContinuationLease, ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_action_item, agent_exec_task, agent_schedule_run as run, agent_session};
use desk_agent_protocol::{AgentError, schedule::ScheduledRunStatus};
use desk_diagnose_core::{
    chat::ChatRole,
    dynamic_run::PermissionRequestState,
    file_scope::DirectoryConsentState,
    session::{
        AgentSessionSurface, ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
    },
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::Expr};

enum CommittedResult<'a> {
    Answer(&'a str),
    Failure(&'a AgentError),
    Permission(&'a str),
    Interrupted { cancelled: bool },
}

#[derive(Clone, Copy)]
enum SettlementLease {
    Live,
    Expired,
}
impl SettlementLease {
    fn accepts(self, deadline: Option<i64>, now: i64) -> bool {
        deadline.is_some_and(|deadline| match self {
            Self::Live => deadline > now,
            Self::Expired => deadline <= now,
        })
    }
}

impl ScheduleStore {
    /// `answer` is the returned LoopOutcome::Answered text, never a client assertion.
    /// Pending/unknown work is left for the dispatcher to wait for or reconcile.
    pub async fn finish_answered_continuation(
        &self,
        lease: ContinuationLease<'_>,
        answer: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.record_committed_continuation(
            lease,
            CommittedResult::Answer(answer),
            SettlementLease::Live,
        )
        .await
    }

    /// The exact error must already be persisted on the original failed turn.
    /// Transport loss with uncommitted session state still requires reconciliation.
    pub async fn finish_failed_continuation(
        &self,
        lease: ContinuationLease<'_>,
        error: &AgentError,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.record_committed_continuation(
            lease,
            CommittedResult::Failure(error),
            SettlementLease::Live,
        )
        .await
    }

    /// Called with the request ID returned by LoopOutcome::PermissionRequested.
    /// Waiting retains the active slot and does not account a terminal outcome.
    pub async fn await_continuation_permission(
        &self,
        lease: ContinuationLease<'_>,
        request_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.record_committed_continuation(
            lease,
            CommittedResult::Permission(request_id),
            SettlementLease::Live,
        )
        .await
    }

    /// Reconcile persisted results, permission pauses and original action records.
    /// Snapshot reads select evidence only; the settlement transaction rechecks
    /// the exact original turn, lease, input and all action families under locks.
    pub async fn recover_committed_continuation(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<Option<run::Model>, ScheduleStoreError> {
        let work = run::Entity::find()
            .filter(run::Column::OwnerUserId.eq(owner))
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.failure_accounted {
            return Ok(matches!(
                work.status.as_str(),
                "succeeded" | "failed" | "cancelled" | "outcome_unknown"
            )
            .then_some(work));
        }
        if work.status != "running"
            || work.attempt != 1
            || work.started_at.is_none()
            || work.finished_at.is_some()
        {
            return Ok(None);
        }
        let Some(node) = work.lease_owner.as_deref() else {
            return Ok(None);
        };
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&work.conversation_id))
            .filter(agent_session::Column::ActorId.eq(owner.to_string()))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let result = match session.turn_state {
            TurnState::Idle if session.terminal_permission_request_id.is_some() => {
                CommittedResult::Permission(
                    session.terminal_permission_request_id.as_deref().unwrap(),
                )
            }
            TurnState::Idle => {
                let Some(answer) = session
                    .conversation
                    .iter()
                    .rev()
                    .find(|message| message.role == ChatRole::Assistant)
                    .filter(|message| {
                        message.turn_id.as_deref() == Some(work.turn_id.as_str())
                            && message.tool_calls.is_empty()
                            && !message.text.trim().is_empty()
                    })
                else {
                    return Ok(None);
                };
                CommittedResult::Answer(&answer.text)
            }
            TurnState::Failed => {
                let Some(error) = session.terminal_error.as_ref() else {
                    return Ok(None);
                };
                CommittedResult::Failure(error)
            }
            TurnState::Running => CommittedResult::Interrupted {
                cancelled: work.cancel_requested_at.is_some(),
            },
            _ => return Ok(None),
        };
        self.record_committed_continuation(
            ContinuationLease {
                owner,
                run_id,
                node_id: node,
                run_epoch: work.lease_epoch,
                session_token: session.lease_token,
            },
            result,
            SettlementLease::Expired,
        )
        .await
        .map(Some)
    }

    async fn record_committed_continuation(
        &self,
        lease: ContinuationLease<'_>,
        result: CommittedResult<'_>,
        settlement_lease: SettlementLease,
    ) -> Result<run::Model, ScheduleStoreError> {
        let interrupted = matches!(result, CommittedResult::Interrupted { .. });
        let cancelled = matches!(result, CommittedResult::Interrupted { cancelled: true });
        let (mut outcome, state, expected_error, mut error_kind) = match result {
            CommittedResult::Interrupted { .. } => (
                if cancelled {
                    ScheduledRunStatus::Cancelled
                } else {
                    ScheduledRunStatus::Failed
                },
                TurnState::Running,
                None,
                Some(
                    if cancelled {
                        "cancelled"
                    } else {
                        "executor_interrupted"
                    }
                    .into(),
                ),
            ),
            CommittedResult::Answer(answer) if !answer.trim().is_empty() => {
                (ScheduledRunStatus::Succeeded, TurnState::Idle, None, None)
            }
            CommittedResult::Answer(_) => return Err(ScheduleStoreError::Invalid),
            CommittedResult::Permission(_) => (
                ScheduledRunStatus::AwaitingPermission,
                TurnState::Idle,
                None,
                None,
            ),
            CommittedResult::Failure(error) => (
                ScheduledRunStatus::Failed,
                TurnState::Failed,
                Some(error),
                Some(
                    serde_json::to_string(&error.kind)
                        .map_err(|_| ScheduleStoreError::Invalid)?
                        .trim_matches('"')
                        .to_string(),
                ),
            ),
        };
        let waiting = matches!(result, CommittedResult::Permission(_));
        let status = match result {
            CommittedResult::Interrupted { .. } => {
                if cancelled {
                    "cancelled"
                } else {
                    "failed"
                }
            }
            CommittedResult::Answer(_) => "succeeded",
            CommittedResult::Failure(_) => "failed",
            CommittedResult::Permission(_) => "awaiting_permission",
        };
        if lease.owner <= 0 || lease.run_epoch <= 0 || lease.session_token == 0 {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(lease.run_id))
            .filter(run::Column::OwnerUserId.eq(lease.owner))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.lease_owner.as_deref() != Some(lease.node_id) || work.lease_epoch != lease.run_epoch
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if let CommittedResult::Permission(id) = result
            && work.status == "awaiting_permission"
            && !work.failure_accounted
            && work.cancel_requested_at.is_none()
            && work.result_ref.as_deref().is_some_and(|reference| {
                reference == format!("permission:{id}") || reference == format!("directory:{id}")
            })
        {
            return Ok(work);
        }
        if work.failure_accounted {
            return if work.status == status && work.error_kind == error_kind {
                Ok(work)
            } else {
                Err(ScheduleStoreError::Conflict)
            };
        }
        // SQLite cannot upgrade a stale read snapshot after another writer commits.
        // This write holds task/session/run/action validation through settlement.
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(lease.owner))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&work.conversation_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
        if i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch) {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(&txn).await?;
        if work.status != "running"
            || work.cancel_requested_at.is_some() != cancelled
            || !settlement_lease.accepts(work.lease_deadline, now)
            || task.kind != "conversation_resume"
            || task.calc_version != desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION
            || task.contract_revision.is_some()
            || task.authorization_revision.is_some()
            || task.active_run_id.as_deref() != Some(lease.run_id)
            || task.source_conversation_id.as_deref() != Some(work.conversation_id.as_str())
            || task.requirement_revision != i64::try_from(session.input_revision).ok()
            || session.actor_id != lease.owner.to_string()
            || row.actor_id != session.actor_id
            || session.device_id != task.target_device_id
            || row.device_id != session.device_id
            || session.conversation_id != work.conversation_id
            || session.surface != AgentSessionSurface::DeviceAssistant
            || session.trigger_origin != TriggerOrigin::ScheduledContinuation
            || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
            || session.current_request_id.as_deref() != Some(lease.run_id)
            || session.version != row.version
            || session.lease_token != lease.session_token
            || i64::try_from(lease.session_token).ok() != Some(row.lease_token)
            || session.turn_state != state
            || session.terminal_error.as_ref() != expected_error
            || (!interrupted && session.execution_state != ExecutionState::None)
            || (state == TurnState::Idle && session.handled_input_seq != session.latest_input_seq)
            || !session.pending_auto_triggers.is_empty()
            || (!waiting
                && (session.permission_requests.iter().any(|request| {
                    request.input_revision == session.input_revision
                        && request.state == PermissionRequestState::Pending
                }) || session
                    .file_scope
                    .records()
                    .iter()
                    .any(|record| record.state == DirectoryConsentState::Pending)))
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let result_ref = match result {
            CommittedResult::Interrupted { .. } => {
                let session_deadline = row.lease_deadline.map(|at| at.timestamp_millis());
                if !matches!(settlement_lease, SettlementLease::Expired)
                    || session_deadline.is_some_and(|at| at > now)
                    || session.terminal_permission_request_id.is_some()
                {
                    return Err(ScheduleStoreError::Conflict);
                }
                None
            }
            CommittedResult::Answer(answer) => {
                let message = session
                    .conversation
                    .iter()
                    .rev()
                    .find(|message| message.role == ChatRole::Assistant)
                    .filter(|message| {
                        message.text == answer
                            && message.tool_calls.is_empty()
                            && message.turn_id.as_deref() == Some(work.turn_id.as_str())
                    })
                    .ok_or(ScheduleStoreError::Conflict)?;
                let reference = format!("message:{}", message.message_id);
                if reference.len() > 512 {
                    return Err(ScheduleStoreError::Invalid);
                }
                Some(reference)
            }
            CommittedResult::Failure(_) => None,
            CommittedResult::Permission(id) => {
                if matches!(settlement_lease, SettlementLease::Expired)
                    && session.terminal_permission_request_id.as_deref() != Some(id)
                {
                    return Err(ScheduleStoreError::Conflict);
                }
                Some(
                    desk_diagnose_core::schedule::permission_wait::reference(&session, id)
                        .ok_or(ScheduleStoreError::Conflict)?,
                )
            }
        };
        let before_recovery = session.clone();
        if interrupted {
            crate::capability_grant_store::scheduled_recovery::reconcile_on(
                &txn,
                &mut session,
                u64::try_from(now).map_err(|_| ScheduleStoreError::Invalid)?,
            )
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        let awaiting_result = interrupted && session.execution_state.is_running();
        let unknown_result = interrupted && session.execution_state.unknown().is_some();
        if unknown_result {
            outcome = ScheduledRunStatus::OutcomeUnknown;
            error_kind = Some("outcome_unknown".into());
        }
        // Do not infer an outcome from a budget reservation or a rendered answer.
        // All action families must have reached a known terminal transport state.
        let actions = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ConversationId.eq(&work.conversation_id))
            .all(&txn)
            .await?;
        if actions.iter().any(|action| {
            if (awaiting_result || unknown_result)
                && session
                    .execution_state
                    .tasks()
                    .into_iter()
                    .any(|identity| match identity.kind {
                        desk_diagnose_core::session::WorkKind::ComputerAction => {
                            identity.work_id == action.id
                        }
                        desk_diagnose_core::session::WorkKind::CapabilityProvider => {
                            identity.work_id == action.id
                                && identity.action_request_id == action.action_request_id
                        }
                        desk_diagnose_core::session::WorkKind::AgentExec => {
                            identity.action_request_id == action.action_request_id
                        }
                        _ => false,
                    })
                && matches!(
                    action.status.as_str(),
                    "capability_dispatching" | "capability_outcome_unknown"
                )
            {
                return false;
            }
            !matches!(
                action.status.as_str(),
                "done"
                    | "rejected"
                    | "expired"
                    | "cancelled"
                    | "capability_succeeded"
                    | "capability_failed"
                    | "capability_superseded_before_intent"
                    | "capability_revoked_before_intent"
            )
        }) {
            return Err(ScheduleStoreError::Conflict);
        }
        let executions = agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ConversationId.eq(&work.conversation_id))
            .all(&txn)
            .await?;
        if executions.iter().any(|execution| {
            execution.status != "done"
                && !((awaiting_result || unknown_result)
                    && session.execution_state.tasks().into_iter().any(|action| {
                        action.kind == desk_diagnose_core::session::WorkKind::AgentExec
                            && action.work_id == execution.id
                            && action.action_request_id == execution.exec_request_id
                            && action.execution_id == execution.execution_generation
                    }))
        }) {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(&txn).await?;
        if !settlement_lease.accepts(work.lease_deadline, now) {
            return Err(ScheduleStoreError::Conflict);
        }
        if interrupted {
            let at =
                chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
            if awaiting_result {
                if session == before_recovery {
                    return Err(ScheduleStoreError::Conflict);
                }
            } else {
                session.finish_turn(TurnState::Failed, at.to_rfc3339());
            }
            session.terminal_error = if awaiting_result {
                None
            } else {
                let kind = if cancelled && !unknown_result {
                    desk_agent_protocol::AgentErrorKind::Cancelled
                } else {
                    desk_agent_protocol::AgentErrorKind::Internal
                };
                let message = if unknown_result {
                    "The original scheduled action outcome is unknown; do not retry automatically."
                } else if cancelled {
                    "Scheduled turn cancelled; original action records were reconciled."
                } else {
                    "The scheduled executor lease expired before the turn completed; original action records were reconciled."
                };
                Some(AgentError {
                    kind,
                    message: message.into(),
                    retryable: false,
                    safe_for_model: false,
                    error_code: None,
                })
            };
            session.version = session
                .version
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?;
            desk_diagnose_core::image_input::strip_session_images(&mut session.conversation);
            desk_diagnose_core::visual_evidence::strip_previews(&mut session.visual_evidence);
            let changed = agent_session::Entity::update_many()
                .set(agent_session::ActiveModel {
                    state_json: Set(session
                        .encode_json_for_storage()
                        .map_err(|_| ScheduleStoreError::Invalid)?),
                    version: Set(session.version),
                    lease_deadline: Set(None),
                    updated_at: Set(at),
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
        }
        if awaiting_result {
            txn.commit().await?;
            return Ok(work);
        }
        crate::agent_session_store::permission_resume::settle_scheduled_decision_on(
            &txn,
            &session,
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?,
        )
        .await
        .map_err(|_| ScheduleStoreError::Conflict)?;
        if waiting {
            let changed = entity::Entity::update_many()
                .set(entity::ActiveModel {
                    revision: Set(task
                        .revision
                        .checked_add(1)
                        .ok_or(ScheduleStoreError::Invalid)?),
                    updated_at: Set(now),
                    ..Default::default()
                })
                .filter(entity::Column::Id.eq(task.id))
                .filter(entity::Column::Revision.eq(task.revision))
                .exec(&txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
            let changed = run::Entity::update_many()
                .set(run::ActiveModel {
                    status: Set("awaiting_permission".into()),
                    result_ref: Set(result_ref),
                    lease_deadline: Set(None),
                    updated_at: Set(now),
                    ..Default::default()
                })
                .filter(run::Column::Id.eq(work.id))
                .filter(run::Column::Status.eq("running"))
                .filter(run::Column::LeaseEpoch.eq(lease.run_epoch))
                .filter(run::Column::CancelRequestedAt.is_null())
                .exec(&txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
            let result = run::Entity::find_by_id(work.id)
                .one(&txn)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
            txn.commit().await?;
            return Ok(result);
        }
        super::settlement::settle(super::settlement::Settlement {
            txn,
            work,
            now,
            outcome,
            offline_timeout: false,
            error_kind,
            result_ref,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

    #[tokio::test]
    async fn permission_wait_keeps_active_slot_and_failure_count_without_a_live_executor() {
        let (store, queued, _, _) = super::super::resume_claim::tests::fixture().await;
        let schema = Schema::new(store.db.get_database_backend());
        store
            .db
            .execute(&schema.create_table_from_entity(agent_action_item::Entity))
            .await
            .unwrap();
        store
            .db
            .execute(&schema.create_table_from_entity(agent_exec_task::Entity))
            .await
            .unwrap();
        let claimed = store
            .claim_conversation_resume(super::super::ContinuationClaim {
                owner: 1,
                run_id: &queued.run_id,
                node_id: "node",
                lease_seconds: 90,
                policy_revision: 7,
                scope: AgentScope {
                    granted: vec![],
                    mode: ExecutionMode::ReadOnly,
                    expires_at: None,
                    policy_name: None,
                },
            })
            .await
            .unwrap();
        let mut session = claimed.session;
        session.turn_state = TurnState::Idle;
        session.handled_input_seq = session.latest_input_seq;
        session.permission_requests.push(serde_json::from_value(serde_json::json!({
            "schema_version":1, "request_id":"permission-wait", "input_revision":session.input_revision,
            "state":"pending", "created_at":"2026-09-06T00:00:00Z", "items":[{
                "item_id":"read", "provider_id":"desktop.session", "tool_name":"inspect_desktop_session",
                "expected_effect":"read_device", "resource_scope":["target:current_device"],
                "operation_scope":["observe"], "suggested_ttl_seconds":120,
                "suggested_max_uses":1, "reason":"Inspect current device"
            }]
        })).unwrap());
        session.permission_requests[0].validate().unwrap();
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let mut row: agent_session::ActiveModel = row.into();
        row.state_json = Set(session.encode_json_for_storage().unwrap());

        let before_session = row.update(&store.db).await.unwrap();
        let before_task = entity::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let held = || ContinuationLease {
            owner: 1,
            run_id: &claimed.run.run_id,
            node_id: "node",
            run_epoch: claimed.run.lease_epoch,
            session_token: session.lease_token,
        };
        assert!(
            store
                .await_continuation_permission(held(), "missing-request")
                .await
                .is_err()
        );
        let waiting = store
            .await_continuation_permission(held(), "permission-wait")
            .await
            .unwrap();
        assert_eq!(waiting.status, "awaiting_permission");
        assert_eq!(
            waiting.result_ref.as_deref(),
            Some("permission:permission-wait")
        );
        assert_eq!(
            store.continuation_candidates(0, 32).await.unwrap(),
            vec![waiting.clone()]
        );
        assert!(waiting.lease_deadline.is_none());
        assert!(waiting.finished_at.is_none());
        assert!(!waiting.failure_accounted);
        assert_eq!(
            store
                .await_continuation_permission(held(), "permission-wait")
                .await
                .unwrap(),
            waiting
        );
        assert!(
            store
                .await_continuation_permission(held(), "different-request")
                .await
                .is_err()
        );
        let mut stale = held();
        stale.run_epoch += 1;
        assert!(
            store
                .await_continuation_permission(stale, "permission-wait")
                .await
                .is_err()
        );
        assert!(
            store
                .finish_answered_continuation(held(), "not a terminal answer")
                .await
                .is_err()
        );
        let task = entity::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.active_run_id, before_task.active_run_id);
        assert_eq!(task.failure_state_json, before_task.failure_state_json);
        assert_eq!(task.revision, before_task.revision + 1);
        assert_eq!(
            agent_session::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before_session
        );
    }
}

#[cfg(test)]
mod terminal_recovery;
