//! Coherent owner presentation; snapshot reads never invalidate execution CAS.

use super::*;
use crate::{
    agent_background_task_store,
    capability_grant_store::{SignalCapabilityGrantStore, computer_background},
    entity::agent_action_item,
};
use desk_agent_protocol::capability_grant::CapabilityGrant;
use desk_diagnose_core::dynamic_run::BackgroundTaskRecord;

pub(crate) struct AssistantSnapshot {
    pub session: SessionSnapshot,
    pub background_tasks: Vec<BackgroundTaskRecord>,
    pub capability_grants: Vec<CapabilityGrant>,
    pub goal: Option<desk_diagnose_core::goal::GoalRun>,
    pub pending_goal_open_request: Option<desk_diagnose_core::goal::GoalOpenRequest>,
}

impl SignalAgentSessionStore {
    /// Resolve an opaque recovery selector, not an execution authorization.
    /// The caller still reads/stops through the exact actor/device store path.
    pub(crate) async fn recovery_device(
        &self,
        run: &str,
        actor: &str,
    ) -> Result<Option<String>, AgentError> {
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(run))
            .filter(agent_session::Column::ActorId.eq(actor))
            .one(&self.db)
            .await
            .map_err(|_| internal("recovery lookup failed"))?;
        let Some(row) = row else { return Ok(None) };
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| internal("invalid recovery session"))?;
        if session.surface != AgentSessionSurface::AiAssistant
            || session.actor_id != actor
            || session.device_id != row.device_id
            || session.conversation_id != run
            || session.version != row.version
            || session.lease_token as i64 != row.lease_token
        {
            return Ok(None);
        }
        Ok(Some(session.device_id))
    }

    /// Clients poll this, so an unchanged conversation must not take the SQLite
    /// writer. One WAL read transaction already sees a single consistent snapshot;
    /// the writer is only needed to persist a changed sequence and fingerprint.
    pub(crate) async fn read_assistant_snapshot_for_subject(
        &self,
        run: &str,
        actor: &str,
        device: &str,
    ) -> Result<Option<AssistantSnapshot>, AgentError> {
        let txn = self.db.begin().await.map_err(|error| {
            log::warn!("[assistant-snapshot] storage failed: phase=begin_read, error={error}");
            internal("begin Assistant snapshot failed")
        })?;
        let loaded =
            desk_diagnose_core::future::boxed(|| load_snapshot_on(&txn, run, actor, device)).await;
        // Nothing was written, so ending the read transaction cannot lose data.
        txn.rollback().await.ok();
        let loaded = loaded.map_err(|error| {
            log::warn!("[assistant-snapshot] storage failed: phase=read, error={error}");
            internal("read Assistant snapshot failed")
        })?;
        match loaded {
            None => return Ok(None),
            Some(loaded) if loaded.is_current() => return Ok(Some(loaded.finish())),
            Some(_) => {}
        }

        let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity)
            .await
            .map_err(|error| {
                log::warn!("[assistant-snapshot] storage failed: phase=begin_write, error={error}");
                internal("begin Assistant snapshot failed")
            })?;
        let result = desk_diagnose_core::future::boxed(|| async {
            let Some(loaded) = load_snapshot_on(&txn, run, actor, device).await? else {
                return Ok(None);
            };
            if !loaded.is_current() {
                agent_session::Entity::update_many()
                    .filter(agent_session::Column::Id.eq(loaded.row.id))
                    .col_expr(agent_session::Column::SnapshotSeq, Expr::value(loaded.seq))
                    .col_expr(
                        agent_session::Column::SnapshotFingerprint,
                        Expr::value(loaded.fingerprint.clone()),
                    )
                    .exec(&txn)
                    .await?;
            }
            Ok::<_, sea_orm::DbErr>(Some(loaded.finish()))
        })
        .await;
        match result {
            Ok(snapshot) => {
                txn.commit()
                    .await
                    .map_err(|_| internal("commit Assistant snapshot failed"))?;
                Ok(snapshot)
            }
            Err(error) => {
                log::warn!("[assistant-snapshot] storage failed: phase=write, error={error}");
                txn.rollback()
                    .await
                    .map_err(|_| internal("rollback Assistant snapshot failed"))?;
                Err(internal("read Assistant snapshot failed"))
            }
        }
    }
}

/// One snapshot read inside a caller-owned transaction, before any write-back.
struct LoadedSnapshot {
    row: agent_session::Model,
    session: PersistedAgentSession,
    tasks: Vec<BackgroundTaskRecord>,
    grants: Vec<CapabilityGrant>,
    subagents: desk_agent_protocol::ai_assistant::subagent::AiAssistantDelegationSnapshot,
    goal: Option<desk_diagnose_core::goal::GoalRun>,
    pending_goal_open_request: Option<desk_diagnose_core::goal::GoalOpenRequest>,
    fingerprint: String,
    seq: i64,
}

impl LoadedSnapshot {
    fn is_current(&self) -> bool {
        self.row.snapshot_seq == Some(self.seq)
            && self.row.snapshot_fingerprint.as_deref() == Some(self.fingerprint.as_str())
    }

    fn finish(self) -> AssistantSnapshot {
        let mut snapshot = snapshot_from_session(self.row, self.session);
        snapshot.seq = self.seq;
        snapshot.subagents = self.subagents;
        AssistantSnapshot {
            session: snapshot,
            background_tasks: self.tasks,
            capability_grants: self.grants,
            goal: self.goal,
            pending_goal_open_request: self.pending_goal_open_request,
        }
    }
}

async fn load_snapshot_on(
    txn: &crate::config::connection::DatabaseTransaction,
    run: &str,
    actor: &str,
    device: &str,
) -> Result<Option<LoadedSnapshot>, sea_orm::DbErr> {
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(run))
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::DeviceId.eq(device))
        .one(txn)
        .await?
    else {
        return Ok(None);
    };
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| sea_orm::DbErr::Custom("invalid Assistant snapshot".into()))?;
    if session.surface != AgentSessionSurface::AiAssistant
        || session.actor_id != actor
        || session.device_id != device
        || session.conversation_id != run
        || (device.is_empty() && !session.has_admitted_personal_input())
    {
        return Ok(None);
    }
    let now = u64::try_from(Utc::now().timestamp_millis())
        .map_err(|_| sea_orm::DbErr::Custom("invalid snapshot clock".into()))?;
    let mut tasks = computer_background::list_on(txn, run, actor, device, now).await?;
    let legacy = agent_action_item::Entity::find()
        .filter(
            agent_action_item::Column::Kind.eq(agent_background_task_store::BACKGROUND_ACTION_KIND),
        )
        .filter(agent_action_item::Column::ConversationId.eq(run))
        .filter(agent_action_item::Column::ActorId.eq(actor))
        .filter(agent_action_item::Column::TargetDeviceId.eq(device))
        .order_by_asc(agent_action_item::Column::Id)
        .all(txn)
        .await?;
    for task in legacy {
        tasks.push(agent_background_task_store::decode_record(&task)?);
    }
    let tasks = desk_diagnose_core::dynamic_run::bounded_background_task_projection(
        tasks,
        session.input_revision,
    );
    let grants = SignalCapabilityGrantStore::list_for_subject_on(txn, run, actor, device).await?;
    let subagents = crate::agent_subagent_store::presentation_on(txn, &session).await?;
    let goal = crate::agent_goal_store::load_latest_for_subject(txn, run, actor, device).await?;
    let pending_goal_open_request =
        crate::agent_goal_open_store::pending_for_subject(txn, run, actor, device).await?;
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &row.state_json,
                row.version,
                &tasks,
                &grants,
                &subagents,
                &goal,
                &pending_goal_open_request
            ))
            .map_err(|_| sea_orm::DbErr::Custom("encode Assistant snapshot failed".into()))?
        )
    );
    let previous = row.snapshot_seq.unwrap_or(row.version).max(row.version);
    if previous < 0 {
        return Err(sea_orm::DbErr::Custom(
            "invalid Assistant snapshot sequence".into(),
        ));
    }
    let seq = if row.snapshot_fingerprint.as_deref() == Some(&fingerprint) {
        previous
    } else {
        previous
            .checked_add(1)
            .ok_or_else(|| sea_orm::DbErr::Custom("Assistant snapshot sequence exhausted".into()))?
    };
    Ok(Some(LoadedSnapshot {
        row,
        session,
        tasks,
        grants,
        subagents,
        goal,
        pending_goal_open_request,
        fingerprint,
        seq,
    }))
}
