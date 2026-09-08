//! Historical identity for receipt verification. Never usable for execution admission.
use super::publication::{TaskRehearsalEvidence, key, load_contract, valid_digest};
use super::{ScheduleStoreError, digest, entity, json};
use crate::entity::{agent_schedule_run as run, agent_task_authorization as authorization};
use desk_agent_protocol::capability_grant::TaskGrantProvenance;
use desk_diagnose_core::schedule::contract::ValidatedTaskContract;
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter};

/// Immutable historical facts, deliberately distinct from CurrentTaskAuthority.
/// Expiry/revocation stop new execution, not inspection of an original receipt.
pub(crate) struct TaskReceiptContext {
    run_id: String,
    contract: ValidatedTaskContract,
    provenance: TaskGrantProvenance,
}
impl TaskReceiptContext {
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }
    pub(crate) fn contract(&self) -> &ValidatedTaskContract {
        &self.contract
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
        let started = work
            .started_at
            .filter(|at| *at > 0)
            .ok_or(ScheduleStoreError::Invalid)?;
        if snapshot.kind != "fresh_task"
            || snapshot.schedule_id != work.schedule_id
            || snapshot.owner_user_id != work.owner_user_id
            || work.conversation_id != work.run_id
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let contract_revision = snapshot
            .contract_revision
            .ok_or(ScheduleStoreError::Invalid)?;
        let authorization_revision = snapshot
            .authorization_revision
            .ok_or(ScheduleStoreError::Invalid)?;
        let (_, contract) = load_contract(
            txn,
            work.owner_user_id,
            &work.schedule_id,
            contract_revision,
        )
        .await?;
        let parent = authorization::Entity::find()
            .filter(authorization::Column::OwnerUserId.eq(work.owner_user_id))
            .filter(authorization::Column::ScheduleId.eq(&work.schedule_id))
            .filter(authorization::Column::AuthorizationRevision.eq(authorization_revision))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if parent.revision_identity != digest(&json(&(&work.schedule_id, authorization_revision))?)
            || parent.task_revision != snapshot.task_revision
            || parent.contract_revision != contract_revision
            || parent.contract_sha256 != contract.digest()
            || parent.version <= 0
            || parent.approved_at <= 0
            || parent.approved_at > started
            || parent.expires_at.is_some_and(|expiry| expiry <= started)
            || i64::try_from(contract.contract().task_revision).ok() != Some(snapshot.task_revision)
            || contract.contract().target_device_id != snapshot.target_device_id
            || contract.contract().prompt_sha256 != digest(&snapshot.prompt)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let proof: TaskRehearsalEvidence = serde_json::from_str(&parent.rehearsal_evidence_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        key(&proof.conversation_id)?;
        key(&proof.rehearsal_run_id)?;
        if proof.rehearsal_run_id != parent.rehearsal_run_id
            || !valid_digest(&proof.evidence_sha256)
            || proof.input_revision == 0
            || proof.finished_at <= 0
            || proof.finished_at > parent.approved_at
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let provenance = TaskGrantProvenance {
            schedule_id: work.schedule_id.clone(),
            scheduled_run_id: work.run_id.clone(),
            task_revision: u64::try_from(snapshot.task_revision)
                .map_err(|_| ScheduleStoreError::Invalid)?,
            contract_revision: u64::try_from(contract_revision)
                .map_err(|_| ScheduleStoreError::Invalid)?,
            contract_sha256: contract.digest().to_owned(),
            authorization_id: parent.authorization_id,
            authorization_revision: u64::try_from(authorization_revision)
                .map_err(|_| ScheduleStoreError::Invalid)?,
            recovery_epoch: u64::try_from(work.recovery_epoch)
                .map_err(|_| ScheduleStoreError::Invalid)?,
        };
        provenance
            .validate()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        Ok(Self {
            run_id: work.run_id.clone(),
            contract,
            provenance,
        })
    }
}
