//! UI attention acknowledgment never consumes model observations or restarts planning.
use super::*;
use crate::entity::agent_subagent_inbox as inbox;

impl SubAgentStore {
    pub async fn mark_ui_read_for_owner(
        &self,
        root: &str,
        actor: &str,
        device: &str,
        task_id: &str,
        through_state_revision: u64,
    ) -> Result<bool, DbErr> {
        if !desk_diagnose_core::subagent::valid_id(task_id) || through_state_revision == 0 {
            return Err(invalid());
        }
        let through = i64::try_from(through_state_revision).map_err(|_| invalid())?;
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        parent_on(&txn, root, actor, device).await?;
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .filter(run_row::Column::RootConversationId.eq(root))
            .filter(run_row::Column::ActorId.eq(actor))
            .filter(run_row::Column::DeviceId.eq(device))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        if through_state_revision > decode_run(&row)?.state_revision {
            return Err(invalid());
        }
        let changed = inbox::Entity::update_many()
            .set(inbox::ActiveModel {
                ui_read_at_ms: Set(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            })
            .filter(inbox::Column::RootConversationId.eq(root))
            .filter(inbox::Column::TaskId.eq(task_id))
            .filter(inbox::Column::ActorId.eq(actor))
            .filter(inbox::Column::DeviceId.eq(device))
            .filter(inbox::Column::StateRevision.lte(through))
            .filter(inbox::Column::UiReadAtMs.is_null())
            .exec(&txn)
            .await?;
        txn.commit().await?;
        Ok(changed.rows_affected > 0)
    }
}
