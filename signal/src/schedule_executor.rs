//! Single-node scheduled dispatch using durable paired claims.
use crate::config::connection::DatabaseConnection;
mod continuation;
mod fresh;
mod subagents;
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
use desk_diagnose_core::{
    agent_loop::LoopOutcome,
    schedule::dispatch::{DispatchCandidate, DispatchResult},
    session::PersistedAgentSession,
};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use futures_util::{StreamExt, stream};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
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
    pub review_usage_reconciled: u64,
}

#[derive(Clone)]
pub struct SignalScheduleExecutor {
    db: DatabaseConnection,
    connections: web::Data<SharedConnectionMap>,
    gate: Arc<AiAssistantGate>,
    approval_cursor: Arc<AtomicI64>,
    /// Instance-local position in the runnable goal FIFO. Only rotates which
    /// candidates are read next; claims are still decided under the write lock.
    goal_cursor: Arc<std::sync::Mutex<Option<crate::agent_goal_store::ContinuationCursor>>>,
    /// Paces subagent candidates that keep failing; a hint, not an authorization.
    subagent_backoff: Arc<desk_diagnose_core::retry_backoff::RetryBackoff>,
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
            goal_cursor: Arc::new(std::sync::Mutex::new(None)),
            subagent_backoff: Arc::default(),
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
        report.review_usage_reconciled = crate::agent_approval_usage::reconcile_usage(
            &self.db,
            chrono::Utc::now().timestamp_millis(),
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
        crate::agent_goal_open_store::expire_due(&self.db, chrono::Utc::now(), BATCH_SIZE).await?;
        crate::agent_goal_store::expire_due(&self.db, chrono::Utc::now(), BATCH_SIZE).await?;
        let after = self
            .goal_cursor
            .lock()
            .map(|cursor| cursor.clone())
            .unwrap_or_default();
        let mut goal_candidates = crate::agent_goal_store::continuation_candidates(
            &self.db,
            u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            BATCH_SIZE,
            after.as_ref(),
        )
        .await?;
        let next = (goal_candidates.len() as u64 == BATCH_SIZE)
            .then(|| goal_candidates.last())
            .flatten()
            .map(|goal| {
                (
                    i64::try_from(goal.updated_at_unix_ms).unwrap_or(i64::MAX),
                    goal.goal_id.clone(),
                )
            });
        if let Ok(mut cursor) = self.goal_cursor.lock() {
            *cursor = next;
        }
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
                    && target.client_id.as_deref() == Some(goal.execution_target.device_id.as_str())
                    && target
                        .model
                        .ai_assistant_settings
                        .as_ref()
                        .is_some_and(|settings| settings.enabled)
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

    async fn process(&self, candidate: run::Model) -> DispatchResult {
        let Ok(task) = serde_json::from_str::<crate::entity::agent_schedule::Model>(
            &candidate.task_snapshot_json,
        ) else {
            return DispatchResult::Deferred;
        };
        let candidate = DispatchCandidate {
            id: candidate.id,
            owner: candidate.owner_user_id,
            schedule_id: candidate.schedule_id,
            run_id: candidate.run_id,
            conversation_id: candidate.conversation_id,
            status: candidate.status,
            device_id: task.target_device_id,
        };
        if task.kind == "fresh_task" {
            return desk_diagnose_core::future::boxed(|| self.process_fresh(&candidate)).await;
        }
        let report = desk_diagnose_core::schedule::dispatch::dispatch_continuation(
            &continuation::ContinuationHost(self),
            &candidate,
        )
        .await;
        if let Some(error) = &report.stopped {
            log::warn!(
                "[schedule-executor] continuation {} stopped before settlement: {}",
                candidate.run_id,
                error.message
            );
        }
        report.result
    }

    pub async fn run(self) {
        actix_web::rt::spawn(self.clone().run_subagent_tasks());
        actix_web::rt::spawn(self.clone().run_subagent_waits());
        actix_web::rt::spawn(self.clone().run_subagent_recovery());
        actix_web::rt::spawn(self.clone().run_approval_reviews());
        let mut cursor = 0;
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
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn run_approval_reviews(self) {
        let mut default_cursor = 0;
        loop {
            if self.gate.is_enabled() {
                let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
                match crate::agent_approval_store::discover_default_review_roots(
                    &self.db,
                    default_cursor,
                    BATCH_SIZE,
                    now_unix_ms,
                )
                .await
                {
                    Ok((unattended, next)) => {
                        default_cursor = next.unwrap_or(0);
                        let mut tasks = stream::iter(unattended.into_iter().map(|subject| {
                            Box::pin(
                                crate::approval_dispatch::process_pending_unattended_command(
                                    self.db.clone(),
                                    self.connections.clone(),
                                    subject,
                                ),
                            )
                        }))
                        .buffer_unordered(LOCAL_CONCURRENCY);
                        while tasks.next().await.is_some() {}
                    }
                    Err(error) => {
                        default_cursor = 0;
                        log::warn!(
                            "[approval-executor] default approval discovery unavailable: {error}"
                        );
                    }
                }
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
