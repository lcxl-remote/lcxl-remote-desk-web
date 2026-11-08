//! Atomic root stop covers spawn races while retaining independent child sources.
use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_delegation_reservation as receipt_row;
use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantStopControl, AiAssistantStopResult, SubAgentStopChoice,
};
use sea_orm::{ActiveModelTrait, ExprTrait, QuerySelect, sea_query::Expr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use desk_assistant_data_store::main_request::{self, is_workbench_root};
use desk_diagnose_core::model_observability::{PermissionOutcome, permission::PendingEnds};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainStopOutcome {
    pub result: AiAssistantStopResult,
    pub cancel_request_ids: Vec<String>,
}

impl SubAgentStore {
    pub async fn resume_main_queue_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        expected_control_revision: u64,
    ) -> Result<u64, DbErr> {
        use crate::entity::agent_permission_resume as resume;
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(root))
            .filter(session_row::Column::ActorId.eq(actor))
            .filter(session_row::Column::DeviceId.eq(device))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut parent =
            PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|_| invalid())?;
        if actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
            || parent.conversation_id != root
            || parent.actor_id != actor
            || parent.device_id != device
            || parent.version != row.version
            || i64::try_from(parent.lease_token).ok() != Some(row.lease_token)
            || (device.is_empty()
                && !parent.has_admitted_personal_input()
                && !is_workbench_root(&parent))
        {
            return Err(invalid());
        }
        let now = chrono::Utc::now();
        if desk_assistant_data_store::main_request::resume_queue_control(
            &mut parent,
            expected_control_revision,
            &now.to_rfc3339(),
        )
        .map_err(|_| invalid())?
        {
            let resumes = resume::Entity::find()
                .filter(resume::Column::RunId.eq(root))
                .filter(resume::Column::ActorId.eq(actor))
                .filter(resume::Column::DeviceId.eq(device))
                .filter(resume::Column::InputRevision.eq(parent.input_revision as i64))
                .filter(resume::Column::State.eq("pending"));
            if resumes
                .clone()
                .filter(resume::Column::Version.eq(i64::MAX))
                .one(&txn)
                .await?
                .is_some()
            {
                return Err(invalid());
            }
            use sea_orm::QueryTrait;
            resume::Entity::update_many()
                .col_expr(resume::Column::State, Expr::value("superseded"))
                .col_expr(resume::Column::UpdatedAt, Expr::value(now))
                .col_expr(
                    resume::Column::Version,
                    Expr::col(resume::Column::Version).add(1_i64),
                )
                .filter(
                    resume::Column::Id.in_subquery(
                        resumes
                            .select_only()
                            .column(resume::Column::Id)
                            .into_query(),
                    ),
                )
                .exec(&txn)
                .await?;
            parent.version = row.version.checked_add(1).ok_or_else(invalid)?;
            let updated = session_row::Entity::update_many()
                .set(session_row::ActiveModel {
                    state_json: Set(parent.encode_json_for_storage().map_err(|_| invalid())?),
                    version: Set(parent.version),
                    updated_at: Set(now),
                    ..Default::default()
                })
                .filter(session_row::Column::Id.eq(row.id))
                .filter(session_row::Column::Version.eq(row.version))
                .filter(session_row::Column::LeaseToken.eq(row.lease_token))
                .exec(&txn)
                .await?;
            if updated.rows_affected != 1 {
                return Err(invalid());
            }
        }
        txn.commit().await?;
        Ok(parent.control_revision)
    }

    pub async fn stop_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        control: &AiAssistantStopControl,
    ) -> Result<MainStopOutcome, DbErr> {
        validate(control)?;
        let txn = desk_diagnose_core::future::boxed(|| async {
            crate::db::begin_write(&self.db, session_row::Entity).await
        })
        .await?;

        let parent = desk_diagnose_core::future::boxed(|| async {
            parent_on(&txn, root, actor, device).await
        })
        .await?;
        if device.is_empty()
            && ((!parent.has_admitted_personal_input() && !is_workbench_root(&parent))
                || actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string())
        {
            return Err(invalid());
        }
        let reservation_id = desk_diagnose_core::native_ids::stable(
            desk_diagnose_core::native_ids::Kind::DelegationReservation,
            &(root, actor, device, "main_stop", &control.client_request_id),
        );
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(control).map_err(|_| invalid())?)
        );
        if let Some(receipt) = desk_diagnose_core::future::boxed(|| async {
            receipt_row::Entity::find()
                .filter(receipt_row::Column::ReservationId.eq(&reservation_id))
                .one(&txn)
                .await
        })
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
            desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
            return Ok(outcome);
        }
        let workbench = is_workbench_root(&parent);
        if (parent.input_revision == 0 && !workbench)
            || parent.input_revision != control.expected_input_revision
            || parent.control_revision != control.expected_control_revision
        {
            return Err(invalid());
        }
        let children = desk_diagnose_core::future::boxed(|| async {
            let query = if workbench {
                desk_assistant_data_store::worker_request::unfinished_in_scope(
                    actor,
                    device,
                    root,
                    Some(root),
                )
            } else {
                run_row::Entity::find()
                    .filter(run_row::Column::RootConversationId.eq(root))
                    .filter(run_row::Column::ActorId.eq(actor))
                    .filter(run_row::Column::DeviceId.eq(device))
                    .filter(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
            };
            query
                .order_by_asc(run_row::Column::ChildConversationId)
                .limit(desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY as u64 + 1)
                .all(&txn)
                .await
        })
        .await?;
        if children.len() > desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY
            || (!children.is_empty() && control.subagent_choice.is_none())
        {
            return Err(invalid());
        }

        let parents = if workbench {
            workbench_parents_on(&txn, &parent, &children).await?
        } else {
            BTreeSet::new()
        };
        let now = chrono::Utc::now();
        if control.subagent_choice == Some(SubAgentStopChoice::IncludeSubAgents) {
            let now = now.timestamp_millis();
            if let (Some(workbench), Some(request)) = (
                parent.agent_role.workbench_id(),
                parent.agent_role.workbench_request_id(),
            ) {
                desk_assistant_data_store::workbench::tasks::stop_issue_round_on(
                    &txn, actor, workbench, request, now,
                )
                .await
                .map_err(|_| invalid())?;
            } else {
                desk_assistant_data_store::issue_authority::stop_conversation(
                    &txn, actor, root, now,
                )
                .await
                .map_err(|_| invalid())?;
                desk_assistant_data_store::issue_reply::lifecycle::cancel_conversation_on(
                    &txn, actor, root, now,
                )
                .await
                .map_err(|_| invalid())?;
            }
        }
        let scope = StopScope {
            children: &children,
            key: &reservation_id,
            choice: control.subagent_choice,
            now,
        };
        let mut effects = StopEffects::default();
        for work in parents {
            desk_diagnose_core::future::boxed(|| {
                stop_workbench_work_on(&txn, &parent, &work, &scope, &mut effects)
            })
            .await?;
        }
        let revision = desk_diagnose_core::future::boxed(|| {
            stop_native_parent_on(&txn, &parent, &scope, true, &mut effects)
        })
        .await?;
        effects.cancel_request_ids.sort();
        effects.cancel_request_ids.dedup();
        let outcome = MainStopOutcome {
            result: AiAssistantStopResult {
                input_revision: parent.input_revision,
                control_revision: revision,
                stopped_subagents: effects.stopped_subagents,
            },
            cancel_request_ids: effects.cancel_request_ids,
        };
        receipt_row::ActiveModel {
            reservation_id: Set(reservation_id.clone()),
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
        desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
        effects.permission_ends.submit(
            now.timestamp_millis(),
            crate::model_metrics::runtime::submit,
        );
        Ok(outcome)
    }
}

struct StopScope<'a> {
    children: &'a [run_row::Model],
    key: &'a str,
    choice: Option<SubAgentStopChoice>,
    now: chrono::DateTime<chrono::Utc>,
}

#[derive(Default)]
struct StopEffects {
    cancel_request_ids: Vec<String>,
    stopped_subagents: Vec<AiAssistantSubAgentSummary>,
    permission_ends: PendingEnds,
}

async fn workbench_parents_on(
    txn: &DatabaseTransaction,
    root: &PersistedAgentSession,
    children: &[run_row::Model],
) -> Result<BTreeSet<String>, DbErr> {
    let unclaimed = main_request::unclaimed_workbench_conversations(txn, root)
        .await
        .map_err(|_| invalid())?;
    let parents = group_row::Entity::find()
        .filter(group_row::Column::SourceWorkbenchId.eq(&root.conversation_id))
        .filter(group_row::Column::ActorId.eq(&root.actor_id))
        .filter(group_row::Column::DeviceId.eq(""))
        .filter(group_row::Column::ParentActive.eq(true))
        .select_only()
        .column(group_row::Column::RootConversationId)
        .into_tuple::<String>()
        .all(txn)
        .await?;
    Ok(parents
        .into_iter()
        .chain(main_request::coordinating_work_conversation(root))
        .chain(
            children
                .iter()
                .map(|child| child.root_conversation_id.clone()),
        )
        .filter(|conversation| !unclaimed.contains(conversation))
        .collect())
}

async fn stop_workbench_work_on(
    txn: &DatabaseTransaction,
    root: &PersistedAgentSession,
    conversation: &str,
    scope: &StopScope<'_>,
    effects: &mut StopEffects,
) -> Result<(), DbErr> {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(conversation))
        .filter(session_row::Column::ActorId.eq(&root.actor_id))
        .filter(session_row::Column::DeviceId.eq(""))
        .one(txn)
        .await?;
    let has_child = scope
        .children
        .iter()
        .any(|child| child.root_conversation_id == conversation);
    let Some(row) = row else {
        return if has_child { Err(invalid()) } else { Ok(()) };
    };
    let work = PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|_| invalid())?;
    if work.actor_id != root.actor_id
        || work.conversation_id != conversation
        || !work.has_admitted_personal_input()
        || work.agent_role.workbench_id() != Some(root.conversation_id.as_str())
        || work.agent_role.workbench_request_id().is_none()
        || work.version != row.version
        || i64::try_from(work.lease_token).ok() != Some(row.lease_token)
    {
        return Err(invalid());
    }
    let stop_planning = !work.main_stopped
        && (has_child
            || crate::agent_run_event_store::main_requests::slot_held(txn, &work)
                .await
                .map_err(|_| invalid())?);
    stop_native_parent_on(txn, &work, scope, stop_planning, effects).await?;
    Ok(())
}

async fn stop_native_parent_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    scope: &StopScope<'_>,
    stop_planning: bool,
    effects: &mut StopEffects,
) -> Result<u64, DbErr> {
    let now = scope.now;
    let children = scope.children;
    let root = &parent.conversation_id;
    let actor = &parent.actor_id;
    let device = &parent.device_id;
    let mut group_ids: BTreeSet<String> = children
        .iter()
        .filter(|child| child.root_conversation_id == *root)
        .map(|child| child.group_id.clone())
        .collect();
    group_ids.extend(parent.delegation_group_id.iter().cloned());
    for group_id in group_ids {
        let row = desk_diagnose_core::future::boxed(|| async {
            group_row::Entity::find()
                .filter(group_row::Column::GroupId.eq(&group_id))
                .filter(group_row::Column::RootConversationId.eq(root))
                .filter(group_row::Column::ActorId.eq(actor))
                .filter(group_row::Column::DeviceId.eq(device))
                .one(txn)
                .await
        })
        .await?
        .ok_or_else(|| invalid())?;
        let mut group = decode_group(&row)?;
        group.stop_parent().map_err(|_| invalid())?;
        desk_diagnose_core::future::boxed(|| async {
            replace_group_on(txn, &row, &group, now.timestamp_millis()).await
        })
        .await?;
        if scope.choice == Some(SubAgentStopChoice::IncludeSubAgents) {
            for child_row in children
                .iter()
                .filter(|child| child.root_conversation_id == *root && child.group_id == group_id)
            {
                let mut run = decode_run(child_row)?;
                let row = desk_diagnose_core::future::boxed(|| async {
                    session_row::Entity::find()
                        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
                        .filter(session_row::Column::ActorId.eq(actor))
                        .filter(session_row::Column::DeviceId.eq(device))
                        .one(txn)
                        .await
                })
                .await?
                .ok_or_else(|| invalid())?;
                let child =
                    PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
                run.validate_session(&child).map_err(|_| invalid())?;
                desk_diagnose_core::future::boxed(|| async {
                    super::native_cancel::cancel_native_actions_on(
                        txn,
                        &child,
                        scope.key,
                        now.timestamp_millis(),
                    )
                    .await
                })
                .await?;
                effects
                    .cancel_request_ids
                    .extend(child.current_request_id.iter().cloned());
                run.request_cancel(run.fence(), &now.to_rfc3339())
                    .map_err(|_| invalid())?;
                run.settle_cancel(&now.to_rfc3339())
                    .map_err(|_| invalid())?;
                desk_diagnose_core::future::boxed(|| async {
                    replace_run_on(txn, child_row, &run, now.timestamp_millis()).await
                })
                .await?;
                effects.permission_ends.extend(
                    desk_diagnose_core::future::boxed(|| async {
                        synchronize_control_on(txn, &run, now.timestamp_millis(), false).await
                    })
                    .await?,
                );
                desk_diagnose_core::future::boxed(|| async {
                    append_state_event_on(txn, &group, &run, now.timestamp_millis()).await
                })
                .await?;
                effects.stopped_subagents.push(run.summary());
            }
        }
    }
    if !stop_planning {
        return Ok(parent.control_revision);
    }
    effects
        .cancel_request_ids
        .extend(parent.current_request_id.iter().cloned());
    effects.cancel_request_ids.extend(
        parent
            .main_coordination
            .iter()
            .map(|lease| lease.batch_id.clone()),
    );
    effects
        .permission_ends
        .extend(PendingEnds::waiting(parent, PermissionOutcome::Cancelled));
    desk_diagnose_core::future::boxed(|| async {
        super::native_cancel::cancel_native_actions_on(
            txn,
            parent,
            scope.key,
            now.timestamp_millis(),
        )
        .await
    })
    .await?;
    desk_diagnose_core::future::boxed(|| async {
        stop_main_goals_on(txn, parent, now.timestamp_millis()).await
    })
    .await?;
    let mut stopped = Box::new(parent.clone());
    desk_diagnose_core::subagent::control::stop_main_session(&mut stopped, &now.to_rfc3339())
        .map_err(|_| invalid())?;
    let row = desk_diagnose_core::future::boxed(|| async {
        session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(root))
            .filter(session_row::Column::ActorId.eq(actor))
            .filter(session_row::Column::DeviceId.eq(device))
            .one(txn)
            .await
    })
    .await?
    .ok_or_else(|| invalid())?;
    if row.version != parent.version
        || row.lease_token != i64::try_from(parent.lease_token).map_err(|_| invalid())?
    {
        return Err(invalid());
    }
    stopped.version = parent.version.checked_add(1).ok_or_else(|| invalid())?;
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
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(stopped.control_revision)
}

fn validate(control: &AiAssistantStopControl) -> Result<(), DbErr> {
    if !desk_diagnose_core::subagent::valid_id(&control.client_request_id)
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
        .ok_or_else(|| invalid())?;
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
