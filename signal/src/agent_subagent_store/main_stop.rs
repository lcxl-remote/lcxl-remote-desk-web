//! Atomic root stop covers spawn races while retaining independent child sources.
use super::*;
use crate::entity::agent_delegation_reservation as receipt_row;
use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantStopControl, AiAssistantStopResult, SubAgentStopChoice,
};
use sea_orm::{ActiveModelTrait, DatabaseTransaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainStopOutcome {
    pub result: AiAssistantStopResult,
    pub cancel_request_ids: Vec<String>,
}

impl SubAgentStore {
    pub async fn stop_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        control: &AiAssistantStopControl,
    ) -> Result<MainStopOutcome, DbErr> {
        validate(control)?;
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;

        let parent = parent_on(&txn, root, actor, device).await?;
        let key = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(root, actor, device, "main_stop", &control.client_request_id))
                    .map_err(|_| invalid())?
            )
        );
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(control).map_err(|_| invalid())?)
        );
        if let Some(receipt) = receipt_row::Entity::find()
            .filter(receipt_row::Column::LogicalKeySha256.eq(&key))
            .one(&txn)
            .await?
        {
            if receipt.operation_kind != "main_stop"
                || receipt.root_conversation_id != root
                || receipt.arguments_sha256 != digest
                || receipt.state != "stop_committed"
            {
                return Err(invalid());
            }
            let outcome = serde_json::from_str(&receipt.reservation_json).map_err(|_| invalid())?;
            txn.commit().await?;
            return Ok(outcome);
        }
        if parent.input_revision != control.expected_input_revision
            || parent.control_revision != control.expected_control_revision
        {
            return Err(invalid());
        }
        let children = run_row::Entity::find()
            .filter(run_row::Column::RootConversationId.eq(root))
            .filter(run_row::Column::ActorId.eq(actor))
            .filter(run_row::Column::DeviceId.eq(device))
            .filter(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
            .order_by_asc(run_row::Column::ChildConversationId)
            .limit(desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY as u64 + 1)
            .all(&txn)
            .await?;
        if children.len() > desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY
            || (!children.is_empty() && control.subagent_choice.is_none())
        {
            return Err(invalid());
        }

        let mut group_ids: std::collections::BTreeSet<String> = children
            .iter()
            .map(|child| child.group_id.clone())
            .collect();
        group_ids.extend(parent.delegation_group_id.iter().cloned());
        let now = chrono::Utc::now();
        let mut cancel_request_ids: Vec<String> =
            parent.current_request_id.iter().cloned().collect();
        let mut stopped_subagents = Vec::new();
        for group_id in group_ids {
            let row = group_row::Entity::find()
                .filter(group_row::Column::GroupId.eq(&group_id))
                .filter(group_row::Column::RootConversationId.eq(root))
                .filter(group_row::Column::ActorId.eq(actor))
                .filter(group_row::Column::DeviceId.eq(device))
                .one(&txn)
                .await?
                .ok_or_else(invalid)?;
            let mut group = decode_group(&row)?;
            group.stop_parent().map_err(|_| invalid())?;
            replace_group_on(&txn, &row, &group, now.timestamp_millis()).await?;
            if control.subagent_choice == Some(SubAgentStopChoice::IncludeSubAgents) {
                for child_row in children.iter().filter(|child| child.group_id == group_id) {
                    let mut run = decode_run(child_row)?;
                    let row = session_row::Entity::find()
                        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
                        .filter(session_row::Column::ActorId.eq(actor))
                        .filter(session_row::Column::DeviceId.eq(device))
                        .one(&txn)
                        .await?
                        .ok_or_else(invalid)?;
                    let child = PersistedAgentSession::decode_json(&row.state_json)
                        .map_err(|_| invalid())?;
                    run.validate_session(&child).map_err(|_| invalid())?;
                    super::native_cancel::cancel_native_actions_on(
                        &txn,
                        &child,
                        &key,
                        now.timestamp_millis(),
                    )
                    .await?;
                    cancel_request_ids.extend(child.current_request_id.iter().cloned());
                    run.request_cancel(run.fence(), &now.to_rfc3339())
                        .map_err(|_| invalid())?;
                    run.settle_cancel(&now.to_rfc3339())
                        .map_err(|_| invalid())?;
                    replace_run_on(&txn, child_row, &run, now.timestamp_millis()).await?;
                    synchronize_control_on(&txn, &run, now.timestamp_millis()).await?;
                    append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
                    stopped_subagents.push(run.summary());
                }
            }
        }
        super::native_cancel::cancel_native_actions_on(&txn, &parent, &key, now.timestamp_millis())
            .await?;
        stop_main_goals_on(&txn, &parent, now.timestamp_millis()).await?;
        let mut stopped = parent.clone();
        desk_diagnose_core::subagent::control::stop_main_session(&mut stopped, &now.to_rfc3339())
            .map_err(|_| invalid())?;
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(root))
            .filter(session_row::Column::ActorId.eq(actor))
            .filter(session_row::Column::DeviceId.eq(device))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        if row.version != parent.version
            || row.lease_token != i64::try_from(parent.lease_token).map_err(|_| invalid())?
        {
            return Err(invalid());
        }
        stopped.version = parent.version.checked_add(1).ok_or_else(invalid)?;
        let changed = session_row::Entity::update_many()
            .set(session_row::ActiveModel {
                state_json: Set(stopped.encode_json_for_storage().map_err(|_| invalid())?),
                version: Set(stopped.version),
                lease_token: Set(i64::try_from(stopped.lease_token).map_err(|_| invalid())?),
                lease_deadline: Set(None),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(session_row::Column::Id.eq(row.id))
            .filter(session_row::Column::Version.eq(row.version))
            .filter(session_row::Column::LeaseToken.eq(row.lease_token))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        cancel_request_ids.sort();
        cancel_request_ids.dedup();
        let outcome = MainStopOutcome {
            result: AiAssistantStopResult {
                input_revision: stopped.input_revision,
                control_revision: stopped.control_revision,
                stopped_subagents,
            },
            cancel_request_ids,
        };
        receipt_row::ActiveModel {
            reservation_id: Set(format!("main-stop-{key}")),
            logical_key_sha256: Set(key),
            root_conversation_id: Set(root.into()),
            group_id: Set(parent
                .delegation_group_id
                .clone()
                .unwrap_or_else(|| root.into())),
            conversation_id: Set(root.into()),
            task_id: Set(None),
            operation_kind: Set("main_stop".into()),
            arguments_sha256: Set(digest),
            source_epoch: Set(0),
            input_revision: Set(i64::try_from(parent.input_revision).map_err(|_| invalid())?),
            control_revision: Set(i64::try_from(parent.control_revision).map_err(|_| invalid())?),
            reservation_json: Set(serde_json::to_string(&outcome).map_err(|_| invalid())?),
            actual_json: Set(None),
            state: Set("stop_committed".into()),
            version: Set(1),
            created_at: Set(now.timestamp_millis()),
            settled_at: Set(Some(now.timestamp_millis())),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        txn.commit().await?;
        Ok(outcome)
    }
}

fn validate(control: &AiAssistantStopControl) -> Result<(), DbErr> {
    if !desk_diagnose_core::subagent::valid_id(&control.client_request_id)
        || control.expected_input_revision == 0
        || control.expected_control_revision == 0
        || control.expected_input_revision > i64::MAX as u64
        || control.expected_control_revision > i64::MAX as u64
    {
        return Err(invalid());
    }
    Ok(())
}

/// The owner goal control already holds root/child/source locks. Resuming main
/// planning does not reset the group's finite limits or terminal children.
pub(crate) async fn resume_goal_parent_on(
    txn: &DatabaseTransaction,
    parent: &mut PersistedAgentSession,
    goal_id: &str,
    now_ms: i64,
) -> Result<(), DbErr> {
    if !parent.main_stopped {
        return Ok(());
    }
    parent.control_revision = parent
        .control_revision
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or_else(invalid)?;
    let rows = group_row::Entity::find()
        .filter(group_row::Column::RootConversationId.eq(&parent.conversation_id))
        .filter(group_row::Column::ActorId.eq(&parent.actor_id))
        .filter(group_row::Column::DeviceId.eq(&parent.device_id))
        .filter(group_row::Column::SourceGoalId.eq(goal_id))
        .order_by_asc(group_row::Column::Id)
        .all(txn)
        .await?;
    for row in rows {
        let mut group = decode_group(&row)?;
        if group.parent_input_revision != parent.input_revision {
            continue;
        }
        group
            .resume_parent(parent.input_revision, parent.control_revision)
            .map_err(|_| invalid())?;
        replace_group_on(txn, &row, &group, now_ms).await?;
    }
    parent.main_stopped = false;
    Ok(())
}

async fn stop_main_goals_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    now_ms: i64,
) -> Result<(), DbErr> {
    let rows = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(&parent.conversation_id))
        .filter(goal_row::Column::ActorId.eq(&parent.actor_id))
        .filter(goal_row::Column::DeviceId.eq(&parent.device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .order_by_asc(goal_row::Column::Id)
        .all(txn)
        .await?;
    for row in rows {
        let mut goal = crate::agent_goal_store::decode(&row)?;
        let previous = (goal.state_version, goal.lease_epoch);
        goal.stop_main_planning(u64::try_from(now_ms).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
        if !crate::agent_goal_store::replace_on(txn, &goal, previous.0, previous.1, None, None)
            .await?
        {
            return Err(invalid());
        }
    }
    Ok(())
}
