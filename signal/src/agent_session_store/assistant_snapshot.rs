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
}

impl SignalAgentSessionStore {
    pub(crate) async fn read_assistant_snapshot_for_subject(
        &self,
        run: &str,
        actor: &str,
        device: &str,
    ) -> Result<Option<AssistantSnapshot>, AgentError> {
        let txn = self
            .db
            .begin()
            .await
            .map_err(|_| internal("begin Assistant snapshot failed"))?;
        let result = async {
            // Reserve the SQLite writer before reading so a concurrent session,
            // grant, completion or cancellation cannot split this snapshot.
            let locked = agent_session::Entity::update_many()
                .filter(agent_session::Column::ConversationId.eq(run))
                .filter(agent_session::Column::ActorId.eq(actor))
                .filter(agent_session::Column::DeviceId.eq(device))
                .col_expr(
                    agent_session::Column::Id,
                    Expr::col(agent_session::Column::Id),
                )
                .exec(&txn)
                .await?;
            if locked.rows_affected == 0 {
                return Ok(None);
            }
            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(run))
                .filter(agent_session::Column::ActorId.eq(actor))
                .filter(agent_session::Column::DeviceId.eq(device))
                .one(&txn)
                .await?
                .ok_or_else(|| sea_orm::DbErr::Custom("snapshot subject disappeared".into()))?;
            let session = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| sea_orm::DbErr::Custom("invalid Assistant snapshot".into()))?;
            if session.surface != AgentSessionSurface::DeviceAssistant
                || session.actor_id != actor
                || session.device_id != device
                || session.conversation_id != run
            {
                return Ok(None);
            }
            let now = u64::try_from(Utc::now().timestamp_millis())
                .map_err(|_| sea_orm::DbErr::Custom("invalid snapshot clock".into()))?;
            let mut tasks = computer_background::list_on(&txn, run, actor, device, now).await?;
            let legacy = agent_action_item::Entity::find()
                .filter(
                    agent_action_item::Column::Kind
                        .eq(agent_background_task_store::BACKGROUND_ACTION_KIND),
                )
                .filter(agent_action_item::Column::ConversationId.eq(run))
                .filter(agent_action_item::Column::ActorId.eq(actor))
                .filter(agent_action_item::Column::TargetDeviceId.eq(device))
                .order_by_asc(agent_action_item::Column::Id)
                .all(&txn)
                .await?;
            for task in legacy {
                tasks.push(agent_background_task_store::decode_record(&task)?);
            }
            tasks.sort_by(|left, right| left.task.task_id.cmp(&right.task.task_id));
            let grants =
                SignalCapabilityGrantStore::list_for_subject_on(&txn, run, actor, device).await?;
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(
                    serde_json::to_vec(&(&row.state_json, row.version, &tasks, &grants)).map_err(
                        |_| sea_orm::DbErr::Custom("encode Assistant snapshot failed".into())
                    )?
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
                previous.checked_add(1).ok_or_else(|| {
                    sea_orm::DbErr::Custom("Assistant snapshot sequence exhausted".into())
                })?
            };
            if row.snapshot_seq != Some(seq)
                || row.snapshot_fingerprint.as_deref() != Some(&fingerprint)
            {
                agent_session::Entity::update_many()
                    .filter(agent_session::Column::Id.eq(row.id))
                    .col_expr(agent_session::Column::SnapshotSeq, Expr::value(seq))
                    .col_expr(
                        agent_session::Column::SnapshotFingerprint,
                        Expr::value(fingerprint),
                    )
                    .exec(&txn)
                    .await?;
            }
            let mut snapshot = snapshot_from_row(row)
                .map_err(|_| sea_orm::DbErr::Custom("invalid Assistant snapshot".into()))?;
            snapshot.seq = seq;
            Ok(Some(AssistantSnapshot {
                session: snapshot,
                background_tasks: tasks,
                capability_grants: grants,
            }))
        }
        .await;
        match result {
            Ok(snapshot) => {
                txn.commit()
                    .await
                    .map_err(|_| internal("commit Assistant snapshot failed"))?;
                Ok(snapshot)
            }
            Err(_) => {
                txn.rollback()
                    .await
                    .map_err(|_| internal("rollback Assistant snapshot failed"))?;
                Err(internal("read Assistant snapshot failed"))
            }
        }
    }
}
