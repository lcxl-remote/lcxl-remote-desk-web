//! Resume a published task against its existing authorization; never renew grants.
use super::publication::{TaskPublicationVerifier, TaskRehearsalEvidence};
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_task_authorization as authorization;
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect};

struct PublishedAuthorizer<'a>(&'a dyn TaskPublicationVerifier);

#[async_trait::async_trait(?Send)]
impl super::recovery::ScheduleAuthorizer for PublishedAuthorizer<'_> {
    async fn authorize(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
    ) -> Result<(), ScheduleStoreError> {
        if task.kind != "fresh_task" {
            return Err(ScheduleStoreError::Invalid);
        }
        self.0.lock_subject(txn, task).await?;
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let contract_revision = task.contract_revision.ok_or(ScheduleStoreError::Conflict)?;
        let authorization_revision = task
            .authorization_revision
            .ok_or(ScheduleStoreError::Conflict)?;
        let record = authorization::Entity::find()
            .filter(authorization::Column::ScheduleId.eq(&task.schedule_id))
            .filter(authorization::Column::OwnerUserId.eq(task.owner_user_id))
            .filter(authorization::Column::AuthorizationRevision.eq(authorization_revision))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::Conflict)?;
        if record.revoked_at.is_some()
            || record.task_revision != task.task_revision
            || record.contract_revision != contract_revision
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let (stored, contract) = super::publication::load_contract(
            txn,
            task.owner_user_id,
            &task.schedule_id,
            contract_revision,
        )
        .await?;
        if record.contract_sha256 != stored.digest_sha256 {
            return Err(ScheduleStoreError::Conflict);
        }
        let original: TaskRehearsalEvidence = serde_json::from_str(&record.rehearsal_evidence_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let current = self
            .0
            .verify(
                txn,
                task,
                &contract,
                &record.rehearsal_run_id,
                record.expires_at,
            )
            .await?;
        if current != original {
            return Err(ScheduleStoreError::Conflict);
        }
        if authorization::Entity::find_by_id(record.id)
            .lock_exclusive()
            .one(txn)
            .await?
            .as_ref()
            != Some(&record)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::authority::authority_now(txn).await?;
        if record.approved_at > now || record.expires_at.is_some_and(|expiry| expiry <= now) {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(())
    }
}

impl ScheduleStore {
    pub async fn resume_published_task(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        verifier: &dyn TaskPublicationVerifier,
    ) -> Result<entity::Model, ScheduleStoreError> {
        self.resume(
            owner,
            schedule_id,
            expected_revision,
            &PublishedAuthorizer(verifier),
        )
        .await
    }
}
