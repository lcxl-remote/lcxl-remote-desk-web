//! Single-node scheduled dispatch using durable paired claims.
mod fresh;
use crate::{
    device_assistant_gate::DeviceAssistantGate,
    device_assistant_orchestrator::{claim_scheduled_permission, resume_scheduled_turn},
    entity::{agent_schedule_run as run, agent_session},
    schedule_store::{
        ClaimedContinuation, ContinuationClaim, ContinuationLease, ScheduleStore,
        ScheduleStoreError,
    },
};
use actix_web::web;
use desk_diagnose_core::{agent_loop::LoopOutcome, session::PersistedAgentSession};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use futures_util::{StreamExt, stream};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::{sync::Arc, time::Duration};

const BATCH_SIZE: u64 = 32;
const LOCAL_CONCURRENCY: usize = 4;
const LEASE_SECONDS: u32 = 90;
const NODE: &str = "oss-scheduler";

#[derive(Default, Debug)]
pub struct ContinuationScanReport {
    pub scanned: usize,
    pub settled: usize,
    pub awaiting_permission: usize,
    pub deferred: usize,
    pub needs_reconciliation: usize,
    pub next_cursor: Option<i64>,
}

#[derive(Clone)]
pub struct SignalScheduleExecutor {
    db: DatabaseConnection,
    connections: web::Data<SharedConnectionMap>,
    gate: Arc<DeviceAssistantGate>,
}

enum DispatchResult {
    Settled,
    Waiting,
    Deferred,
    Reconcile,
}

impl SignalScheduleExecutor {
    pub fn new(
        db: DatabaseConnection,
        connections: web::Data<SharedConnectionMap>,
        gate: Arc<DeviceAssistantGate>,
    ) -> Self {
        Self {
            db,
            connections,
            gate,
        }
    }

    pub async fn scan_once(
        &self,
        after_id: i64,
    ) -> Result<ContinuationScanReport, ScheduleStoreError> {
        let store = ScheduleStore::new(self.db.clone());
        let mut candidates = store.continuation_candidates(after_id, BATCH_SIZE).await?;
        candidates.extend(store.fresh_task_candidates(after_id, BATCH_SIZE).await?);
        candidates.sort_by_key(|candidate| candidate.id);
        candidates.truncate(BATCH_SIZE as usize);
        let mut report = ContinuationScanReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == BATCH_SIZE as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        let mut tasks = stream::iter(
            candidates
                .into_iter()
                .map(|candidate| self.process(candidate)),
        )
        .buffer_unordered(LOCAL_CONCURRENCY);
        while let Some(result) = tasks.next().await {
            match result {
                DispatchResult::Settled => report.settled += 1,
                DispatchResult::Waiting => report.awaiting_permission += 1,
                DispatchResult::Deferred => report.deferred += 1,
                DispatchResult::Reconcile => report.needs_reconciliation += 1,
            }
        }
        Ok(report)
    }

    async fn claim_initial(&self, candidate: &run::Model) -> Option<(String, ClaimedContinuation)> {
        let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&candidate.conversation_id))
            .filter(agent_session::Column::ActorId.eq(owner.to_string()))
            .one(&self.db)
            .await
            .ok()??;
        let session = PersistedAgentSession::decode_json(&row.state_json).ok()?;
        let target = {
            let map = self.connections.read().await;
            let mut targets = map.values().filter(|target| {
                target.auth_context.auth_kind == AuthKind::TokenAuth
                    && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                    && target.model.version_info.client_id.as_deref()
                        == Some(session.device_id.as_str())
            });
            let first = targets
                .next()
                .map(|target| target.model.connection_id.clone());
            if targets.next().is_some() {
                return None;
            }
            first
        };
        let store = ScheduleStore::new(self.db.clone());
        let Some(target) = target else {
            let _ = store.wait_for_device(&candidate.run_id).await;
            return None;
        };
        crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&target, chrono::Utc::now())?;
        let config = crate::model_provider::load(&self.db).await.ok()?;
        crate::model_dial::SignalModelSeam::from_config(&config).ok()?;
        config.destination_identity().ok()?;
        if !self.gate.is_enabled() {
            return None;
        }
        // The persisted scope is only an upper bound. compose_turn independently
        // projects current readiness, original input and grants before any call.
        let mut scope = session.scope_snapshot;
        scope.mode = scope
            .mode
            .restrict_to(config.execution_mode)
            .restrict_to(desk_agent_protocol::ExecutionMode::ConfirmEachAction);
        let claimed = store
            .claim_conversation_resume(ContinuationClaim {
                owner,
                run_id: &candidate.run_id,
                node_id: NODE,
                lease_seconds: LEASE_SECONDS,
                policy_revision:
                    desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                scope,
            })
            .await
            .ok()?;
        Some((target, claimed))
    }

    async fn process(&self, candidate: run::Model) -> DispatchResult {
        if serde_json::from_str::<crate::entity::agent_schedule::Model>(
            &candidate.task_snapshot_json,
        )
        .is_ok_and(|task| task.kind == "fresh_task")
        {
            return self.process_fresh(candidate).await;
        }
        if !self.gate.is_enabled()
            || candidate.owner_user_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID
        {
            return DispatchResult::Deferred;
        }
        let claimed = if candidate.status == "awaiting_permission" {
            claim_scheduled_permission(
                self.connections.get_ref(),
                &self.db,
                &self.gate,
                &candidate.run_id,
                LEASE_SECONDS,
            )
            .await
            .ok()
            .map(|value| (value.target_connection_id, value.claimed))
        } else {
            self.claim_initial(&candidate).await
        };
        let Some((target, claimed)) = claimed else {
            return DispatchResult::Deferred;
        };
        let run_id = claimed.run.run_id.clone();
        let conversation = claimed.session.conversation_id.clone();
        let epoch = claimed.run.lease_epoch;
        let token = claimed.session.lease_token;
        let held = || ContinuationLease {
            owner: candidate.owner_user_id,
            run_id: &run_id,
            node_id: NODE,
            run_epoch: epoch,
            session_token: token,
        };
        let result = resume_scheduled_turn(
            self.connections.clone(),
            self.db.clone(),
            &self.gate,
            target,
            claimed,
            LEASE_SECONDS,
        )
        .await;
        let store = ScheduleStore::new(self.db.clone());
        let settled = match result {
            Ok(LoopOutcome::Answered(answer)) => {
                store.finish_answered_continuation(held(), &answer).await
            }
            Ok(LoopOutcome::PermissionRequested { request_id }) => {
                return if store
                    .await_continuation_permission(held(), &request_id)
                    .await
                    .is_ok()
                {
                    DispatchResult::Waiting
                } else {
                    DispatchResult::Reconcile
                };
            }
            // A transport error or other loop outcome alone is not completion.
            // The settlement transaction verifies the persisted error, exact turn,
            // both leases and absence of unresolved effects before accounting it.
            _ => {
                let Ok(Some(row)) = agent_session::Entity::find()
                    .filter(agent_session::Column::ConversationId.eq(&conversation))
                    .filter(agent_session::Column::ActorId.eq(candidate.owner_user_id.to_string()))
                    .one(&self.db)
                    .await
                else {
                    return DispatchResult::Reconcile;
                };
                let Ok(session) = PersistedAgentSession::decode_json(&row.state_json) else {
                    return DispatchResult::Reconcile;
                };
                let Some(error) = session.terminal_error else {
                    return DispatchResult::Reconcile;
                };
                store.finish_failed_continuation(held(), &error).await
            }
        };
        if settled.is_ok() {
            DispatchResult::Settled
        } else {
            DispatchResult::Reconcile
        }
    }

    pub async fn run(self) {
        let mut cursor = 0;
        let mut rehearsal_cursor = 0;
        loop {
            match self.scan_once(cursor).await {
                Ok(report) => {
                    cursor = report.next_cursor.unwrap_or(0);
                    if report.needs_reconciliation > 0 {
                        log::warn!(
                            "[schedule-executor] {} started continuations require reconciliation",
                            report.needs_reconciliation
                        );
                    }
                }
                Err(_) => {
                    cursor = 0;
                    log::warn!("[schedule-executor] candidate scan unavailable; retrying");
                }
            }
            match ScheduleStore::new(self.db.clone())
                .recover_terminal_rehearsals(rehearsal_cursor, BATCH_SIZE)
                .await
            {
                Ok(report) => rehearsal_cursor = report.next_cursor.unwrap_or(0),
                Err(_) => {
                    rehearsal_cursor = 0;
                    log::warn!("[schedule-executor] rehearsal recovery unavailable; retrying");
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
