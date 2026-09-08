//! Dispatch isolated tasks with current local gates and Provider availability.
use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::{
    capability_availability::callable_tools, model_capability::ModelCapabilities,
    schedule::contract::parse_contract,
};

impl SignalScheduleExecutor {
    pub(super) async fn process_fresh(&self, candidate: run::Model) -> DispatchResult {
        match self.start_fresh(&candidate).await {
            Ok(result) => result,
            Err(_) => DispatchResult::Deferred,
        }
    }

    async fn start_fresh(
        &self,
        candidate: &run::Model,
    ) -> Result<DispatchResult, ScheduleStoreError> {
        let store = ScheduleStore::new(self.db.clone());
        if store
            .reject_unstarted_budget_policy(candidate.owner_user_id, &candidate.run_id)
            .await?
        {
            return Ok(DispatchResult::Settled);
        }
        let task = store
            .read(candidate.owner_user_id, &candidate.schedule_id)
            .await?;
        let target = {
            let peers = self.connections.read().await;
            let mut matching = peers.values().filter(|peer| {
                peer.auth_context.auth_kind == AuthKind::TokenAuth
                    && peer.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                    && peer.model.version_info.client_id.as_deref()
                        == Some(task.target_device_id.as_str())
            });
            let first = matching.next().map(|peer| peer.model.connection_id.clone());
            if matching.next().is_some() {
                return Err(ScheduleStoreError::Conflict);
            }
            first
        };
        let Some(target) = target else {
            if candidate.status != "awaiting_permission" {
                store.wait_for_device(&candidate.run_id).await?;
            }
            return Ok(DispatchResult::Deferred);
        };
        let config = crate::model_provider::load(&self.db)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        let (providers, inventory, _, readiness) =
            crate::device_assistant_orchestrator::current_capability_projection(
                &self.db,
                self.connections.get_ref(),
                &target,
                ModelCapabilities {
                    image_input: config.supports_image_input,
                },
            )
            .await;
        if readiness.is_none() || !self.gate.is_enabled() {
            return Err(ScheduleStoreError::Conflict);
        }
        let stored = store
            .read_contract(
                candidate.owner_user_id,
                &candidate.schedule_id,
                task.contract_revision.ok_or(ScheduleStoreError::Conflict)?,
            )
            .await?;
        let contract =
            parse_contract(&stored.canonical_json).map_err(|_| ScheduleStoreError::Invalid)?;
        let mut tools =
            callable_tools(&providers, &inventory).map_err(|_| ScheduleStoreError::Invalid)?;
        tools.retain(|tool| {
            crate::device_assistant_orchestrator::fresh::contains_tool(
                &contract,
                &providers,
                tool.name(),
            )
        });
        let mut granted = Vec::new();
        for tool in &tools {
            if !granted.contains(&tool.required_capability) {
                granted.push(tool.required_capability);
            }
        }
        let scope = AgentScope {
            granted,
            mode: config
                .execution_mode
                .restrict_to(ExecutionMode::ConfirmEachAction),
            expires_at: None,
            policy_name: Some("oss-device-assistant-provider".into()),
        };
        let claimed = store
            .claim_fresh_task(
                self.connections.get_ref(),
                &self.gate,
                crate::schedule_store::FreshTaskClaim {
                    owner: candidate.owner_user_id,
                    run_id: &candidate.run_id,
                    node_id: NODE,
                    lease_seconds: LEASE_SECONDS,
                    scope,
                    policy_revision:
                        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                },
            )
            .await?;
        let active = run::Entity::find_by_id(candidate.id)
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let token = claimed.session.lease_token;
        let result = crate::device_assistant_orchestrator::resume_fresh_task(
            self.connections.clone(),
            self.db.clone(),
            self.gate.clone(),
            claimed.target_connection_id,
            NODE,
            claimed.session,
            LEASE_SECONDS,
        )
        .await;
        let lease = || crate::schedule_store::FreshTaskLease {
            owner: candidate.owner_user_id,
            run_id: &candidate.run_id,
            node_id: NODE,
            run_epoch: active.lease_epoch,
            session_token: token,
        };
        let settled = match result {
            Ok(LoopOutcome::Answered(answer)) => {
                store.finish_answered_fresh_task(lease(), &answer).await
            }
            Ok(LoopOutcome::PermissionRequested { request_id }) => {
                return Ok(
                    if store
                        .await_fresh_task_permission(lease(), &request_id)
                        .await
                        .is_ok()
                    {
                        DispatchResult::Waiting
                    } else {
                        DispatchResult::Reconcile
                    },
                );
            }
            _ => {
                let row = agent_session::Entity::find()
                    .filter(agent_session::Column::ConversationId.eq(&candidate.run_id))
                    .one(&self.db)
                    .await?
                    .ok_or(ScheduleStoreError::NotFound)?;
                let session = PersistedAgentSession::decode_json(&row.state_json)
                    .map_err(|_| ScheduleStoreError::Invalid)?;
                let Some(error) = session.terminal_error.as_ref() else {
                    return Ok(DispatchResult::Reconcile);
                };
                store.finish_failed_fresh_task(lease(), error).await
            }
        };
        Ok(match settled {
            Ok(run) if run.status == "outcome_unknown" => DispatchResult::Reconcile,
            Ok(_) => DispatchResult::Settled,
            Err(_) => DispatchResult::Reconcile,
        })
    }
}
