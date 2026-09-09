//! Owner-scoped durable draft storage; transport authorization happens upstream.
use crate::entity::agent_schedule as entity;
use desk_agent_protocol::schedule::ScheduleDraft;
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION, lifecycle::FailureState, normalize_draft,
};
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::OnConflict,
};
use sha2::{Digest, Sha256};

mod authority;
mod history;
mod model_management;
mod proposal;
mod review_wait;
mod search;
pub use history::RunHistoryPage;
mod fresh_admission;
mod fresh_model;
mod fresh_recovery;
mod fresh_result;
mod fresh_session;
mod receipt_context;
pub(crate) use receipt_context::TaskReceiptContext;
mod approval_admission;
mod approval_claim;
mod approval_expiry;
mod approval_request;
mod directory_receipt;
mod fresh_permission_recovery;
pub(crate) use approval_admission::lock_fresh_approval_on;
pub(crate) use approval_request::{
    task_directory_resolution_matches_on, validate_task_directory_on, validate_task_permission_on,
};
pub use fresh_admission::{ClaimedFreshTask, FreshTaskClaim};
pub use fresh_model::FreshModelDispatch;
mod materializer;
pub use materializer::ScheduleScanReport;
mod budget;
pub use authority::CurrentTaskAuthority;
pub use budget::{TaskBudgetKind, TaskBudgetRequest};
mod cancel;
mod device_wait;
mod edit;
mod publication;
mod publication_runtime;
#[cfg(test)]
pub(crate) use publication::tests::{
    Verifier as TestPublicationVerifier, fixture_on as publication_test_fixture,
};
pub use publication_runtime::SignalTaskPublicationVerifier;
mod rehearsal;
pub(crate) use rehearsal::validate_rehearsal_input_on;
pub use rehearsal::{
    ObservedRehearsalRead, RehearsalReadReport, RehearsalRecoveryReport, RehearsalToolSource,
};
mod queue;
mod recovery;
mod resume_admission;
pub(crate) use resume_admission::{fresh_action_authority_on, lock_action_session};
mod continuation_candidates;
mod continuation_heartbeat;
mod resume_activation;
mod resume_claim;
mod resume_lease;
pub use continuation_heartbeat::ScheduleHeartbeat;
pub use resume_claim::{ClaimedContinuation, ContinuationClaim, ContinuationPermissionClaim};
pub use resume_lease::{ContinuationLease, FreshTaskLease};
mod revocation;
pub use publication::{PublishTask, TaskPublicationVerifier, TaskRehearsalEvidence};
pub use recovery::ScheduleAuthorizer;
mod policy_admission;
mod resume_result;
mod settlement;

#[derive(Debug)]
pub enum ScheduleStoreError {
    Invalid,
    NotFound,
    Conflict,
    BudgetExceeded,
    Backend(DbErr),
}
impl From<DbErr> for ScheduleStoreError {
    fn from(error: DbErr) -> Self {
        Self::Backend(error)
    }
}
fn json<T: serde::Serialize>(value: &T) -> Result<String, ScheduleStoreError> {
    serde_json::to_string(value).map_err(|_| ScheduleStoreError::Invalid)
}
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[derive(Clone)]
pub struct ScheduleStore {
    db: DatabaseConnection,
}
impl ScheduleStore {
    pub(crate) async fn database_time(&self) -> Result<i64, ScheduleStoreError> {
        queue::database_now(&self.db).await
    }

    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Pausing scheduling never pretends to cancel an already dispatched action.
    pub async fn pause(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        now_ms: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        use crate::entity::agent_schedule_run as work_entity;
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let row = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.revision != expected_revision {
            return Err(ScheduleStoreError::Conflict);
        }
        if !matches!(row.status.as_str(), "active" | "triggered" | "paused") || now_ms < 0 {
            return Err(ScheduleStoreError::Invalid);
        }
        let mut failures: FailureState = serde_json::from_str(&row.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        failures
            .pause_reasons
            .insert(desk_agent_protocol::schedule::SchedulePauseReason::User);
        let revision = row
            .revision
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let pending = if let Some(run_id) = row.active_run_id.as_deref() {
            work_entity::Entity::find()
                .filter(work_entity::Column::RunId.eq(run_id))
                .one(&txn)
                .await?
                .filter(|work| matches!(work.status.as_str(), "queued" | "waiting_device"))
        } else {
            None
        };
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                status: Set("paused".into()),
                active_run_id: Set(if pending.is_some() {
                    None
                } else {
                    row.active_run_id.clone()
                }),
                next_run_at: Set(None),
                revision: Set(revision),
                failure_state_json: Set(json(&failures)?),
                updated_at: Set(now_ms),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(row.id))
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::Revision.eq(expected_revision))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        if let Some(work) = pending {
            let cancelled = work_entity::Entity::update_many()
                .set(work_entity::ActiveModel {
                    status: Set("cancelled".into()),
                    cancel_requested_at: Set(Some(now_ms)),
                    failure_accounted: Set(true),
                    finished_at: Set(Some(now_ms)),
                    updated_at: Set(now_ms),
                    ..Default::default()
                })
                .filter(work_entity::Column::Id.eq(work.id))
                .filter(work_entity::Column::Status.eq(work.status))
                .filter(work_entity::Column::LeaseEpoch.eq(work.lease_epoch))
                .exec(&txn)
                .await?;
            if cancelled.rows_affected != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
        }
        txn.commit().await?;
        self.read(owner, schedule_id).await
    }

    /// Owner is derived from the authenticated control subject, never a model field.
    pub async fn create_draft(
        &self,
        owner: i32,
        request: &ScheduleDraft,
        now_ms: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        Self::create_draft_on(&self.db, owner, request, now_ms).await
    }

    pub(crate) async fn create_draft_on<C: sea_orm::ConnectionTrait>(
        db: &C,
        owner: i32,
        request: &ScheduleDraft,
        now_ms: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if owner <= 0 || now_ms < 0 {
            return Err(ScheduleStoreError::Invalid);
        }
        let request = normalize_draft(request).map_err(|_| ScheduleStoreError::Invalid)?;
        let creation_identity = digest(&json(&(owner, &request.client_create_key))?);
        let payload_hash = digest(&json(&request)?);
        let model = entity::ActiveModel {
            schedule_id: Set(uuid::Uuid::new_v4().to_string()),
            owner_user_id: Set(owner),
            target_device_id: Set(request.target_device_id),
            kind: Set(json(&request.kind)?.trim_matches('"').to_string()),
            status: Set("draft".into()),
            title: Set(request.title),
            prompt: Set(request.prompt),
            locale: Set(request.locale),
            model_id: Set(request.model_id),
            spec_json: Set(json(&request.spec)?),
            calc_version: Set(SCHEDULE_CALC_VERSION.into()),
            revision: Set(1),
            task_revision: Set(1),
            next_run_at: Set(None),
            recurrence_cursor_at: Set(None),
            grace_seconds: Set(3600),
            creation_source: Set(json(&request.creation_source)?
                .trim_matches('"')
                .to_string()),
            creation_identity: Set(creation_identity.clone()),
            creation_payload_sha256: Set(payload_hash.clone()),
            source_conversation_id: Set(request.source_conversation_id),
            requirement_revision: Set(request.requirement_revision.map(|r| r as i64)),
            contract_revision: Set(None),
            authorization_revision: Set(None),
            failure_state_json: Set(json(&FailureState::default())?),
            active_run_id: Set(None),
            created_at: Set(now_ms),
            updated_at: Set(now_ms),
            ..Default::default()
        };
        entity::Entity::insert(model)
            .on_conflict(
                OnConflict::column(entity::Column::CreationIdentity)
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(db)
            .await?;
        let row = entity::Entity::find()
            .filter(entity::Column::CreationIdentity.eq(creation_identity))
            .filter(entity::Column::OwnerUserId.eq(owner))
            .one(db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.creation_payload_sha256 != payload_hash {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(row)
    }

    pub async fn read(
        &self,
        owner: i32,
        schedule_id: &str,
    ) -> Result<entity::Model, ScheduleStoreError> {
        entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)
    }

    /// The stable primary-key cursor prevents duplicate rows during concurrent inserts.
    pub async fn list(
        &self,
        owner: i32,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<entity::Model>, ScheduleStoreError> {
        if owner <= 0 || after_id < 0 || limit == 0 || limit > 100 {
            return Err(ScheduleStoreError::Invalid);
        }
        Ok(entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::Id.gt(after_id))
            .filter(entity::Column::Status.ne("deleted"))
            .order_by_asc(entity::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pause_preserves_active_work_and_blockers_and_rejects_stale_revision() {
        use desk_agent_protocol::schedule::SchedulePauseReason;
        let store = store().await;
        let row = store.create_draft(1, &draft(), 1000).await.unwrap();
        let mut failures = FailureState::default();
        failures
            .pause_reasons
            .insert(SchedulePauseReason::UnknownSideEffect);
        entity::Entity::update_many()
            .set(entity::ActiveModel {
                status: Set("active".into()),
                next_run_at: Set(Some(5000)),
                active_run_id: Set(Some("running-1".into())),
                failure_state_json: Set(json(&failures).unwrap()),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(row.id))
            .exec(&store.db)
            .await
            .unwrap();
        let paused = store.pause(1, &row.schedule_id, 1, 2000).await.unwrap();
        assert_eq!(paused.active_run_id.as_deref(), Some("running-1"));
        assert!(paused.next_run_at.is_none());
        let state: FailureState = serde_json::from_str(&paused.failure_state_json).unwrap();
        assert!(state.pause_reasons.contains(&SchedulePauseReason::User));
        assert!(
            state
                .pause_reasons
                .contains(&SchedulePauseReason::UnknownSideEffect)
        );
        assert!(matches!(
            store.pause(1, &row.schedule_id, 1, 3000).await,
            Err(ScheduleStoreError::Conflict)
        ));
        assert!(matches!(
            store.pause(2, &row.schedule_id, 2, 3000).await,
            Err(ScheduleStoreError::NotFound)
        ));
    }
    use desk_agent_protocol::schedule::{
        ScheduleCreationSource, ScheduleRule, ScheduleSpec, ScheduledTaskKind,
    };
    use sea_orm::{ConnectionTrait, Database, Schema};
    pub(super) fn draft() -> ScheduleDraft {
        ScheduleDraft {
            time_confirmation: None,
            client_create_key: "create-1".into(),
            kind: ScheduledTaskKind::FreshTask,
            target_device_id: "device-1".into(),
            title: "Report".into(),
            prompt: "Write a daily report".into(),
            locale: Some("en".into()),
            model_id: None,
            spec: ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Daily {
                    utc_time: "06:00:00".into(),
                },
            },
            source_conversation_id: None,
            requirement_revision: None,
            creation_source: ScheduleCreationSource::Manual,
        }
    }
    pub(super) async fn store() -> ScheduleStore {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(crate::entity::schedule_budget_policy::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(crate::entity::agent_schedule_run::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(entity::Entity))
            .await
            .unwrap();
        for index in schema.create_index_from_entity(entity::Entity) {
            db.execute(&index).await.unwrap();
        }
        ScheduleStore::new(db)
    }
    #[tokio::test]
    async fn create_is_idempotent_but_changed_payload_conflicts() {
        let store = store().await;
        let input = draft();
        let first = store.create_draft(1, &input, 1000).await.unwrap();
        let second = store.create_draft(1, &input, 2000).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.status, "draft");
        assert!(first.next_run_at.is_none());
        assert!(first.authorization_revision.is_none());
        let mut changed = input.clone();
        changed.prompt = "Different requirement".into();
        assert!(matches!(
            store.create_draft(1, &changed, 3000).await,
            Err(ScheduleStoreError::Conflict)
        ));
        assert_eq!(store.list(1, 0, 100).await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn ownership_and_client_keys_are_isolated_and_pagination_is_bounded() {
        let store = store().await;
        let first = store.create_draft(1, &draft(), 1000).await.unwrap();
        let other = store.create_draft(2, &draft(), 1000).await.unwrap();
        assert_ne!(first.schedule_id, other.schedule_id);
        assert!(matches!(
            store.read(2, &first.schedule_id).await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert_eq!(store.list(2, 0, 100).await.unwrap(), vec![other]);
        assert!(store.list(1, first.id, 100).await.unwrap().is_empty());
        assert!(matches!(
            store.list(1, 0, 101).await,
            Err(ScheduleStoreError::Invalid)
        ));
    }
}

mod late_receipts;

mod outcome_review;
pub(crate) use outcome_review::reviewed_at as outcome_reviewed_at;

mod resume_published;

mod cancel_actions;
