//! Bounded scan positions are hints; execution always takes a fresh fenced claim.
use super::*;
use desk_diagnose_core::subagent::{creation::TaskCreationEnvelope, runtime::RuntimeTurn};

impl SubAgentStore {
    pub async fn queued_task_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run_row::Model>, DbErr> {
        if !(1..=32).contains(&limit) {
            return Err(invalid());
        }
        run_row::Entity::find()
            .filter(run_row::Column::Id.gt(after_id))
            .filter(run_row::Column::State.is_in(["queued", "waiting_resource"]))
            .filter(run_row::Column::NextAttemptAtMs.lte(chrono::Utc::now().timestamp_millis()))
            .order_by_asc(run_row::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
    }

    pub async fn root_wait_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<group_row::Model>, DbErr> {
        if !(1..=32).contains(&limit) {
            return Err(invalid());
        }
        group_row::Entity::find()
            .filter(group_row::Column::Id.gt(after_id))
            .filter(group_row::Column::ParentActive.eq(true))
            .filter(group_row::Column::SourceAdmission.eq("open"))
            .order_by_asc(group_row::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
    }

    pub async fn child_runtime_candidate(
        &self,
        task_id: &str,
    ) -> Result<Option<RuntimeTurn>, DbErr> {
        let selected = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)?;
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
            .ok_or_else(invalid)?;
        let run = decode_run(&row)?;
        if run.state != SubAgentState::Queued || run.source_paused {
            return Ok(None);
        }
        let group_row = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
            .filter(group_row::Column::RootConversationId.eq(&run.binding.root_conversation_id))
            .filter(group_row::Column::ActorId.eq(&run.actor_id))
            .filter(group_row::Column::DeviceId.eq(&run.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
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
            .ok_or_else(invalid)?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.version != row.version || session.turn_state.is_active() {
            return Ok(None);
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
        super::notification::prepare_notification_on(&txn, &mut session).await?;
        let Some((group, source)) =
            super::parent_wait::ready_parent_source_on(&txn, &session).await?
        else {
            txn.commit().await?;
            return Ok(None);
        };
        if !matches!(
            group.source,
            desk_diagnose_core::subagent::DelegationSource::UserInput { .. }
        ) {
            return Ok(None);
        }
        let candidate = RuntimeTurn::ParentCompletion { session, source };
        candidate.validate().map_err(|_| invalid())?;
        txn.commit().await?;
        Ok(Some(candidate))
    }
}
