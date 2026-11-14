//! Dispatch isolated tasks with current local gates and Provider availability.
use super::*;
use crate::schedule_store::{ClaimedFreshTask, FreshTaskLease};
use desk_agent_protocol::{AgentError, AgentScope, ExecutionMode};
use desk_diagnose_core::{
    capability_availability::callable_tools,
    model_capability::ModelCapabilities,
    schedule::dispatch::{
        DevicePresence, FreshDispatchHost, FreshSettled, LeaseIds, PersistedError, TurnRun,
        dispatch_fresh,
    },
};

/// One scan's view of the executor as a fresh-task dispatch host.
struct FreshHost<'a>(&'a SignalScheduleExecutor);

fn lease(candidate: &DispatchCandidate, ids: LeaseIds) -> FreshTaskLease<'_> {
    FreshTaskLease {
        owner: candidate.owner,
        run_id: &candidate.run_id,
        node_id: NODE,
        run_epoch: ids.run_epoch,
        session_token: ids.session_token,
    }
}

fn settled(result: Result<run::Model, ScheduleStoreError>) -> Option<FreshSettled> {
    match result.ok()?.status.as_str() {
        "awaiting_children" => Some(FreshSettled::AwaitingChildren),
        "outcome_unknown" => Some(FreshSettled::OutcomeUnknown),
        _ => Some(FreshSettled::Other),
    }
}

impl SignalScheduleExecutor {
    /// The fresh dispatch phase, type-erased at its original call point.
    pub fn process_fresh<'a>(
        &'a self,
        candidate: &'a DispatchCandidate,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DispatchResult> + 'a>> {
        desk_diagnose_core::future::phase(|| async move {
            let host = FreshHost(self);
            desk_diagnose_core::future::phase(|| dispatch_fresh(&host, candidate))
                .await
                .result
        })
    }

    async fn claim_fresh(
        &self,
        candidate: &DispatchCandidate,
        target: String,
    ) -> Result<(LeaseIds, ClaimedFreshTask), ScheduleStoreError> {
        let store = ScheduleStore::new(self.db.clone());
        let config = crate::model_provider::load(&self.db)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        let (providers, inventory, _, readiness) =
            crate::ai_assistant_orchestrator::current_capability_projection(
                &self.db,
                self.connections.get_ref(),
                Some(&target),
                ModelCapabilities {
                    image_input: config.supports_image_input,
                },
            )
            .await;
        if readiness.is_none() || !self.gate.is_enabled() {
            return Err(ScheduleStoreError::Conflict);
        }
        // A scheduled run offers the same non-interactive tools as a session;
        // every device action is approved on its own.
        let mut tools =
            callable_tools(&providers, &inventory).map_err(|_| ScheduleStoreError::Invalid)?;
        tools.retain(|tool| {
            !desk_diagnose_core::ai_assistant::is_interactive_only_tool(tool.name())
        });
        let mut granted = Vec::new();
        for capability in tools
            .iter()
            .filter_map(|tool| tool.required_device_capability)
        {
            if !granted.contains(&capability) {
                granted.push(capability);
            }
        }
        let scope = AgentScope {
            granted,
            mode: config
                .execution_mode
                .restrict_to(ExecutionMode::ConfirmEachAction),
            expires_at: None,
            policy_name: Some("oss-ai-assistant-provider".into()),
        };
        let claimed = desk_diagnose_core::future::boxed(|| {
            store.claim_fresh_task(
                self.connections.get_ref(),
                &self.gate,
                crate::schedule_store::FreshTaskClaim {
                    owner: candidate.owner,
                    run_id: &candidate.run_id,
                    node_id: NODE,
                    lease_seconds: LEASE_SECONDS,
                    scope,
                    policy_revision:
                        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                },
            )
        })
        .await?;
        let active = run::Entity::find_by_id(candidate.id)
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let ids = LeaseIds {
            run_epoch: active.lease_epoch,
            session_token: claimed.session.lease_token,
        };
        Ok((ids, claimed))
    }

    async fn fresh_persisted_error(
        &self,
        candidate: &DispatchCandidate,
    ) -> Result<Option<AgentError>, ScheduleStoreError> {
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&candidate.run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        Ok(session.terminal_error)
    }
}

#[async_trait::async_trait(?Send)]
impl FreshDispatchHost for FreshHost<'_> {
    type Target = String;
    type Claimed = ClaimedFreshTask;

    async fn device_presence(&self, device_id: &str) -> DevicePresence<String> {
        self.0.device_route(device_id).await
    }

    async fn wait_for_device(&self, run_id: &str) {
        let _ = ScheduleStore::new(self.0.db.clone())
            .wait_for_device(run_id)
            .await;
    }

    async fn claim(
        &self,
        candidate: &DispatchCandidate,
        target: String,
    ) -> Option<(LeaseIds, ClaimedFreshTask)> {
        self.0.claim_fresh(candidate, target).await.ok()
    }

    async fn run_turn(&self, claimed: ClaimedFreshTask) -> TurnRun {
        match owned_task::run(crate::ai_assistant_orchestrator::resume_fresh_task(
            self.0.connections.clone(),
            self.0.db.clone(),
            self.0.gate.clone(),
            claimed.target_connection_id,
            NODE,
            claimed.session,
            LEASE_SECONDS,
        ))
        .await
        {
            Ok(result) => TurnRun::Finished(result),
            Err(_) => TurnRun::Unaccounted,
        }
    }

    async fn finish_answered(
        &self,
        candidate: &DispatchCandidate,
        ids: LeaseIds,
        answer: &str,
    ) -> Option<FreshSettled> {
        settled(
            ScheduleStore::new(self.0.db.clone())
                .finish_answered_fresh_task(lease(candidate, ids), answer)
                .await,
        )
    }

    async fn await_children(
        &self,
        candidate: &DispatchCandidate,
        ids: LeaseIds,
        wait_id: &str,
    ) -> bool {
        ScheduleStore::new(self.0.db.clone())
            .await_fresh_task_children(lease(candidate, ids), wait_id)
            .await
            .is_ok()
    }

    async fn await_permission(
        &self,
        candidate: &DispatchCandidate,
        ids: LeaseIds,
        request_id: &str,
    ) -> bool {
        ScheduleStore::new(self.0.db.clone())
            .await_fresh_task_permission(lease(candidate, ids), request_id)
            .await
            .is_ok()
    }

    async fn persisted_error(&self, candidate: &DispatchCandidate) -> PersistedError {
        match self.0.fresh_persisted_error(candidate).await {
            Ok(Some(error)) => PersistedError::Failed(error),
            Ok(None) => PersistedError::Clean,
            Err(_) => PersistedError::Unreadable,
        }
    }

    async fn finish_failed(
        &self,
        candidate: &DispatchCandidate,
        ids: LeaseIds,
        error: &AgentError,
    ) -> Option<FreshSettled> {
        settled(
            ScheduleStore::new(self.0.db.clone())
                .finish_failed_fresh_task(lease(candidate, ids), error)
                .await,
        )
    }
}
