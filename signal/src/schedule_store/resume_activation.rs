//! Enable tasks: owner creation, AI proposal decisions and resumes. Enabling
//! schedules the calendar only and never grants a tool or device permission.
use super::{ScheduleStore, ScheduleStoreError, entity};
pub use desk_assistant_data_store::schedule::ProposalDecider;
use desk_assistant_data_store::schedule::{find_task, record_decision_on};

const HOST: super::session_host::SignalScheduleHost = super::session_host::SignalScheduleHost;

impl ScheduleStore {
    /// Owner creation enables the task at once; there is no draft state.
    pub async fn create_task(
        &self,
        owner: i32,
        request: &desk_agent_protocol::schedule::ScheduleCreation,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if request.creation_source != desk_agent_protocol::schedule::ScheduleCreationSource::Manual
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let now = desk_assistant_data_store::schedule::authority_now(&txn).await?;
        let task = desk_diagnose_core::future::send_phase(|| {
            Self::create_task_on(&txn, owner, request, now, None)
        })
        .await?;
        let result = if task.status == "pending_review" {
            let enabled = Self::activate_on(&txn, owner, &task, false).await?;
            if enabled.kind == "conversation_resume" {
                record_decision_on(&HOST, &txn, &enabled, None).await?;
            }
            enabled
        } else {
            task
        };
        txn.commit().await?;
        Ok(result)
    }

    /// Resume a paused task while its device is ready. A continuation also
    /// needs its original requirement and a future time.
    pub async fn resume_task(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        verifier: &dyn super::TaskSubjectVerifier,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let task = find_task(&txn, owner, schedule_id, expected_revision).await?;
        if task.status != "paused" {
            return Err(ScheduleStoreError::Conflict);
        }
        // Recheck the current account/device gate before acquiring the task
        // lock. This enables the calendar only, never restores consumed grants.
        verifier.lock_subject(&txn, &task).await?;
        let result = Self::activate_on(&txn, owner, &task, true).await?;
        if result.kind == "conversation_resume" {
            record_decision_on(&HOST, &txn, &result, None).await?;
        }
        txn.commit().await?;
        Ok(result)
    }

    /// Decide a pending AI fresh-task proposal. Approval enables the task as
    /// proposed; rejection deletes it. The decision is written back to the
    /// waiting proposing turn. Neither grants a tool or device permission.
    pub async fn decide_proposal(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        approve: bool,
        decider: ProposalDecider,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = Self::decide_proposal_on(
            &txn,
            owner,
            schedule_id,
            expected_revision,
            approve,
            &decider,
        )
        .await?;
        txn.commit().await?;
        Ok(result)
    }

    /// `decide_proposal` inside the caller's transaction, for a decision that
    /// commits together with its review record.
    pub(crate) async fn decide_proposal_on(
        txn: &crate::config::connection::DatabaseTransaction,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        approve: bool,
        decider: &ProposalDecider,
    ) -> Result<entity::Model, ScheduleStoreError> {
        desk_assistant_data_store::schedule::decide_proposal_on(
            &HOST,
            txn,
            owner,
            schedule_id,
            expected_revision,
            approve,
            decider,
        )
        .await
    }

    /// Enable a pending or paused task inside the caller's transaction.
    pub(super) async fn activate_on(
        txn: &crate::config::connection::DatabaseTransaction,
        owner: i32,
        task: &entity::Model,
        resume: bool,
    ) -> Result<entity::Model, ScheduleStoreError> {
        desk_assistant_data_store::schedule::activate_on(&HOST, txn, owner, task, resume).await
    }
}

impl ScheduleStore {
    /// Test helper: enable a pending task as owner creation does, with the
    /// continuation's activation receipt.
    #[cfg(test)]
    pub(crate) async fn activate_pending_for_test(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let task = find_task(&txn, owner, schedule_id, expected_revision).await?;
        let result = Self::activate_on(&txn, owner, &task, false).await?;
        if result.kind == "conversation_resume" {
            record_decision_on(&HOST, &txn, &result, None).await?;
        }
        txn.commit().await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
