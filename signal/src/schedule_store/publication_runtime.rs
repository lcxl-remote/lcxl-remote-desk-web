//! Single-owner publication against live device readiness and transaction-owned evidence.
use super::{
    ScheduleStore, ScheduleStoreError, TaskPublicationVerifier, TaskRehearsalEvidence, entity,
};
use crate::{
    control_authorizer::SINGLE_ACCOUNT_USER_ID, device_assistant_gate::DeviceAssistantGate,
};
use desk_agent_protocol::{capability_provider::ProductSurface, schedule::contract::TaskBudget};
use desk_diagnose_core::{
    capability_availability::callable_tools, model_capability::ModelCapabilities,
    schedule::contract::ValidatedTaskContract,
};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use sea_orm::DatabaseTransaction;

pub struct SignalTaskPublicationVerifier<'a> {
    pub connections: &'a SharedConnectionMap,
    pub gate: &'a DeviceAssistantGate,
    pub maximum_budget: &'a TaskBudget,
}

impl SignalTaskPublicationVerifier<'_> {
    async fn target(&self, task: &entity::Model) -> Result<String, ScheduleStoreError> {
        if task.owner_user_id != SINGLE_ACCOUNT_USER_ID
            || !self.gate.is_enabled()
            || task.model_id.is_some()
        {
            return Err(ScheduleStoreError::NotFound);
        }
        let map = self.connections.read().await;
        let mut matches = map.values().filter(|peer| {
            peer.auth_context.auth_kind == AuthKind::TokenAuth
                && peer.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && peer.model.version_info.client_id.as_deref()
                    == Some(task.target_device_id.as_str())
        });
        let peer = matches.next().ok_or(ScheduleStoreError::NotFound)?;
        if matches.next().is_some() {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(peer.model.connection_id.clone())
    }
}

#[async_trait::async_trait]
impl TaskPublicationVerifier for SignalTaskPublicationVerifier<'_> {
    async fn lock_subject(
        &self,
        _: &DatabaseTransaction,
        task: &entity::Model,
    ) -> Result<(), ScheduleStoreError> {
        self.target(task).await.map(|_| ())
    }

    async fn verify(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
        contract: &ValidatedTaskContract,
        rehearsal_run_id: &str,
        _: Option<i64>,
    ) -> Result<TaskRehearsalEvidence, ScheduleStoreError> {
        let policy = crate::schedule_budget_policy::read(txn).await?;
        if !desk_diagnose_core::schedule::policy::permits(&policy, &contract.contract().budget) {
            return Err(ScheduleStoreError::BudgetExceeded);
        }
        let gate = self.gate.snapshot();
        let target = self.target(task).await?;
        let requested = &contract.contract().budget;
        let maximum = self.maximum_budget;
        if requested.max_runs_per_utc_day > maximum.max_runs_per_utc_day
            || requested.max_calls_per_run > maximum.max_calls_per_run
            || requested.max_model_tokens_per_run > maximum.max_model_tokens_per_run
            || requested.max_runtime_seconds > maximum.max_runtime_seconds
        {
            return Err(ScheduleStoreError::BudgetExceeded);
        }
        let config = crate::model_provider::load(txn).await?;
        if !config.is_configured() {
            return Err(ScheduleStoreError::Conflict);
        }
        let destination = config
            .destination_identity()
            .map_err(|_| ScheduleStoreError::Conflict)?;
        let (registry, inventory, _, readiness) =
            crate::device_assistant_orchestrator::current_capability_projection(
                txn,
                self.connections,
                &target,
                ModelCapabilities {
                    image_input: config.supports_image_input,
                },
            )
            .await;
        let readiness = readiness.ok_or(ScheduleStoreError::Conflict)?;
        let tools =
            callable_tools(&registry, &inventory).map_err(|_| ScheduleStoreError::Conflict)?;
        let mut allowed = Vec::new();
        for rule in &contract.contract().permissions {
            if !tools.iter().any(|tool| tool.name() == rule.tool_name) {
                return Err(ScheduleStoreError::Conflict);
            }
            allowed.push(
                registry
                    .capability_for_tool(&rule.tool_name)
                    .ok_or(ScheduleStoreError::Conflict)?
                    .required_capability,
            );
        }
        contract
            .validate_current_catalog(&registry, ProductSurface::OssPersonalOwner, &allowed)
            .map_err(|_| ScheduleStoreError::Conflict)?;
        let mut evidence = ScheduleStore::publication_contract_scope_evidence_on(
            txn,
            task,
            contract,
            rehearsal_run_id,
        )
        .await?;
        // Evidence may wait for durable action fences. Recheck process-owned policy
        // and the exact connection/readiness generation before granting publication.
        let current = crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&target, chrono::Utc::now())
            .ok_or(ScheduleStoreError::Conflict)?;
        if self.gate.snapshot() != gate
            || self.target(task).await? != target
            || current.readiness != readiness
        {
            return Err(ScheduleStoreError::Conflict);
        }
        evidence.evidence_sha256 = super::digest(&super::json(&(
            &evidence.evidence_sha256,
            destination,
            maximum,
            gate.revision,
            readiness.revision,
        ))?);
        Ok(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule_store::publication_test_fixture;
    use desk_agent_protocol::device_assistant::DeviceAssistantSettings;
    use sea_orm::Database;

    #[tokio::test]
    async fn publication_without_enabled_assistant_and_live_target_creates_no_authority() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let (store, task, _, input) = publication_test_fixture(db).await;
        let connections = SharedConnectionMap::new();
        let gate = DeviceAssistantGate::default();
        for enabled in [false, true] {
            gate.replace(DeviceAssistantSettings {
                enabled,
                revision: 1,
            });
            let verifier = SignalTaskPublicationVerifier {
                connections: &connections,
                gate: &gate,
                maximum_budget: &desk_diagnose_core::schedule::TASK_PUBLICATION_BUDGET,
            };
            assert!(matches!(
                store.publish_task(1, &input, &verifier).await,
                Err(ScheduleStoreError::NotFound)
            ));
            let unchanged = store.read(1, &task.schedule_id).await.unwrap();
            assert_eq!(unchanged.revision, task.revision);
            assert_eq!(unchanged.status, "draft");
            assert!(unchanged.authorization_revision.is_none());
            assert!(unchanged.next_run_at.is_none());
        }
    }
}
