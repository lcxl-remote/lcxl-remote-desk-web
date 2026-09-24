//! Single-node scheduled dispatch using durable paired claims.
mod fresh;
use crate::owned_task;
use crate::{
    ai_assistant_gate::AiAssistantGate,
    ai_assistant_orchestrator::{
        claim_scheduled_permission, resume_queued_goal, resume_scheduled_turn,
    },
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
use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

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
    pub goal_scanned: usize,
    pub goal_settled: usize,
    pub goal_deferred: usize,
    pub review_expired: u64,
}

#[derive(Clone)]
pub struct SignalScheduleExecutor {
    db: DatabaseConnection,
    connections: web::Data<SharedConnectionMap>,
    gate: Arc<AiAssistantGate>,
    approval_cursor: Arc<AtomicI64>,
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
        gate: Arc<AiAssistantGate>,
    ) -> Self {
        Self {
            db,
            connections,
            gate,
            approval_cursor: Arc::new(AtomicI64::new(0)),
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
        report.review_expired = crate::agent_approval_store::expire_review_leases(
            &self.db,
            u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            BATCH_SIZE,
        )
        .await?;
        let mut tasks = stream::iter(
            candidates
                .into_iter()
                // Keep the large dispatch future out of buffer_unordered's poll frame.
                .map(|candidate| Box::pin(self.process(candidate))),
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
        crate::agent_goal_open_store::expire_due(&self.db, chrono::Utc::now(), BATCH_SIZE as u64)
            .await?;
        crate::agent_goal_store::expire_due(&self.db, chrono::Utc::now(), BATCH_SIZE as u64)
            .await?;
        let mut goal_candidates = crate::agent_goal_store::queued_candidates(
            &self.db,
            u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            BATCH_SIZE,
        )
        .await?;
        goal_candidates.extend(
            crate::agent_goal_store::waiting_device_candidates(
                &self.db,
                u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
                BATCH_SIZE,
            )
            .await?,
        );
        goal_candidates.extend(
            crate::agent_goal_store::waiting_model_candidates(
                &self.db,
                u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
                BATCH_SIZE,
            )
            .await?,
        );
        goal_candidates.extend(
            crate::agent_goal_store::waiting_work_candidates(
                &self.db,
                u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
                BATCH_SIZE,
            )
            .await?,
        );
        report.goal_scanned = goal_candidates.len();
        let mut goals = stream::iter(
            goal_candidates
                .into_iter()
                .map(|goal| Box::pin(self.process_goal(goal))),
        )
        .buffer_unordered(LOCAL_CONCURRENCY);
        while let Some(settled) = goals.next().await {
            if settled {
                report.goal_settled += 1;
            } else {
                report.goal_deferred += 1;
            }
        }
        Ok(report)
    }

    async fn process_goal(&self, goal: desk_diagnose_core::goal::GoalRun) -> bool {
        use desk_diagnose_core::goal::{GoalState, GoalWaitReason};

        if !matches!(
            goal.state,
            GoalState::Queued
                | GoalState::Waiting(GoalWaitReason::Device)
                | GoalState::Waiting(GoalWaitReason::Model)
                | GoalState::Waiting(GoalWaitReason::Work)
        ) {
            return false;
        }
        if goal.state == GoalState::Waiting(GoalWaitReason::Work) {
            let _ = crate::agent_goal_store::wake_for_work_completion(
                &self.db,
                &goal,
                chrono::Utc::now(),
            )
            .await;
            return false;
        }
        if goal.state == GoalState::Waiting(GoalWaitReason::Model) {
            let _ =
                crate::agent_goal_store::wake_model_due(&self.db, &goal, chrono::Utc::now()).await;
            return false;
        }
        let target = {
            let map = self.connections.read().await;
            let mut targets = map.values().filter(|target| {
                target.auth_context.auth_kind == AuthKind::TokenAuth
                    && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                    && target.model.version_info.client_id.as_deref()
                        == Some(goal.device_id.as_str())
            });
            let first = targets
                .next()
                .map(|target| target.model.connection_id.clone());
            if targets.next().is_some() {
                None
            } else {
                first
            }
        };
        let available = target.as_deref().is_some_and(|connection_id| {
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(connection_id, chrono::Utc::now())
                .is_some()
        });
        if !available || goal.state == GoalState::Waiting(GoalWaitReason::Device) {
            if let Err(error) = crate::agent_goal_store::update_device_availability(
                &self.db,
                &goal,
                available,
                chrono::Utc::now(),
            )
            .await
            {
                log::warn!("[goal-executor] device wait transition failed: {error}");
            }
            return false;
        }
        match crate::agent_goal_store::pause_if_missing_attachments(
            &self.db,
            &goal,
            chrono::Utc::now(),
        )
        .await
        {
            Ok(Some(_)) => return false,
            Ok(None) => {}
            Err(error) => {
                log::warn!("[goal-executor] attachment preflight failed: {error}");
                return false;
            }
        }
        let connections = self.connections.clone();
        let db = self.db.clone();
        let gate = self.gate.clone();
        let observed_goal = goal.clone();
        match owned_task::run(async move { resume_queued_goal(connections, db, &gate, goal).await })
            .await
        {
            Ok(Ok(LoopOutcome::TurnBusy)) => {
                let _ = crate::agent_goal_store::pause_if_unresolved_work(
                    &self.db,
                    &observed_goal,
                    chrono::Utc::now(),
                )
                .await;
                false
            }
            Ok(Ok(_)) => true,
            Ok(Err(error)) => {
                log::warn!(
                    "[goal-executor] queued goal could not continue: {}",
                    error.message
                );
                if error.retryable
                    && error.kind == desk_agent_protocol::AgentErrorKind::ModelUnavailable
                {
                    let _ = crate::agent_goal_store::wait_for_model(
                        &self.db,
                        &observed_goal,
                        None,
                        chrono::Utc::now(),
                    )
                    .await;
                } else if error.kind == desk_agent_protocol::AgentErrorKind::ModelRejected {
                    let _ = crate::agent_goal_store::block_model_before_claim(
                        &self.db,
                        &observed_goal,
                        chrono::Utc::now(),
                    )
                    .await;
                }
                false
            }
            Err(_) => false,
        }
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
            return Box::pin(self.process_fresh(candidate)).await;
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
        // Poll the model continuation outside the scanner's dispatch stack.
        // The owned task preserves scan cancellation ownership.
        let connections = self.connections.clone();
        let db = self.db.clone();
        let gate = self.gate.clone();
        let result = owned_task::run(async move {
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
        .await;
        let Ok(result) = result else {
            // A lost task is not proof of completion or permission to replay.
            return DispatchResult::Reconcile;
        };
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
        actix_web::rt::spawn(self.clone().run_approval_reviews());
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

    async fn run_approval_reviews(self) {
        loop {
            if self.gate.is_enabled() {
                let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
                match crate::agent_approval_store::pending_permission_reviews(
                    &self.db,
                    self.approval_cursor.load(Ordering::Relaxed),
                    BATCH_SIZE,
                    now_unix_ms,
                )
                .await
                {
                    Ok((candidates, next_cursor)) => {
                        self.approval_cursor
                            .store(next_cursor.unwrap_or(0), Ordering::Relaxed);
                        let mut approvals = stream::iter(candidates.into_iter().map(|candidate| {
                            Box::pin(crate::approval_dispatch::process_pending_permission_review(
                                self.db.clone(),
                                self.connections.clone(),
                                candidate,
                            ))
                        }))
                        .buffer_unordered(LOCAL_CONCURRENCY);
                        while approvals.next().await.is_some() {}
                    }
                    Err(error) => {
                        self.approval_cursor.store(0, Ordering::Relaxed);
                        log::warn!("[approval-executor] candidate scan unavailable: {error}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
