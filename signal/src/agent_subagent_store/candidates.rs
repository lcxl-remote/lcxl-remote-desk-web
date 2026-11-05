//! Bounded scan positions are hints; execution always takes a fresh fenced claim.
use super::*;
use desk_diagnose_core::subagent::{creation::TaskCreationEnvelope, runtime::RuntimeTurn};
use sea_orm::{ExprTrait, QueryTrait};

pub(super) use desk_assistant_data_store::worker_request::is_worker_head_on;

type RootWaitPage = (Vec<group_row::Model>, Option<i64>);

impl SubAgentStore {
    pub async fn queued_task_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run_row::Model>, DbErr> {
        if !(1..=32).contains(&limit) {
            return Err(invalid());
        }
        let compatible = compatible_sessions();
        run_row::Entity::find()
            .filter(run_row::Column::RootConversationId.in_subquery(compatible.clone()))
            .filter(run_row::Column::ChildConversationId.in_subquery(compatible))
            .filter(run_row::Column::Id.gt(after_id))
            .filter(run_row::Column::State.is_in(["queued", "waiting_resource"]))
            .filter(run_row::Column::NextAttemptAtMs.lte(chrono::Utc::now().timestamp_millis()))
            .order_by_asc(run_row::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
    }

    /// Read-only scheduling hints. Claims still recheck the current locked session.
    pub fn root_wait_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> futures_util::future::BoxFuture<'_, Result<RootWaitPage, DbErr>> {
        desk_diagnose_core::future::send_phase(|| async move {
            if !(1..=32).contains(&limit) {
                return Err(invalid());
            }
            let compatible = compatible_sessions();
            let children = sea_orm::sea_query::Query::select()
                .expr(sea_orm::sea_query::Expr::val(1))
                .from(run_row::Entity)
                .and_where(
                    sea_orm::sea_query::Expr::col((run_row::Entity, run_row::Column::GroupId))
                        .equals((group_row::Entity, group_row::Column::GroupId)),
                )
                .to_owned();
            let rows = group_row::Entity::find()
                .filter(group_row::Column::RootConversationId.in_subquery(compatible))
                .filter(sea_orm::sea_query::Expr::exists(children))
                .filter(group_row::Column::Id.gt(after_id))
                .filter(group_row::Column::ParentActive.eq(true))
                .filter(group_row::Column::SourceAdmission.eq("open"))
                .order_by_asc(group_row::Column::Id)
                .limit(limit)
                .all(&self.db)
                .await?;
            // Cursor progress comes from the raw page, including skipped records.
            let next = (rows.len() == limit as usize).then(|| rows.last().unwrap().id);
            if rows.is_empty() {
                return Ok((Vec::new(), next));
            }
            let parents = session_row::Entity::find()
                .filter(
                    session_row::Column::ConversationId
                        .is_in(rows.iter().map(|row| row.root_conversation_id.as_str())),
                )
                .filter(session_row::Column::ConversationSchemaVersion.eq(i32::from(
                    desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                )))
                .all(&self.db)
                .await?;
            let wake_groups = desk_assistant_data_store::worker_report::pending_wake_groups_on(
                &self.db,
                rows.iter().map(|row| row.group_id.clone()),
                chrono::Utc::now().timestamp_millis(),
            )
            .await?;
            let mut seen = std::collections::BTreeSet::new();
            let mut candidates = Vec::new();
            for row in rows {
                let subject = (
                    row.root_conversation_id.clone(),
                    row.actor_id.clone(),
                    row.device_id.clone(),
                );
                if !seen.insert(subject) {
                    continue;
                }
                let Some(parent) = parents.iter().find(|parent| {
                    parent.conversation_id == row.root_conversation_id
                        && parent.actor_id == row.actor_id
                        && parent.device_id == row.device_id
                }) else {
                    continue;
                };
                let Ok(session) = PersistedAgentSession::decode_json(&parent.state_json) else {
                    continue;
                };
                if session.surface == AgentSessionSurface::AiAssistant
                    && session.agent_role.is_main()
                    && session.check_subject(&row.actor_id, &row.device_id).is_ok()
                    && !session.turn_state.is_active()
                    && (session.subagent_wait.is_some()
                        || session.has_ready_subagent_completion()
                        || (session.agent_role.workbench_request_id().is_some()
                            && session.delegation_group_id.as_deref()
                                == Some(row.group_id.as_str())
                            && wake_groups.contains(&row.group_id)))
                {
                    candidates.push(row);
                }
            }
            Ok((candidates, next))
        })
    }

    pub async fn child_runtime_candidate(
        &self,
        task_id: &str,
    ) -> Result<Option<RuntimeTurn>, DbErr> {
        let selected = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await?
            .ok_or_else(|| invalid())?;
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        parent_on(
            &txn,
            &selected.root_conversation_id,
            &selected.actor_id,
            &selected.device_id,
        )
        .await?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::Id.eq(selected.id))
            .one(&txn)
            .await?
            .ok_or_else(|| invalid())?;
        let run = decode_run(&row)?;
        if run.state != SubAgentState::Queued
            || run.source_paused
            || !is_worker_head_on(&txn, &row).await?
        {
            return Ok(None);
        }
        let group_row = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
            .filter(group_row::Column::RootConversationId.eq(&run.binding.root_conversation_id))
            .filter(group_row::Column::ActorId.eq(&run.actor_id))
            .filter(group_row::Column::DeviceId.eq(&run.device_id))
            .one(&txn)
            .await?
            .ok_or_else(|| invalid())?;
        let group = decode_group(&group_row)?;
        if group.source_admission != SourceAdmission::Open
            || group.source_epoch != run.binding.source_epoch
        {
            return Ok(None);
        }
        let creation: TaskCreationEnvelope =
            serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
        if creation.source != decode_creation(&group_row)? {
            return Err(invalid());
        }
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
            .filter(session_row::Column::ActorId.eq(&run.actor_id))
            .filter(session_row::Column::DeviceId.eq(&run.device_id))
            .one(&txn)
            .await?
            .ok_or_else(|| invalid())?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.version != row.version || session.turn_state.is_active() {
            return Ok(None);
        }
        match crate::issue_connections::check_work_on(
            &txn,
            &session,
            false,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        {
            Ok(()) => {}
            Err(desk_assistant_data_store::AssistantStoreError::Inaccessible) => return Ok(None),
            Err(_) => return Err(invalid()),
        }
        let candidate = RuntimeTurn::Child {
            session,
            run,
            creation,
        };
        candidate.validate().map_err(|_| invalid())?;
        txn.commit().await?;
        Ok(Some(candidate))
    }

    pub async fn parent_runtime_candidate(
        &self,
        root: &str,
        actor: &str,
        device: &str,
    ) -> Result<Option<RuntimeTurn>, DbErr> {
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        let mut session = parent_on(&txn, root, actor, device).await?;
        if session.turn_state.is_active()
            || session.turn_state == desk_diagnose_core::session::TurnState::Cancelled
        {
            return Ok(None);
        }
        if session.trigger_origin == desk_diagnose_core::session::TriggerOrigin::ScheduledTask {
            return Ok(None);
        }
        if session.main_stopped
            || session.subagent_wait.is_some()
            || session.terminal_permission_request_id.is_some()
        {
            return Ok(None);
        }
        match crate::issue_connections::check_work_on(
            &txn,
            &session,
            false,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        {
            Ok(()) => {}
            Err(desk_assistant_data_store::AssistantStoreError::Inaccessible) => return Ok(None),
            Err(_) => return Err(invalid()),
        }
        let source = if session.ready_subagent_wait.is_some() {
            super::parent_wait::ready_parent_source_on(&txn, &session).await?
        } else if session.agent_role.workbench_request_id().is_some() {
            super::notification::prepare_workbench_wake_on(&txn, &mut session).await?
        } else {
            None
        };
        let Some((group, source)) = source else {
            txn.commit().await?;
            return Ok(None);
        };
        if group.source.request_input_revision().is_none() {
            return Ok(None);
        }
        let candidate = RuntimeTurn::ParentCompletion {
            session: *session,
            source,
        };
        candidate.validate().map_err(|_| invalid())?;
        txn.commit().await?;
        Ok(Some(candidate))
    }
}

fn compatible_sessions() -> sea_orm::sea_query::SelectStatement {
    let compatible = session_row::Entity::find()
        .select_only()
        .column(session_row::Column::ConversationId)
        .filter(session_row::Column::ConversationSchemaVersion.eq(i32::from(
            desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
        )));
    compatible.into_query()
}
