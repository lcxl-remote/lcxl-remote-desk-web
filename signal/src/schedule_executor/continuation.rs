//! Conversation timers and their permission resumes through the shared dispatch flow.
use super::*;
use desk_diagnose_core::schedule::dispatch::{
    ContinuationDispatchHost, DevicePresence, DispatchCandidate, LeaseIds, TurnRun,
};

/// One scan's view of the executor as a continuation dispatch host.
pub(super) struct ContinuationHost<'a>(pub(super) &'a SignalScheduleExecutor);

fn lease_ids(claimed: &ClaimedContinuation) -> LeaseIds {
    LeaseIds {
        run_epoch: claimed.run.lease_epoch,
        session_token: claimed.session.lease_token,
    }
}

fn held<'a>(candidate: &'a DispatchCandidate, lease: LeaseIds) -> ContinuationLease<'a> {
    ContinuationLease {
        owner: candidate.owner,
        run_id: &candidate.run_id,
        node_id: NODE,
        run_epoch: lease.run_epoch,
        session_token: lease.session_token,
    }
}

impl SignalScheduleExecutor {
    async fn source_session(
        &self,
        candidate: &DispatchCandidate,
    ) -> Option<Box<PersistedAgentSession>> {
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&candidate.conversation_id))
            .filter(agent_session::Column::ActorId.eq(candidate.owner.to_string()))
            .one(&self.db)
            .await
            .ok()??;
        PersistedAgentSession::decode_json(&row.state_json)
            .ok()
            .map(Box::new)
    }

    /// One live device connection routes the run; several cannot be decided.
    pub(super) async fn device_route(&self, device_id: &str) -> DevicePresence<String> {
        let map = self.connections.read().await;
        let mut targets = map.values().filter(|target| {
            target.auth_context.auth_kind == AuthKind::TokenAuth
                && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && target.client_id.as_deref() == Some(device_id)
        });
        let first = targets
            .next()
            .map(|target| target.model.connection_id.clone());
        match first {
            None => DevicePresence::Offline,
            Some(_) if targets.next().is_some() => DevicePresence::Unknown,
            Some(target) => DevicePresence::Online(target),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl ContinuationDispatchHost for ContinuationHost<'_> {
    type Target = String;
    type Session = Box<PersistedAgentSession>;
    type Claimed = (String, ClaimedContinuation);

    fn admits(&self, candidate: &DispatchCandidate) -> bool {
        self.0.gate.is_enabled()
            && candidate.owner == crate::control_authorizer::SINGLE_ACCOUNT_USER_ID
    }

    async fn load_session(&self, candidate: &DispatchCandidate) -> Option<Self::Session> {
        self.0.source_session(candidate).await
    }

    async fn device_presence(&self, device_id: &str) -> DevicePresence<String> {
        self.0.device_route(device_id).await
    }

    async fn wait_for_device(&self, run_id: &str) {
        let _ = ScheduleStore::new(self.0.db.clone())
            .wait_for_device(run_id)
            .await;
    }

    async fn claim_initial(
        &self,
        candidate: &DispatchCandidate,
        session: Self::Session,
        target: String,
    ) -> Option<(LeaseIds, Self::Claimed)> {
        crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&target, chrono::Utc::now())?;
        let config = crate::model_provider::load(&self.0.db).await.ok()?;
        crate::model_dial::SignalModelSeam::from_config(&config).ok()?;
        config.destination_identity().ok()?;
        if !self.0.gate.is_enabled() {
            return None;
        }
        // The persisted scope is only an upper bound. compose_turn independently
        // projects current readiness, original input and grants before any call.
        let mut scope = session.scope_snapshot;
        scope.mode = scope
            .mode
            .restrict_to(config.execution_mode)
            .restrict_to(desk_agent_protocol::ExecutionMode::ConfirmEachAction);
        let claimed = ScheduleStore::new(self.0.db.clone())
            .claim_conversation_resume(ContinuationClaim {
                owner: candidate.owner,
                run_id: &candidate.run_id,
                node_id: NODE,
                lease_seconds: LEASE_SECONDS,
                policy_revision:
                    desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                scope,
            })
            .await
            .inspect_err(|error| {
                log::debug!("[schedule-executor] continuation claim deferred: {error:?}");
            })
            .ok()?;
        Some((lease_ids(&claimed), (target, claimed)))
    }

    async fn claim_permission(
        &self,
        candidate: &DispatchCandidate,
    ) -> Option<(LeaseIds, Self::Claimed)> {
        let value = claim_scheduled_permission(
            self.0.connections.get_ref(),
            &self.0.db,
            &self.0.gate,
            &candidate.run_id,
            LEASE_SECONDS,
        )
        .await
        .ok()?;
        Some((
            lease_ids(&value.claimed),
            (value.target_connection_id, value.claimed),
        ))
    }

    async fn run_turn(&self, (target, claimed): Self::Claimed) -> TurnRun {
        // Poll the model continuation outside the scanner's dispatch stack.
        // The owned task preserves scan cancellation ownership.
        let connections = self.0.connections.clone();
        let db = self.0.db.clone();
        let gate = self.0.gate.clone();
        match owned_task::run(async move {
            Box::pin(resume_scheduled_turn(
                connections,
                db,
                &gate,
                target,
                claimed,
                LEASE_SECONDS,
            ))
            .await
        })
        .await
        {
            Ok(result) => TurnRun::Finished(result),
            // A lost task is not proof of completion or permission to replay.
            Err(_) => TurnRun::Unaccounted,
        }
    }

    async fn finish_answered(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        answer: &str,
    ) -> bool {
        ScheduleStore::new(self.0.db.clone())
            .finish_answered_continuation(held(candidate, lease), answer)
            .await
            .is_ok()
    }

    async fn await_permission(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        request_id: &str,
    ) -> bool {
        ScheduleStore::new(self.0.db.clone())
            .await_continuation_permission(held(candidate, lease), request_id)
            .await
            .is_ok()
    }

    async fn persisted_error(
        &self,
        candidate: &DispatchCandidate,
    ) -> Option<desk_agent_protocol::AgentError> {
        self.0.source_session(candidate).await?.terminal_error
    }

    async fn finish_failed(
        &self,
        candidate: &DispatchCandidate,
        lease: LeaseIds,
        error: &desk_agent_protocol::AgentError,
    ) -> bool {
        ScheduleStore::new(self.0.db.clone())
            .finish_failed_continuation(held(candidate, lease), error)
            .await
            .is_ok()
    }
}
