//! Historical identity for receipt verification. Never usable for execution admission.
use super::{ScheduleStoreError, entity};
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::{
    capability_grant::TaskGrantProvenance, schedule::policy::ScheduleBudgetPolicy,
};
use sea_orm::EntityTrait;

/// Immutable historical facts, deliberately distinct from CurrentTaskAuthority.
/// Expiry stops new execution, not inspection of an original receipt.
pub(crate) struct TaskReceiptContext {
    run_id: String,
    target_device_id: String,
    provenance: TaskGrantProvenance,
}
impl TaskReceiptContext {
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }
    /// The device the occurrence ran on.
    pub(crate) fn target_device_id(&self) -> &str {
        &self.target_device_id
    }
    pub(crate) fn provenance(&self) -> &TaskGrantProvenance {
        &self.provenance
    }

    /// The recovery caller owns task/session fences. Reloading rejects a caller's
    /// invented snapshot; no current task revision is substituted for this run.
    pub(crate) async fn load(
        txn: &DatabaseTransaction,
        work: &run::Model,
    ) -> Result<Self, ScheduleStoreError> {
        if run::Entity::find_by_id(work.id).one(txn).await?.as_ref() != Some(work) {
            return Err(ScheduleStoreError::Conflict);
        }
        let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let policy: ScheduleBudgetPolicy = serde_json::from_str(
            work.budget_policy_json
                .as_deref()
                .ok_or(ScheduleStoreError::Invalid)?,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
        if snapshot.kind != "fresh_task"
            || snapshot.schedule_id != work.schedule_id
            || snapshot.owner_user_id != work.owner_user_id
            || work.conversation_id != work.run_id
            || work.started_at.is_none_or(|at| at <= 0)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let provenance = TaskGrantProvenance {
            schedule_id: work.schedule_id.clone(),
            scheduled_run_id: work.run_id.clone(),
            task_revision: u64::try_from(snapshot.task_revision)
                .map_err(|_| ScheduleStoreError::Invalid)?,
            policy_revision: policy.revision,
            recovery_epoch: u64::try_from(work.recovery_epoch)
                .map_err(|_| ScheduleStoreError::Invalid)?,
        };
        provenance
            .validate()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        Ok(Self {
            run_id: work.run_id.clone(),
            target_device_id: snapshot.target_device_id,
            provenance,
        })
    }
}
