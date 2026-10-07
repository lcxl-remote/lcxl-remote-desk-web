//! Current parent authority read under the same task fence as publication and revocation.
use super::publication::{TaskRehearsalEvidence, key, load_contract, valid_digest};
use super::{ScheduleStore, ScheduleStoreError, digest, entity, json};
use crate::entity::{agent_schedule_run as run, agent_task_authorization as authorization};
use desk_agent_protocol::{capability_grant::TaskGrantProvenance, schedule::SchedulePauseReason};
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION, contract::ValidatedTaskContract, lifecycle::FailureState,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, QueryFilter, Set,
    sea_query::{Alias, Expr, Query},
};

/// Database-verified parent binding. This is not a capability grant or a budget reservation.
/// Its validity ends with the caller's transaction; it must never be cached for later dispatch.
pub struct CurrentTaskAuthority {
    provenance: TaskGrantProvenance,
    contract: ValidatedTaskContract,
    run: run::Model,
    verified_at: i64,
    valid_until: i64,
    original_started_at_ms: i64,
    authorization_expires_at_ms: Option<i64>,
}
impl CurrentTaskAuthority {
    pub fn provenance(&self) -> &TaskGrantProvenance {
        &self.provenance
    }
    pub fn contract(&self) -> &ValidatedTaskContract {
        &self.contract
    }
    pub fn run(&self) -> &run::Model {
        &self.run
    }
    pub fn verified_at(&self) -> i64 {
        self.verified_at
    }
    /// Persistable source evidence excludes the renewable planner lease. Callers
    /// must still reacquire current occurrence authority before any new dispatch.
    pub fn delegation_source(
        &self,
    ) -> Result<desk_diagnose_core::subagent::creation::ScheduledCreationSource, ScheduleStoreError>
    {
        desk_diagnose_core::subagent::creation::ScheduledCreationSource::capture(
            self.provenance.clone(),
            &self.contract,
            self.original_started_at_ms,
            self.authorization_expires_at_ms,
        )
        .map_err(|_| ScheduleStoreError::Invalid)
    }

    /// Upper bound for a derived grant, including parent, lease and runtime expiry.
    pub fn valid_until(&self) -> i64 {
        self.valid_until
    }
}

/// A current finite source is not a parent planner lease or a task grant.
/// Only the occurrence budget module can inspect its execution-shaped backing.
pub(crate) struct CurrentDelegationSourceAuthority {
    current: CurrentTaskAuthority,
}

impl CurrentDelegationSourceAuthority {
    pub fn provenance(&self) -> &TaskGrantProvenance {
        self.current.provenance()
    }
    pub fn contract(&self) -> &ValidatedTaskContract {
        self.current.contract()
    }
    pub fn run(&self) -> &run::Model {
        self.current.run()
    }
    pub fn verified_at(&self) -> i64 {
        self.current.verified_at()
    }
    pub fn deadline_ms(&self) -> i64 {
        self.current.valid_until()
    }
    pub(super) fn budget_authority(&self) -> &CurrentTaskAuthority {
        &self.current
    }
}

#[derive(Clone, Copy)]
enum AuthorityPurpose<'a> {
    Planner { node: &'a str, lease_epoch: i64 },
    Approval { node: &'a str, lease_epoch: i64 },
    Delegation(&'a desk_diagnose_core::subagent::creation::ScheduledCreationSource),
}

/// PostgreSQL CURRENT_TIMESTAMP is the transaction start, which can precede a lock wait.
/// Authority expiry is checked against wall time after obtaining the task write fence.
pub(super) async fn authority_now(txn: &DatabaseTransaction) -> Result<i64, ScheduleStoreError> {
    let expression = if txn.get_database_backend() == DbBackend::Postgres {
        Expr::cust("clock_timestamp()")
    } else {
        Expr::cust("strftime('%Y-%m-%dT%H:%M:%fZ', 'now')")
    };
    let query = Query::select()
        .expr_as(expression, Alias::new("now"))
        .to_owned();
    let row = txn
        .query_one(&query)
        .await?
        .ok_or(ScheduleStoreError::Invalid)?;
    Ok(row
        .try_get::<chrono::DateTime<chrono::Utc>>("", "now")?
        .timestamp_millis())
}

/// Approval inspection cannot be passed to model/tool budget or dispatch APIs.
/// It deliberately has no execution-authority accessor.
pub(super) struct PendingTaskApproval {
    pub contract: ValidatedTaskContract,
    pub verified_at: i64,
    pub valid_until: i64,
}

impl ScheduleStore {
    /// Join this fence with current session/device policy, exact-call validation, budget
    /// reservation and grant/outbox writes in ONE short transaction. Roll back on any error.
    /// Do not call providers or models while holding it. Always lock the task before the
    /// session and dispatch rows so revocation and dispatch share a consistent lock order.
    /// Recheck after any additional lock wait before committing derived authority.
    pub async fn lock_run_authority(
        txn: &DatabaseTransaction,
        owner: i32,
        device: &str,
        run_id: &str,
        node: &str,
        lease_epoch: i64,
    ) -> Result<CurrentTaskAuthority, ScheduleStoreError> {
        Self::lock_authority_for(
            txn,
            owner,
            device,
            run_id,
            AuthorityPurpose::Planner { node, lease_epoch },
        )
        .await
    }

    pub(super) async fn lock_waiting_approval(
        txn: &DatabaseTransaction,
        owner: i32,
        device: &str,
        run_id: &str,
        node: &str,
        lease_epoch: i64,
    ) -> Result<PendingTaskApproval, ScheduleStoreError> {
        let current = Self::lock_authority_for(
            txn,
            owner,
            device,
            run_id,
            AuthorityPurpose::Approval { node, lease_epoch },
        )
        .await?;
        Ok(PendingTaskApproval {
            contract: current.contract,
            verified_at: current.verified_at,
            valid_until: current.valid_until,
        })
    }

    /// Callers hold owner/root/child control before this publication fence and
    /// recheck their own planner before dispatch. No device/model I/O is allowed.
    pub(crate) async fn lock_delegation_source_authority(
        txn: &DatabaseTransaction,
        owner: i32,
        device: &str,
        source: &desk_diagnose_core::subagent::creation::ScheduledCreationSource,
    ) -> Result<CurrentDelegationSourceAuthority, ScheduleStoreError> {
        source.validate().map_err(|_| ScheduleStoreError::Invalid)?;
        let current = Self::lock_authority_for(
            txn,
            owner,
            device,
            &source.provenance.scheduled_run_id,
            AuthorityPurpose::Delegation(source),
        )
        .await?;
        if current.delegation_source()? != *source {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(CurrentDelegationSourceAuthority { current })
    }

    async fn lock_authority_for(
        txn: &DatabaseTransaction,
        owner: i32,
        device: &str,
        run_id: &str,
        purpose: AuthorityPurpose<'_>,
    ) -> Result<CurrentTaskAuthority, ScheduleStoreError> {
        if owner <= 0 || device.is_empty() {
            return Err(ScheduleStoreError::Invalid);
        }
        match purpose {
            AuthorityPurpose::Planner { node, lease_epoch }
            | AuthorityPurpose::Approval { node, lease_epoch }
                if node.is_empty() || lease_epoch <= 0 =>
            {
                return Err(ScheduleStoreError::Invalid);
            }
            _ => {}
        }
        let initial = run::Entity::find()
            .filter(run::Column::OwnerUserId.eq(owner))
            .filter(run::Column::RunId.eq(run_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&initial.schedule_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.target_device_id != device
            || task.kind != "fresh_task"
            || task.active_run_id.as_deref() != Some(run_id)
            || !matches!(task.status.as_str(), "active" | "triggered" | "paused")
            || task.calc_version != SCHEDULE_CALC_VERSION
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let touched = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(txn)
            .await?;
        if touched.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = authority_now(txn).await?;
        // Reload after the fence: the first read is only a task locator.
        let work = run::Entity::find_by_id(initial.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.owner_user_id != owner
            || work.schedule_id != task.schedule_id
            || work.run_id != run_id
            || match purpose {
                AuthorityPurpose::Planner { .. } => work.status != "running",
                AuthorityPurpose::Approval { .. } => work.status != "awaiting_permission",
                AuthorityPurpose::Delegation(_) => !matches!(
                    work.status.as_str(),
                    "running" | "awaiting_permission" | "awaiting_children"
                ),
            }
            || work.failure_accounted
            || work.finished_at.is_some()
            || work.cancel_requested_at.is_some()
            || match purpose {
                AuthorityPurpose::Planner { node, lease_epoch } => {
                    work.lease_owner.as_deref() != Some(node)
                        || work.lease_epoch != lease_epoch
                        || work.lease_deadline.is_none_or(|deadline| deadline <= now)
                }
                AuthorityPurpose::Approval { node, lease_epoch } => {
                    work.lease_owner.as_deref() != Some(node)
                        || work.lease_epoch != lease_epoch
                        || work.lease_deadline.is_some()
                }
                AuthorityPurpose::Delegation(source) => {
                    work.conversation_id != work.run_id
                        || work.turn_id != format!("{}-turn", work.run_id)
                        || source.provenance.scheduled_run_id != work.run_id
                }
            }
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let failures: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        // A user pause stops future occurrences; it does not cancel the current run.
        if failures
            .pause_reasons
            .iter()
            .any(|r| *r != SchedulePauseReason::User)
            || i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let snapshot: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if snapshot.schedule_id != task.schedule_id
            || snapshot.owner_user_id != owner
            || snapshot.target_device_id != device
            || snapshot.kind != task.kind
            || snapshot.task_revision != task.task_revision
            || snapshot.prompt != task.prompt
            || snapshot.model_id != task.model_id
            || snapshot.locale != task.locale
            || snapshot.contract_revision != task.contract_revision
            || snapshot.authorization_revision != task.authorization_revision
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let contract_revision = task.contract_revision.ok_or(ScheduleStoreError::Conflict)?;
        let authorization_revision = task
            .authorization_revision
            .ok_or(ScheduleStoreError::Conflict)?;
        let (_, contract) = load_contract(txn, owner, &task.schedule_id, contract_revision).await?;
        let policy = crate::schedule_budget_policy::read(txn).await?;
        if !desk_diagnose_core::schedule::policy::permits(&policy, &contract.contract().budget) {
            return Err(ScheduleStoreError::BudgetExceeded);
        }
        let parent = authorization::Entity::find()
            .filter(authorization::Column::OwnerUserId.eq(owner))
            .filter(authorization::Column::ScheduleId.eq(&task.schedule_id))
            .filter(authorization::Column::AuthorizationRevision.eq(authorization_revision))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if parent.revision_identity != digest(&json(&(&task.schedule_id, authorization_revision))?)
            || parent.task_revision != task.task_revision
            || parent.contract_revision != contract_revision
            || parent.contract_sha256 != contract.digest()
            || parent.revoked_at.is_some()
            || parent.revoked_reason.is_some()
            || parent.version <= 0
            || parent.approved_at <= 0
            || parent.approved_at > now
            || parent.expires_at.is_some_and(|deadline| deadline <= now)
            || contract.contract().task_revision != task.task_revision as u64
            || contract.contract().target_device_id != device
            || contract.contract().prompt_sha256 != digest(&task.prompt)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let proof: TaskRehearsalEvidence = serde_json::from_str(&parent.rehearsal_evidence_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        key(&proof.conversation_id)?;
        key(&proof.rehearsal_run_id)?;
        if !valid_digest(&proof.evidence_sha256)
            || proof.rehearsal_run_id != parent.rehearsal_run_id
            || proof.input_revision == 0
            || proof.input_revision > i64::MAX as u64
            || proof.finished_at <= 0
            || proof.finished_at > parent.approved_at
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let provenance = TaskGrantProvenance {
            schedule_id: task.schedule_id,
            scheduled_run_id: work.run_id.clone(),
            task_revision: u64::try_from(task.task_revision)
                .map_err(|_| ScheduleStoreError::Invalid)?,
            contract_revision: u64::try_from(contract_revision)
                .map_err(|_| ScheduleStoreError::Invalid)?,
            contract_sha256: contract.digest().to_owned(),
            authorization_id: parent.authorization_id,
            authorization_revision: u64::try_from(authorization_revision)
                .map_err(|_| ScheduleStoreError::Invalid)?,
            recovery_epoch: failures.recovery_epoch,
        };
        provenance
            .validate()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let started = work
            .started_at
            .filter(|at| *at > 0 && *at <= now)
            .ok_or(ScheduleStoreError::Invalid)?;
        let runtime = i64::from(contract.contract().budget.max_runtime_seconds)
            .checked_mul(1000)
            .and_then(|limit| started.checked_add(limit))
            .ok_or(ScheduleStoreError::Invalid)?;
        let valid_until = runtime
            .min(match purpose {
                AuthorityPurpose::Planner { .. } => {
                    work.lease_deadline.ok_or(ScheduleStoreError::Invalid)?
                }
                AuthorityPurpose::Approval { .. } | AuthorityPurpose::Delegation(_) => i64::MAX,
            })
            .min(parent.expires_at.unwrap_or(i64::MAX));
        if valid_until <= now {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(CurrentTaskAuthority {
            provenance,
            contract,
            run: work,
            verified_at: now,
            valid_until,
            original_started_at_ms: started,
            authorization_expires_at_ms: parent.expires_at,
        })
    }
}
