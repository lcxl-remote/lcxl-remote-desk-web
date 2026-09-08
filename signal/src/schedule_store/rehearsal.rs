//! Reserve a fresh, explicitly requested interactive rehearsal before any execution.
use super::publication::key;
use super::{ScheduleStore, ScheduleStoreError, digest, entity, json};
use crate::entity::agent_task_rehearsal as rehearsal;
use desk_diagnose_core::conversation_key::derive_conversation_key;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};

impl ScheduleStore {
    /// Called only after an authenticated owner requests an actual interactive run.
    /// The reservation supplies neither a successful receipt nor ongoing authority.
    pub async fn reserve_rehearsal(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        client_key: &str,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        key(schedule_id)?;
        key(client_key)?;
        if owner <= 0 || expected_revision <= 0 {
            return Err(ScheduleStoreError::Invalid);
        }
        let identity = digest(&json(&(owner, client_key))?);
        let payload = digest(&json(&(schedule_id, expected_revision))?);
        let txn = self.db.begin().await?;
        let existing = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::CreationIdentity.eq(&identity))
            .one(&txn)
            .await?;
        if let Some(existing) = existing {
            if existing.creation_payload_sha256 != payload {
                return Err(ScheduleStoreError::Conflict);
            }
            // Idempotent reads cannot restart an old rehearsal or restore a task.
            return Ok(existing);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.revision != expected_revision
            || task.kind != "fresh_task"
            || task.active_run_id.is_some()
            || !matches!(
                task.status.as_str(),
                "draft" | "paused" | "awaiting_authorization"
            )
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
        if failures
            .pause_reasons
            .contains(&desk_agent_protocol::schedule::SchedulePauseReason::UnknownSideEffect)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let now = super::queue::database_now(&txn).await?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(expected_revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                status: Set("rehearsing".into()),
                next_run_at: Set(None),
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
        // A paused or edited task still cannot overlap an unfinished rehearsal.
        if rehearsal::Entity::find()
            .filter(rehearsal::Column::ScheduleId.eq(schedule_id))
            .filter(rehearsal::Column::Status.is_in([
                "pending",
                "running",
                "awaiting_permission",
                "outcome_unknown",
            ]))
            .one(&txn)
            .await?
            .is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let client_conversation_id = format!("rehearsal_{}", uuid::Uuid::new_v4());
        let conversation_id = derive_conversation_key(
            &owner.to_string(),
            &task.target_device_id,
            Some(&client_conversation_id),
            "",
        );
        let id = uuid::Uuid::new_v4().to_string();
        rehearsal::Entity::insert(rehearsal::ActiveModel {
            rehearsal_id: Set(id.clone()),
            schedule_id: Set(task.schedule_id),
            owner_user_id: Set(owner),
            task_revision: Set(task.task_revision),
            target_device_id: Set(task.target_device_id),
            prompt_sha256: Set(digest(&task.prompt)),
            prompt: Set(task.prompt),
            locale: Set(task.locale),
            model_id: Set(task.model_id),
            client_conversation_id: Set(client_conversation_id),
            conversation_id: Set(conversation_id),
            status: Set("pending".into()),
            creation_identity: Set(identity),
            creation_payload_sha256: Set(payload),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec_without_returning(&txn)
        .await?;
        let result = rehearsal::Entity::find()
            .filter(rehearsal::Column::RehearsalId.eq(id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::Invalid)?;
        txn.commit().await?;
        Ok(result)
    }

    pub async fn read_rehearsal(
        &self,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)
    }

    /// Cancel only a reservation that has not been claimed for execution.
    /// Running work must use its session cancellation path, never this shortcut.
    pub async fn cancel_pending_rehearsal(
        &self,
        owner: i32,
        rehearsal_id: &str,
        expected_revision: i64,
    ) -> Result<rehearsal::Model, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let row = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.status == "cancelled" {
            return Ok(row);
        }
        if row.status != "pending" || row.started_at.is_some() {
            return Err(ScheduleStoreError::Conflict);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&row.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let now = super::queue::database_now(&txn).await?;
        // Every rehearsal transition takes the task lock before the rehearsal row.
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(expected_revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                status: Set(if task.status == "rehearsing" {
                    "draft".into()
                } else {
                    task.status
                }),
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
        let changed = rehearsal::Entity::update_many()
            .set(rehearsal::ActiveModel {
                status: Set("cancelled".into()),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(rehearsal::Column::Id.eq(row.id))
            .filter(rehearsal::Column::Status.eq("pending"))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = rehearsal::Entity::find_by_id(row.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::Invalid)?;
        txn.commit().await?;
        Ok(result)
    }
}

mod finish;
mod input;
mod start;
pub(crate) use input::validate_rehearsal_input_on;

#[cfg(test)]
mod tests;

mod reads;
pub use reads::{ObservedRehearsalRead, RehearsalReadReport};

mod transport;

mod cancel;

mod recovery;
pub use recovery::RehearsalRecoveryReport;

mod publication_evidence;

mod read_sources;
pub use read_sources::RehearsalToolSource;

mod actions;

mod contract_draft;
