//! End an original child wait without reclaiming planning or inventing native results.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::agent_subagent_store as children;
#[cfg(test)]
use crate::entity::agent_subagent_run as child_row;
use crate::entity::{
    agent_delegation_group as group_row, agent_schedule_run as run, agent_session,
};
use desk_diagnose_core::session::{PersistedAgentSession, TriggerOrigin, TurnState};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

impl ScheduleStore {
    pub(super) async fn children_wait_candidates(
        &self,
        after: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        if after < 0 || !(1..=32).contains(&limit) {
            return Err(ScheduleStoreError::Invalid);
        }
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after))
            .filter(run::Column::Status.eq("awaiting_children"))
            .filter(run::Column::FailureAccounted.eq(false))
            .filter(run::Column::FinishedAt.is_null())
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    /// Owner-only stop leaves finite children running until terminal or source
    /// expiry. Withdrawal closes child planning immediately; actual native work
    /// is then reconciled before releasing the original schedule's active slot.
    pub async fn settle_fresh_children_wait(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        self.settle_fresh_children_wait_with_observer(run_id, crate::model_metrics::runtime::submit)
            .await
    }

    async fn settle_fresh_children_wait_with_observer(
        &self,
        run_id: &str,
        mut submit: impl FnMut(desk_diagnose_core::model_observability::ObservationEvent),
    ) -> Result<bool, ScheduleStoreError> {
        let peek = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let snapshot: entity::Model = serde_json::from_str(&peek.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let txn = crate::db::begin_write(&self.db, entity::Entity).await?;
        let subject_withdrawn = false;
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&peek.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(peek.owner_user_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.kind != "fresh_task" || task.active_run_id.as_deref() != Some(run_id) {
            return Ok(false);
        }
        if snapshot.owner_user_id != peek.owner_user_id
            || snapshot.schedule_id != task.schedule_id
            || snapshot.target_device_id != task.target_device_id
            || snapshot.kind != "fresh_task"
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let work = run::Entity::find_by_id(peek.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.status != "awaiting_children"
            || work.failure_accounted
            || work.finished_at.is_some()
            || work.lease_owner.is_some()
            || work.lease_deadline.is_some()
            || work.attempt != 1
            || work.conversation_id != work.run_id
            || work.started_at.is_none()
        {
            return Ok(false);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if session.actor_id != work.owner_user_id.to_string()
            || row.actor_id != session.actor_id
            || session.device_id != snapshot.target_device_id
            || row.device_id != session.device_id
            || row.version != session.version
            || row.lease_token != session.lease_token as i64
            || row.lease_deadline.is_some()
            || session.conversation_id != work.run_id
            || !session.agent_role.is_main()
            || session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.input_revision != 1
            || session.current_request_id.as_deref() != Some(run_id)
            || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
            || !(matches!(session.turn_state, TurnState::Idle | TurnState::Cancelled)
                || (session.turn_state == TurnState::Failed
                    && session.is_subagent_result_turn()
                    && (session
                        .ready_subagent_notification
                        .as_ref()
                        .is_some_and(|notice| {
                            notice.accepted_response_message_id.is_none()
                                && notice.retry_after_ms.is_some()
                        })
                        || session
                            .ready_subagent_wait
                            .as_ref()
                            .is_some_and(|wait| wait.retry_after_ms.is_some()))
                    && session.terminal_error.as_ref().is_some_and(|error| {
                        error.kind == desk_agent_protocol::AgentErrorKind::ModelUnavailable
                            && error.retryable
                    })))
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let reference = work
            .result_ref
            .as_deref()
            .and_then(|value| {
                value
                    .strip_prefix("children:")
                    .or_else(|| value.strip_prefix("answer-children:"))
                    .or_else(|| {
                        session
                            .main_stopped
                            .then(|| value.strip_prefix("stopped-children:"))
                            .flatten()
                    })
            })
            .filter(|value| desk_diagnose_core::subagent::valid_id(value))
            .ok_or(ScheduleStoreError::Invalid)?;
        if !session.main_stopped
            && !work
                .result_ref
                .as_deref()
                .is_some_and(|value| value.starts_with("answer-children:"))
            && session
                .subagent_wait
                .as_ref()
                .is_none_or(|wait| wait.wait_id != reference)
            && session
                .ready_subagent_wait
                .as_ref()
                .is_none_or(|wait| wait.wait_id != reference)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let groups = group_row::Entity::find()
            .filter(group_row::Column::RootConversationId.eq(run_id))
            .filter(group_row::Column::SourceOccurrenceId.eq(run_id))
            .filter(group_row::Column::SourceScheduleId.eq(&work.schedule_id))
            .filter(group_row::Column::ActorId.eq(&session.actor_id))
            .filter(group_row::Column::DeviceId.eq(&session.device_id))
            .order_by_asc(group_row::Column::Id)
            .limit(2)
            .all(&txn)
            .await?;
        let [group_row] = groups.as_slice() else {
            return Err(ScheduleStoreError::Conflict);
        };
        if session.delegation_group_id.as_deref() != Some(group_row.group_id.as_str())
            || (work
                .result_ref
                .as_deref()
                .is_some_and(|value| value.starts_with("stopped-children:"))
                && (!session.main_stopped || reference != group_row.group_id))
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let group = children::decode_group(group_row)?;
        let source = children::decode_creation(group_row)?
            .scheduled_source
            .ok_or(ScheduleStoreError::Invalid)?;
        let now = super::authority::authority_now(&txn).await?;
        let authority_withdrawn = match Self::lock_delegation_source_authority(
            &txn,
            work.owner_user_id,
            &session.device_id,
            &source,
        )
        .await
        {
            Ok(_) => false,
            Err(
                ScheduleStoreError::Conflict
                | ScheduleStoreError::NotFound
                | ScheduleStoreError::BudgetExceeded,
            ) => true,
            Err(error) => return Err(error),
        };
        let tasks = children::group_children_on(&txn, &group).await?;
        let mut all_terminal = true;
        for task in &tasks {
            all_terminal &= children::decode_run(task)?.state.is_terminal();
        }
        let expired = now >= group.limits.deadline_ms;
        let cancelled =
            work.cancel_requested_at.is_some() || (session.main_stopped && all_terminal);
        let withdrawn = authority_withdrawn
            || subject_withdrawn
            || expired
            || group.source_admission
                == desk_diagnose_core::subagent::group::SourceAdmission::Closed;
        if !cancelled && !withdrawn {
            return Ok(false);
        }
        // Closing planning and native stop intents commit even while the real
        // command is still running. Returning false never rolls these back.
        let (_, mut permission_ends) =
            children::close_scheduled_group_on(&txn, group_row, now).await?;
        permission_ends.extend(
            desk_diagnose_core::model_observability::permission::PendingEnds::waiting(
                &session,
                if cancelled {
                    desk_diagnose_core::model_observability::PermissionOutcome::Cancelled
                } else if expired {
                    desk_diagnose_core::model_observability::PermissionOutcome::Expired
                } else {
                    desk_diagnose_core::model_observability::PermissionOutcome::Revoked
                },
            ),
        );
        let current_group = group_row::Entity::find_by_id(group_row.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let native = children::scheduled_native_on(&txn, &current_group).await?;
        if native == children::ScheduledNativeDisposition::Pending {
            txn.commit().await?;
            permission_ends.submit(now, &mut submit);
            return Ok(false);
        }

        let unknown = native == children::ScheduledNativeDisposition::Unknown;
        let outcome = if unknown {
            desk_agent_protocol::schedule::ScheduledRunStatus::OutcomeUnknown
        } else if cancelled {
            desk_agent_protocol::schedule::ScheduledRunStatus::Cancelled
        } else {
            desk_agent_protocol::schedule::ScheduledRunStatus::Failed
        };
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        if !session.main_stopped {
            desk_diagnose_core::subagent::control::stop_main_session(
                &mut session,
                &timestamp.to_rfc3339(),
            )
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        session.finish_turn(
            if unknown || !cancelled {
                TurnState::Failed
            } else {
                TurnState::Cancelled
            },
            timestamp.to_rfc3339(),
        );
        session.terminal_error = Some(desk_agent_protocol::AgentError {
            kind: if unknown {
                desk_agent_protocol::AgentErrorKind::TransportError
            } else if cancelled {
                desk_agent_protocol::AgentErrorKind::Cancelled
            } else if expired {
                desk_agent_protocol::AgentErrorKind::Timeout
            } else {
                desk_agent_protocol::AgentErrorKind::PermissionDenied
            },
            message: if unknown {
                "An original delegated action has an unknown outcome; automatic retry is forbidden"
            } else if cancelled {
                "Scheduled task stopped; original child records were retained"
            } else {
                "The original scheduled source expired or its authority was withdrawn"
            }
            .into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
        session.version = row
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        session.updated_at = timestamp.to_rfc3339();
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(session
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                version: Set(session.version),
                lease_token: Set(session.lease_token as i64),
                lease_deadline: Set(None),

                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .filter(agent_session::Column::LeaseDeadline.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result_ref = work.result_ref.clone();
        super::settlement::settle(super::settlement::Settlement {
            txn,
            work,
            now,
            outcome,
            offline_timeout: false,
            result_ref,
            error_kind: Some(
                if unknown {
                    "outcome_unknown"
                } else if cancelled {
                    "cancelled"
                } else if expired {
                    "child_wait_timeout"
                } else {
                    "source_withdrawn"
                }
                .into(),
            ),
        })
        .await?;
        permission_ends.submit(now, &mut submit);
        Ok(true)
    }
}

impl ScheduleStore {
    /// Late original results backfill genuine root receipts and close the marker.
    /// The original outcome, failure policy and source admission stay unchanged.
    pub(super) async fn reconcile_fresh_children_receipts(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let peek = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let snapshot: entity::Model = serde_json::from_str(&peek.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let txn = crate::db::begin_write(&self.db, entity::Entity).await?;

        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&peek.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(peek.owner_user_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.kind != "fresh_task"
            || snapshot.kind != "fresh_task"
            || snapshot.schedule_id != task.schedule_id
            || snapshot.owner_user_id != peek.owner_user_id
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let work = run::Entity::find_by_id(peek.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.status != "outcome_unknown"
            || !work.failure_accounted
            || work.finished_at.is_none()
            || work.receipts_reconciled_at.is_some()
            || work.lease_deadline.is_some()
            || work.conversation_id != work.run_id
            || !work.result_ref.as_deref().is_some_and(|value| {
                value.starts_with("children:")
                    || value.starts_with("stopped-children:")
                    || value.starts_with("answer-children:")
                    || value.starts_with("delegated-effects:")
            })
        {
            return Ok(false);
        }
        let groups = group_row::Entity::find()
            .filter(group_row::Column::RootConversationId.eq(run_id))
            .filter(group_row::Column::SourceOccurrenceId.eq(run_id))
            .filter(group_row::Column::SourceScheduleId.eq(&work.schedule_id))
            .filter(group_row::Column::ActorId.eq(work.owner_user_id.to_string()))
            .filter(group_row::Column::DeviceId.eq(&snapshot.target_device_id))
            .limit(2)
            .all(&txn)
            .await?;
        let [group] = groups.as_slice() else {
            return Err(ScheduleStoreError::Conflict);
        };
        if group.source_admission != "closed"
            || children::scheduled_native_on(&txn, group).await?
                != children::ScheduledNativeDisposition::Settled
        {
            return Ok(false);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if row.actor_id != work.owner_user_id.to_string()
            || row.actor_id != session.actor_id
            || row.device_id != snapshot.target_device_id
            || row.device_id != session.device_id
            || row.version != session.version
            || row.lease_token != session.lease_token as i64
            || row.lease_deadline.is_some()
            || !session.agent_role.is_main()
            || session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.input_revision != 1
            || session.current_request_id.as_deref() != Some(run_id)
            || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
            || !matches!(
                session.turn_state,
                TurnState::Idle | TurnState::Failed | TurnState::Cancelled
            )
            || !session.pending_auto_triggers.is_empty()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(&txn).await?;
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        if !session.execution_state.states().is_empty()
            || !session.unclosed_tool_call_ids().is_empty()
        {
            let context = super::TaskReceiptContext::load(&txn, &work).await?;
            let restored = crate::capability_grant_store::fresh_recovery::restore_completed_calls(
                &txn,
                &mut session,
                &context,
                &timestamp.to_rfc3339(),
            )
            .await?;
            if !restored
                || !session.execution_state.states().is_empty()
                || !session.unclosed_tool_call_ids().is_empty()
            {
                return Ok(false);
            }
            session.version = row
                .version
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?;
            session.updated_at = timestamp.to_rfc3339();
            let saved = agent_session::Entity::update_many()
                .set(agent_session::ActiveModel {
                    state_json: Set(session
                        .encode_json_for_storage()
                        .map_err(|_| ScheduleStoreError::Invalid)?),
                    version: Set(session.version),
                    updated_at: Set(timestamp),
                    ..Default::default()
                })
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(row.version))
                .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
                .filter(agent_session::Column::LeaseDeadline.is_null())
                .exec(&txn)
                .await?;
            if saved.rows_affected != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
        }
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                receipts_reconciled_at: Set(Some(now)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
            .filter(run::Column::Status.eq("outcome_unknown"))
            .filter(run::Column::FailureAccounted.eq(true))
            .filter(run::Column::ReceiptsReconciledAt.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        txn.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/model_metrics_scheduled_permissions.rs"
    ));
    use crate::entity::agent_task_authorization as authorization;
    use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};

    async fn original_run(db: &sea_orm::DatabaseConnection, root: &str) -> run::Model {
        run::Entity::find()
            .filter(run::Column::RunId.eq(root))
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn healthy_child_wait_is_not_a_completed_or_expired_occurrence() {
        let (db, schedule, _, parent, _) = children::scheduled_test_wait_fixture().await;
        let before = original_run(&db, &parent.conversation_id).await;
        assert!(
            !schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert_eq!(original_run(&db, &parent.conversation_id).await, before);
        assert_eq!(
            schedule
                .read(1, &before.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(parent.conversation_id.as_str())
        );
    }

    #[tokio::test]
    async fn cancel_closes_children_and_releases_the_original_active_slot_once() {
        let (db, schedule, _, parent, task_id) = children::scheduled_test_wait_fixture().await;
        let before = original_run(&db, &parent.conversation_id).await;
        run::Entity::update_many()
            .set(run::ActiveModel {
                cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert!(
            !schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let after = original_run(&db, &parent.conversation_id).await;
        assert_eq!(after.status, "cancelled");
        assert!(after.failure_accounted && after.finished_at.is_some());
        assert_eq!(after.started_at, before.started_at);
        assert_eq!(after.attempt, before.attempt);
        assert_eq!(after.lease_epoch, before.lease_epoch);
        assert_eq!(after.result_ref, before.result_ref);
        assert!(
            schedule
                .read(1, &after.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
        let child = child_row::Entity::find()
            .filter(child_row::Column::TaskId.eq(&task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.state, "cancelled");
        let group = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&child.group_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(group.source_admission, "closed");
        assert!(!group.parent_active);
        assert!(!group.creation_envelope_json.is_empty());
    }

    #[tokio::test]
    async fn main_only_stop_keeps_the_child_source_until_it_finishes_without_reviving_main() {
        let (db, schedule, store, parent, task_id) = children::scheduled_test_wait_fixture().await;
        store
            .stop_for_owner(
                &parent.conversation_id,
                &parent.actor_id,
                &parent.device_id,
                &AiAssistantStopControl {
                    client_request_id: "scheduled-main-only".into(),
                    expected_input_revision: parent.input_revision,
                    expected_control_revision: parent.control_revision,
                    subagent_choice: Some(SubAgentStopChoice::MainOnly),
                },
            )
            .await
            .unwrap();
        let stopped = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let saved = PersistedAgentSession::decode_json(&stopped.state_json).unwrap();
        assert!(saved.main_stopped);
        assert!(
            !schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let work = original_run(&db, &parent.conversation_id).await;
        assert_eq!(work.status, "awaiting_children");
        assert!(work.cancel_requested_at.is_none());
        let child = child_row::Entity::find()
            .filter(child_row::Column::TaskId.eq(&task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.state, "queued");
        children::complete_scheduled_test_child(&db, &task_id).await;
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let current = agent_session::Entity::find_by_id(stopped.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let current = PersistedAgentSession::decode_json(&current.state_json).unwrap();
        assert!(current.main_stopped);
        assert_eq!(current.lease_token, saved.lease_token);
        assert_eq!(current.input_revision, saved.input_revision);
        assert_eq!(current.control_revision, saved.control_revision);
        assert_eq!(current.turn_state, TurnState::Cancelled);
        assert_eq!(
            original_run(&db, &parent.conversation_id).await.status,
            "cancelled"
        );
    }

    #[tokio::test]
    async fn revoked_publication_finishes_the_wait_without_recording_an_owner_cancellation() {
        let (db, schedule, _, parent, _) = children::scheduled_test_wait_fixture().await;
        let work = original_run(&db, &parent.conversation_id).await;
        authorization::Entity::update_many()
            .set(authorization::ActiveModel {
                revoked_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                revoked_reason: Set(Some("owner_revoked".into())),
                ..Default::default()
            })
            .filter(authorization::Column::ScheduleId.eq(&work.schedule_id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let after = original_run(&db, &parent.conversation_id).await;
        assert_eq!(after.status, "failed");
        assert_eq!(after.error_kind.as_deref(), Some("source_withdrawn"));
        assert!(after.cancel_requested_at.is_none());
        assert!(
            schedule
                .read(1, &after.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn native_running_and_unknown_facts_survive_source_closure_without_replanning() {
        let (db, schedule, _, parent, task_id) = children::scheduled_test_wait_fixture().await;
        let child = child_row::Entity::find()
            .filter(child_row::Column::TaskId.eq(&task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        use crate::entity::agent_exec_task as native;
        let exec = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
        exec.create(
            "scheduled-native-action",
            "scheduled-native-generation",
            &child.child_conversation_id,
            "original-child-call",
            "original-host",
            chrono::Utc::now() + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
        exec.mark_running("scheduled-native-generation")
            .await
            .unwrap();
        let command = native::Entity::find()
            .filter(native::Column::ExecutionGeneration.eq("scheduled-native-generation"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let metric_aliases =
            seed_observed_schedule_waits(&db, &parent, &child.child_conversation_id).await;
        let original = original_run(&db, &parent.conversation_id).await;
        run::Entity::update_many()
            .set(run::ActiveModel {
                cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(original.id))
            .exec(&db)
            .await
            .unwrap();
        let mut metric_events = Vec::new();
        assert!(
            !schedule
                .settle_fresh_children_wait_with_observer(&parent.conversation_id, |event| {
                    metric_events.push(event)
                })
                .await
                .unwrap()
        );
        assert_observed_schedule_waits_ended(&metric_events, &metric_aliases);
        let before_unknown = original_run(&db, &parent.conversation_id).await;
        assert_eq!(before_unknown.status, "awaiting_children");
        let retained = native::Entity::find_by_id(command.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.cancel_requested_at.is_some());
        assert_eq!(retained.status, command.status);
        native::Entity::update_many()
            .set(native::ActiveModel {
                status: Set("unknown".into()),
                ..Default::default()
            })
            .filter(native::Column::Id.eq(command.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let unknown = original_run(&db, &parent.conversation_id).await;
        assert_eq!(unknown.status, "outcome_unknown");
        assert!(unknown.receipts_reconciled_at.is_none());
        assert!(
            schedule
                .read(1, &unknown.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
        assert!(
            !schedule
                .reconcile_late_task_receipts(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert_eq!(original_run(&db, &parent.conversation_id).await, unknown);
        let retained = native::Entity::find_by_id(command.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.cancel_requested_at.is_some());
        assert_eq!(retained.status, "unknown");
    }
    #[tokio::test]
    async fn settled_historical_child_receipts_close_only_the_marker_without_reviving_original_planning()
     {
        let (db, schedule, _, parent, _) = children::scheduled_test_wait_fixture().await;
        let before = original_run(&db, &parent.conversation_id).await;
        run::Entity::update_many()
            .set(run::ActiveModel {
                cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        // Exercise a historical uncertain outcome with all retained native facts
        // already settled. Receipt reconciliation must not rewrite that outcome.
        run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("outcome_unknown".into()),
                error_kind: Set(Some("outcome_unknown".into())),
                receipts_reconciled_at: Set(None),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        let unknown = original_run(&db, &parent.conversation_id).await;
        let root_before = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            schedule
                .reconcile_late_task_receipts(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert!(
            !schedule
                .reconcile_late_task_receipts(&parent.conversation_id)
                .await
                .unwrap()
        );
        let after = original_run(&db, &parent.conversation_id).await;
        assert_eq!(after.status, unknown.status);
        assert_eq!(after.error_kind, unknown.error_kind);
        assert_eq!(after.failure_accounted, unknown.failure_accounted);
        assert_eq!(after.finished_at, unknown.finished_at);
        assert_eq!(after.result_ref, unknown.result_ref);
        assert_eq!(after.lease_epoch, unknown.lease_epoch);
        assert!(after.receipts_reconciled_at.is_some());
        let root_after = agent_session::Entity::find_by_id(root_before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root_before, root_after);
        let current = PersistedAgentSession::decode_json(&root_after.state_json).unwrap();
        assert!(current.main_stopped && !current.turn_state.is_active());
        assert!(
            schedule
                .read(1, &after.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
    }
}
