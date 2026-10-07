//! Reclaim the original occurrence only after its durable child dependency resolves.
use super::{FreshTaskClaim, ScheduleStore, ScheduleStoreError};
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{PersistedAgentSession, TriggerOrigin, TurnState};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set};

impl ScheduleStore {
    /// Caller holds current owner/device and root control. Neither the dependency
    /// result nor this claim mints an operation grant or a new occurrence quota.
    pub(super) async fn claim_fresh_children_on(
        txn: &DatabaseTransaction,
        input: FreshTaskClaim<'_>,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        if input.owner <= 0
            || input.node_id.is_empty()
            || !(30..=300).contains(&input.lease_seconds)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(input.run_id))
            .filter(run::Column::OwnerUserId.eq(input.owner))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(input.run_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if work.status != "awaiting_children"
            || work.failure_accounted
            || work.finished_at.is_some()
            || work.cancel_requested_at.is_some()
            || work.lease_owner.is_some()
            || work.lease_deadline.is_some()
            || work.attempt != 1
            || work.started_at.is_none()
            || session.actor_id != input.owner.to_string()
            || row.actor_id != session.actor_id
            || row.device_id != session.device_id
            || row.version != session.version
            || row.lease_token != session.lease_token as i64
            || row.lease_deadline.is_some()
            || !(session.turn_state == TurnState::Idle
                || (session.turn_state == TurnState::Failed
                    && (session
                        .ready_subagent_notification
                        .as_ref()
                        .is_some_and(|notice| {
                            notice.accepted_response_message_id.is_none()
                                && notice.retry_after_ms.is_some()
                        })
                        || (session.subagent_result_only
                            && session
                                .ready_subagent_wait
                                .as_ref()
                                .is_some_and(|wait| wait.retry_after_ms.is_some())))))
            || !session.agent_role.is_main()
            || session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.policy_revision != input.policy_revision
            || session.main_stopped
            || session.input_revision != 1
            || session.subagent_wait.is_some()
            || session.terminal_error.as_ref().is_some_and(|error| {
                error.kind != desk_agent_protocol::AgentErrorKind::ModelUnavailable
                    || !error.retryable
                    || (!session.subagent_result_only
                        && session.ready_subagent_notification.is_none())
            })
            || session.terminal_permission_request_id.is_some()
            || !session.unclosed_tool_call_ids().is_empty()
            || !session.execution_state.states().is_empty()
            || session
                .permission_requests
                .iter()
                .any(|request| !request.state.is_terminal())
            || session.current_request_id.as_deref() != Some(input.run_id)
            || session.current_turn_id.as_deref() != Some(format!("{}-turn", input.run_id).as_str())
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let answer_reference = work
            .result_ref
            .as_deref()
            .and_then(|value| value.strip_prefix("answer-children:"));
        let (group, source) = if let Some(answer_id) = answer_reference {
            if !session.conversation.iter().any(|message| {
                message.message_id == answer_id
                    && message.role == desk_diagnose_core::chat::ChatRole::Assistant
                    && message.turn_id == session.current_turn_id
                    && message.tool_calls.is_empty()
            }) {
                return Err(ScheduleStoreError::Conflict);
            }
            let source_row = crate::entity::agent_delegation_group::Entity::find()
                .filter(
                    crate::entity::agent_delegation_group::Column::GroupId.eq(session
                        .delegation_group_id
                        .as_deref()
                        .ok_or(ScheduleStoreError::Conflict)?),
                )
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            let source = crate::agent_subagent_store::decode_creation(&source_row)?;
            let frozen = source
                .scheduled_source
                .as_ref()
                .ok_or(ScheduleStoreError::Invalid)?;
            Self::lock_delegation_source_authority(txn, input.owner, &session.device_id, frozen)
                .await?;
            crate::agent_subagent_store::prepare_notification_on(txn, &mut session).await?;
            row = agent_session::Entity::find_by_id(row.id)
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
            if row.version != session.version {
                return Err(ScheduleStoreError::Conflict);
            }
            crate::agent_subagent_store::ready_notification_source_on(txn, &session)
                .await
                .map_err(|_| ScheduleStoreError::Conflict)?
                .ok_or(ScheduleStoreError::Conflict)?
        } else {
            let (wait, group, source) =
                crate::agent_subagent_store::ready_wait_source_on(txn, &session)
                    .await
                    .map_err(|_| ScheduleStoreError::Conflict)?
                    .ok_or(ScheduleStoreError::Conflict)?;
            if work.result_ref.as_deref() != Some(format!("children:{}", wait.wait_id).as_str()) {
                return Err(ScheduleStoreError::Conflict);
            }
            (group, source)
        };
        if !matches!(&group.source, desk_diagnose_core::subagent::DelegationSource::ScheduledOccurrence {
                schedule_id, occurrence_id,
            } if schedule_id == &work.schedule_id && occurrence_id == input.run_id)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let frozen = source
            .scheduled_source
            .as_ref()
            .ok_or(ScheduleStoreError::Invalid)?;
        let authority =
            Self::lock_delegation_source_authority(txn, input.owner, &session.device_id, frozen)
                .await?;
        // Task -> session lock order agrees with cancellation and native dispatch.
        let locked = agent_session::Entity::find_by_id(row.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if locked != row {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(txn).await?;
        let until = now
            .checked_add(i64::from(input.lease_seconds) * 1_000)
            .ok_or(ScheduleStoreError::Invalid)?
            .min(authority.deadline_ms())
            .min(group.limits.deadline_ms);
        if now < authority.verified_at() || until <= now {
            return Err(ScheduleStoreError::Conflict);
        }
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        let deadline =
            chrono::DateTime::from_timestamp_millis(until).ok_or(ScheduleStoreError::Invalid)?;
        let epoch = work
            .lease_epoch
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        session.version = row
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        session.lease_token = session
            .lease_token
            .checked_add(1)
            .filter(|token| *token <= i64::MAX as u64)
            .ok_or(ScheduleStoreError::Invalid)?;
        if answer_reference.is_some() {
            session.subagent_result_only = true;
        }
        session.turn_state = TurnState::Running;
        session.terminal_error = None;
        session.scope_snapshot =
            desk_diagnose_core::session::narrow_scope(&session.turn_start_scope, &input.scope);
        session.updated_at = timestamp.to_rfc3339();
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("running".into()),
                lease_owner: Set(Some(input.node_id.into())),
                lease_epoch: Set(epoch),
                lease_deadline: Set(Some(until)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
            .filter(run::Column::Status.eq("awaiting_children"))
            .filter(run::Column::LeaseOwner.is_null())
            .filter(run::Column::CancelRequestedAt.is_null())
            .filter(run::Column::FailureAccounted.eq(false))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(session
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                version: Set(session.version),
                lease_token: Set(session.lease_token as i64),
                lease_deadline: Set(Some(deadline)),
                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .filter(agent_session::Column::LeaseDeadline.is_null())
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        crate::agent_subagent_store::mark_notification_attempt_on(txn, &session).await?;
        let after = Self::lock_run_authority(
            txn,
            input.owner,
            &session.device_id,
            input.run_id,
            input.node_id,
            epoch,
        )
        .await?;
        if after.provenance() != &frozen.provenance {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::TransactionTrait;

    pub(super) async fn claim_on(
        db: &sea_orm::DatabaseConnection,
        parent: &PersistedAgentSession,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        let txn = crate::db::begin_write(db, agent_session::Entity).await?;
        let result = ScheduleStore::claim_fresh_children_on(
            &txn,
            FreshTaskClaim {
                owner: 1,
                run_id: &parent.conversation_id,
                node_id: "children-node",
                lease_seconds: 90,
                policy_revision: parent.policy_revision,
                scope: parent.scope_snapshot.clone(),
            },
        )
        .await?;
        txn.commit().await?;
        Ok(result)
    }

    #[tokio::test]
    async fn dependency_claim_waits_for_real_completion_then_renews_both_holders_once() {
        let (db, schedule, store, parent, task_id) =
            crate::agent_subagent_store::scheduled_test_wait_fixture().await;
        let before = run::Entity::find()
            .filter(run::Column::RunId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(claim_on(&db, &parent).await.is_err());
        crate::agent_subagent_store::complete_scheduled_test_child(&db, &task_id).await;
        let ready = store
            .resolve_parent_wait(&parent.conversation_id, &parent.actor_id, &parent.device_id)
            .await
            .unwrap();
        assert!(matches!(
            ready,
            crate::agent_subagent_store::ParentWaitResolution::Ready(_)
        ));
        let after = claim_on(&db, &parent).await.unwrap();
        assert_eq!(after.lease_token, parent.lease_token + 1);
        assert_eq!(after.input_revision, parent.input_revision);
        assert_eq!(after.delegation_group_id, parent.delegation_group_id);
        assert_eq!(after.trigger_origin, TriggerOrigin::ScheduledTask);
        assert!(after.ready_subagent_wait.is_some());
        assert!(claim_on(&db, &after).await.is_err());
        let current = run::Entity::find_by_id(before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.lease_epoch, before.lease_epoch + 1);
        assert_eq!(current.started_at, before.started_at);
        assert_eq!(current.attempt, 1);
        assert_eq!(current.status, "running");
        let txn = db.begin().await.unwrap();
        let authority = ScheduleStore::lock_run_authority(
            &txn,
            1,
            &after.device_id,
            &after.conversation_id,
            "children-node",
            current.lease_epoch,
        )
        .await
        .unwrap();
        assert!(
            current.lease_deadline.unwrap()
                <= authority
                    .delegation_source()
                    .unwrap()
                    .deadline_ms()
                    .unwrap()
        );
        txn.rollback().await.unwrap();
        assert_eq!(
            schedule
                .read(1, &current.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(current.run_id.as_str())
        );
    }

    #[tokio::test]
    async fn resolved_result_never_reclaims_a_cancelled_source_or_stopped_parent() {
        for stopped in [false, true] {
            let (db, _, store, parent, task_id) =
                crate::agent_subagent_store::scheduled_test_wait_fixture().await;
            crate::agent_subagent_store::complete_scheduled_test_child(&db, &task_id).await;
            store
                .resolve_parent_wait(&parent.conversation_id, &parent.actor_id, &parent.device_id)
                .await
                .unwrap();
            if stopped {
                let row = agent_session::Entity::find()
                    .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                let mut current = PersistedAgentSession::decode_json(&row.state_json).unwrap();
                current.main_stopped = true;
                current.version += 1;
                agent_session::Entity::update_many()
                    .set(agent_session::ActiveModel {
                        state_json: Set(current.encode_json_for_storage().unwrap()),
                        version: Set(current.version),
                        ..Default::default()
                    })
                    .filter(agent_session::Column::Id.eq(row.id))
                    .exec(&db)
                    .await
                    .unwrap();
            } else {
                run::Entity::update_many()
                    .set(run::ActiveModel {
                        cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                        ..Default::default()
                    })
                    .filter(run::Column::RunId.eq(&parent.conversation_id))
                    .exec(&db)
                    .await
                    .unwrap();
            }
            assert!(claim_on(&db, &parent).await.is_err());
            let current = run::Entity::find()
                .filter(run::Column::RunId.eq(&parent.conversation_id))
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(current.status, "awaiting_children");
            assert!(current.lease_owner.is_none());
        }
    }
}

#[cfg(test)]
mod notification_tests;
