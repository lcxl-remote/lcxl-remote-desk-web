//! Immutable task contracts and explicit owner publication in one schedule transaction.
use super::queue::database_now;
use super::{ScheduleStore, ScheduleStoreError, digest, entity, json};
use crate::entity::{
    agent_task_authorization as authorization, agent_task_contract as contract_row,
};
use desk_agent_protocol::schedule::{SchedulePauseReason, contract::TaskContract};
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION,
    contract::{ValidatedTaskContract, parse_contract, validate_contract},
    lifecycle::FailureState,
    next_after, parse_json, validate_publication,
};
#[cfg(test)]
use sea_orm::TransactionTrait;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, Set,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct PublishTask {
    pub schedule_id: String,
    pub expected_revision: i64,
    pub contract_revision: i64,
    pub contract_sha256: String,
    pub rehearsal_run_id: String,
    pub expires_at: Option<i64>,
    pub client_publish_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRehearsalEvidence {
    pub rehearsal_run_id: String,
    pub conversation_id: String,
    pub input_revision: u64,
    pub evidence_sha256: String,
    pub finished_at: i64,
}

/// The runtime verifies actual successful calls/receipts, coverage, owner/device policy,
/// model availability and platform budgets under this same transaction. A model's
/// permission summary or a client-provided success flag is never evidence.
#[async_trait::async_trait]
pub trait TaskPublicationVerifier: Send + Sync {
    /// Lock the current owner/device before any task or session write fence.
    /// Implementations must retain these locks on the supplied transaction.
    async fn lock_subject(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
    ) -> Result<(), ScheduleStoreError>;

    async fn verify(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
        contract: &ValidatedTaskContract,
        rehearsal_run_id: &str,
        expires_at: Option<i64>,
    ) -> Result<TaskRehearsalEvidence, ScheduleStoreError>;
}

pub(super) fn key(value: &str) -> Result<(), ScheduleStoreError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok(())
}
pub(super) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
async fn owned<C: ConnectionTrait>(
    db: &C,
    owner: i32,
    id: &str,
) -> Result<entity::Model, ScheduleStoreError> {
    entity::Entity::find()
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::ScheduleId.eq(id))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)
}
pub(super) async fn load_contract<C: ConnectionTrait>(
    db: &C,
    owner: i32,
    schedule: &str,
    revision: i64,
) -> Result<(contract_row::Model, ValidatedTaskContract), ScheduleStoreError> {
    let row = contract_row::Entity::find()
        .filter(contract_row::Column::OwnerUserId.eq(owner))
        .filter(contract_row::Column::ScheduleId.eq(schedule))
        .filter(contract_row::Column::ContractRevision.eq(revision))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let parsed = parse_contract(&row.canonical_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if parsed.digest() != row.digest_sha256
        || parsed.canonical_json() != row.canonical_json
        || parsed.contract().schedule_id != row.schedule_id
        || i64::try_from(parsed.contract().task_revision).ok() != Some(row.task_revision)
        || i64::try_from(parsed.contract().contract_revision).ok() != Some(row.contract_revision)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok((row, parsed))
}

impl ScheduleStore {
    pub async fn read_contract(
        &self,
        owner: i32,
        schedule: &str,
        revision: i64,
    ) -> Result<contract_row::Model, ScheduleStoreError> {
        Ok(load_contract(&self.db, owner, schedule, revision).await?.0)
    }

    /// Saving a definition does not approve it or create authority. Revisions are server-assigned.
    pub async fn save_contract(
        &self,
        owner: i32,
        expected_revision: i64,
        input: &TaskContract,
    ) -> Result<contract_row::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let now = database_now(&txn).await?;
        let task = owned(&txn, owner, &input.schedule_id).await?;
        if task.revision != expected_revision
            || task.active_run_id.is_some()
            || !matches!(
                task.status.as_str(),
                "draft" | "awaiting_authorization" | "paused"
            )
            || task.kind != "fresh_task"
            || i64::try_from(input.task_revision).ok() != Some(task.task_revision)
            || input.target_device_id != task.target_device_id
            || input.prompt_sha256 != digest(&task.prompt)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let previous = contract_row::Entity::find()
            .filter(contract_row::Column::ScheduleId.eq(&task.schedule_id))
            .order_by_desc(contract_row::Column::ContractRevision)
            .one(&txn)
            .await?;
        let revision = previous
            .map_or(0, |r| r.contract_revision)
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let mut input = input.clone();
        input.contract_revision = revision as u64;
        let validated = validate_contract(&input).map_err(|_| ScheduleStoreError::Invalid)?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                contract_revision: Set(Some(revision)),
                authorization_revision: Set(None),
                next_run_at: Set(None),
                status: Set(if task.status == "draft" {
                    "draft"
                } else {
                    "awaiting_authorization"
                }
                .into()),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(expected_revision))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        contract_row::Entity::insert(contract_row::ActiveModel {
            contract_id: Set(uuid::Uuid::new_v4().to_string()),
            schedule_id: Set(task.schedule_id.clone()),
            owner_user_id: Set(owner),
            task_revision: Set(task.task_revision),
            contract_revision: Set(revision),
            revision_identity: Set(digest(&json(&(&task.schedule_id, revision))?)),
            digest_sha256: Set(validated.digest().into()),
            canonical_json: Set(validated.canonical_json().into()),
            created_at: Set(now),
            ..Default::default()
        })
        .exec_without_returning(&txn)
        .await?;
        let result = load_contract(&txn, owner, &task.schedule_id, revision)
            .await?
            .0;
        txn.commit().await?;
        Ok(result)
    }

    /// The authenticated owner confirms the exact stored digest. The verifier is mandatory.
    pub async fn publish_task(
        &self,
        owner: i32,
        input: &PublishTask,
        verifier: &dyn TaskPublicationVerifier,
    ) -> Result<authorization::Model, ScheduleStoreError> {
        key(&input.client_publish_key)?;
        key(&input.rehearsal_run_id)?;
        if !valid_digest(&input.contract_sha256) {
            return Err(ScheduleStoreError::Invalid);
        }
        let identity = digest(&json(&(&input.schedule_id, &input.client_publish_key))?);
        let payload_digest = digest(&json(input)?);
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        if let Some(existing) = authorization::Entity::find()
            .filter(authorization::Column::OwnerUserId.eq(owner))
            .filter(authorization::Column::PublicationIdentity.eq(&identity))
            .one(&txn)
            .await?
        {
            if existing.publication_payload_sha256 != payload_digest {
                return Err(ScheduleStoreError::Conflict);
            }
            return Ok(existing);
        }
        let locator = owned(&txn, owner, &input.schedule_id).await?;
        verifier.lock_subject(&txn, &locator).await?;
        // A subject lock may have waited while another request edited the task.
        let task = owned(&txn, owner, &input.schedule_id).await?;
        if task.id != locator.id || task.target_device_id != locator.target_device_id {
            return Err(ScheduleStoreError::Conflict);
        }
        // Serialize publication with edits, revocation and evidence collection before
        // taking the time used for authority checks. This fence changes no revision.
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(&txn).await?;
        if task.kind != "fresh_task"
            || task.revision != input.expected_revision
            || task.active_run_id.is_some()
            || !matches!(
                task.status.as_str(),
                "draft" | "awaiting_authorization" | "paused"
            )
            || task.contract_revision != Some(input.contract_revision)
            || task.calc_version != SCHEDULE_CALC_VERSION
            || input.expires_at.is_some_and(|at| at <= now)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let (_, contract) =
            load_contract(&txn, owner, &task.schedule_id, input.contract_revision).await?;
        if contract.digest() != input.contract_sha256
            || contract.contract().task_revision != task.task_revision as u64
            || contract.contract().target_device_id != task.target_device_id
            || contract.contract().prompt_sha256 != digest(&task.prompt)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let mut failures: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if failures
            .pause_reasons
            .contains(&SchedulePauseReason::UnknownSideEffect)
            || failures
                .pause_reasons
                .contains(&SchedulePauseReason::ScheduleUpgradeRequired)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let spec = parse_json(&task.spec_json).map_err(|_| ScheduleStoreError::Invalid)?;
        let spec =
            validate_publication(&spec, now, false).map_err(|_| ScheduleStoreError::Invalid)?;
        let proof = verifier
            .verify(
                &txn,
                &task,
                &contract,
                &input.rehearsal_run_id,
                input.expires_at,
            )
            .await?;
        // Evidence verification may wait for session/action locks. An elapsed
        // authorization or once time must not become active after that wait.
        let now = super::authority::authority_now(&txn).await?;
        if input.expires_at.is_some_and(|at| at <= now) {
            return Err(ScheduleStoreError::Conflict);
        }
        let spec =
            validate_publication(&spec, now, false).map_err(|_| ScheduleStoreError::Invalid)?;
        key(&proof.conversation_id)?;
        if proof.rehearsal_run_id != input.rehearsal_run_id
            || proof.input_revision == 0
            || proof.input_revision > i64::MAX as u64
            || proof.finished_at <= 0
            || proof.finished_at > now
            || !valid_digest(&proof.evidence_sha256)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let previous = authorization::Entity::find()
            .filter(authorization::Column::ScheduleId.eq(&task.schedule_id))
            .order_by_desc(authorization::Column::AuthorizationRevision)
            .one(&txn)
            .await?;
        let revision = previous
            .map_or(0, |r| r.authorization_revision)
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        failures
            .pause_reasons
            .remove(&SchedulePauseReason::AuthorizationInvalid);
        failures.resume().map_err(|_| ScheduleStoreError::Invalid)?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                authorization_revision: Set(Some(revision)),
                status: Set("active".into()),
                failure_state_json: Set(json(&failures)?),
                next_run_at: Set(next_after(&spec, now).map_err(|_| ScheduleStoreError::Invalid)?),
                recurrence_cursor_at: Set(Some(now)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(input.expected_revision))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let authorization_id = uuid::Uuid::new_v4().to_string();
        authorization::Entity::insert(authorization::ActiveModel {
            authorization_id: Set(authorization_id.clone()),
            schedule_id: Set(task.schedule_id.clone()),
            owner_user_id: Set(owner),
            task_revision: Set(task.task_revision),
            contract_revision: Set(input.contract_revision),
            contract_sha256: Set(input.contract_sha256.clone()),
            authorization_revision: Set(revision),
            revision_identity: Set(digest(&json(&(&task.schedule_id, revision))?)),
            publication_identity: Set(identity),
            publication_payload_sha256: Set(payload_digest),
            rehearsal_run_id: Set(input.rehearsal_run_id.clone()),
            rehearsal_evidence_json: Set(json(&proof)?),
            approved_at: Set(now),
            expires_at: Set(input.expires_at),
            revoked_at: Set(None),
            revoked_reason: Set(None),
            version: Set(1),
            ..Default::default()
        })
        .exec_without_returning(&txn)
        .await?;
        let result = authorization::Entity::find()
            .filter(authorization::Column::AuthorizationId.eq(&authorization_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        Ok(result)
    }
}

#[cfg(test)]
pub(super) mod tests;
