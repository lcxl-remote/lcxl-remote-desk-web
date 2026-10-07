//! Child planning and root result delivery release independent session leases.
use super::*;

impl SignalScheduleExecutor {
    pub(super) async fn run_subagent_tasks(self) {
        let store = crate::agent_subagent_store::SubAgentStore::new(self.db.clone());
        let mut cursor = 0;
        let mut expiry_cursor = 0;
        let mut source_cleanup_cursor = 0;
        loop {
            match store
                .withdrawn_scheduled_source_candidates(source_cleanup_cursor, BATCH_SIZE)
                .await
            {
                Ok(rows) => {
                    source_cleanup_cursor = if rows.len() == BATCH_SIZE as usize {
                        rows.last().map_or(0, |row| row.id)
                    } else {
                        0
                    };
                    for row in rows {
                        if let Err(error) = store
                            .close_scheduled_source(
                                &row.root_conversation_id,
                                &row.actor_id,
                                &row.device_id,
                            )
                            .await
                        {
                            log::warn!(
                                "[subagent-executor] withdrawn source {} cleanup unavailable: {}",
                                row.group_id,
                                error
                            );
                        }
                    }
                }
                Err(error) => {
                    source_cleanup_cursor = 0;
                    log::warn!("[subagent-executor] withdrawn source scan unavailable: {error}");
                }
            }
            match store
                .expired_task_candidates(expiry_cursor, BATCH_SIZE)
                .await
            {
                Ok(rows) => {
                    expiry_cursor = if rows.len() == BATCH_SIZE as usize {
                        rows.last().map_or(0, |row| row.id)
                    } else {
                        0
                    };
                    for row in rows {
                        if let Err(error) = store.expire_task(&row.task_id).await {
                            log::warn!(
                                "[subagent-executor] task {} expiry unavailable: {}",
                                row.task_id,
                                error
                            );
                        }
                    }
                }
                Err(error) => {
                    expiry_cursor = 0;
                    log::warn!("[subagent-executor] expiry scan unavailable: {error}");
                }
            }
            if self.gate.is_enabled() {
                match store.queued_task_candidates(cursor, BATCH_SIZE).await {
                    Ok(candidates) => {
                        cursor = if candidates.len() == BATCH_SIZE as usize {
                            candidates.last().map_or(0, |row| row.id)
                        } else {
                            0
                        };
                        let mut tasks = stream::iter(candidates.into_iter().map(|row| {
                            let store = &store;
                            let executor = &self;
                            async move {
                                if row.state == "waiting_resource" {
                                    let available =
                                        executor.delegated_device_available(&row.device_id).await;
                                    if !store
                                        .retry_resource_task(&row.task_id, available)
                                        .await
                                        .unwrap_or(false)
                                    {
                                        return;
                                    }
                                }
                                match store.child_runtime_candidate(&row.task_id).await {
                                    Ok(Some(runtime)) => executor.drive_subagent(runtime).await,
                                    Ok(None) => {}
                                    Err(error) => log::warn!(
                                        "[subagent-executor] task {} unavailable: {}",
                                        row.task_id,
                                        error
                                    ),
                                }
                            }
                        }))
                        .buffer_unordered(LOCAL_CONCURRENCY);
                        while tasks.next().await.is_some() {}
                    }
                    Err(error) => {
                        cursor = 0;
                        log::warn!("[subagent-executor] candidate scan unavailable: {error}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Receipt reconciliation does not depend on the model/automation gate.
    pub(super) async fn run_subagent_recovery(self) {
        let store = crate::agent_subagent_store::SubAgentStore::new(self.db.clone());
        let mut cursor = 0;
        loop {
            match store.recovery_candidates(cursor, BATCH_SIZE).await {
                Ok(rows) => {
                    cursor = if rows.len() == BATCH_SIZE as usize {
                        rows.last().map_or(0, |row| row.id)
                    } else {
                        0
                    };
                    for row in rows {
                        if let Err(error) = store.reconcile_task(&row.task_id).await {
                            log::warn!(
                                "[subagent-executor] task {} reconciliation unavailable: {}",
                                row.task_id,
                                error
                            );
                        }
                    }
                }
                Err(error) => {
                    cursor = 0;
                    log::warn!("[subagent-executor] recovery scan unavailable: {error}");
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    pub(super) async fn run_subagent_waits(self) {
        let store = crate::agent_subagent_store::SubAgentStore::new(self.db.clone());
        let mut cursor = 0;
        loop {
            if self.gate.is_enabled() {
                match store.root_wait_candidates(cursor, BATCH_SIZE).await {
                    Ok(candidates) => {
                        cursor = if candidates.len() == BATCH_SIZE as usize {
                            candidates.last().map_or(0, |row| row.id)
                        } else {
                            0
                        };
                        let mut tasks = stream::iter(candidates.into_iter().map(|row| {
                            let store = &store;
                            let executor = &self;
                            async move {
                                let subject = (&row.root_conversation_id, &row.actor_id, &row.device_id);
                                match store.resolve_parent_wait(subject.0, subject.1, subject.2).await {
                                    Ok(crate::agent_subagent_store::ParentWaitResolution::Ready(_) | crate::agent_subagent_store::ParentWaitResolution::NotWaiting) => {
                                        match store.parent_runtime_candidate(subject.0, subject.1, subject.2).await {
                                            Ok(Some(runtime)) => executor.drive_subagent(runtime).await,
                                            Ok(None) => {}
                                            Err(error) => log::warn!("[subagent-executor] root {} unavailable: {}", row.root_conversation_id, error),
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(error) => log::warn!("[subagent-executor] root wait {} unavailable: {}", row.root_conversation_id, error),
                                }
                            }
                        })).buffer_unordered(LOCAL_CONCURRENCY);
                        while tasks.next().await.is_some() {}
                    }
                    Err(error) => {
                        cursor = 0;
                        log::warn!("[subagent-executor] root scan unavailable: {error}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn delegated_device_available(&self, device: &str) -> bool {
        let connections = self.connections.read().await;
        let mut targets = connections.values().filter(|target| {
            target.auth_context.auth_kind == AuthKind::TokenAuth
                && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && target.model.version_info.client_id.as_deref() == Some(device)
        });
        targets.next().is_some() && targets.next().is_none()
    }

    async fn drive_subagent(&self, runtime: desk_diagnose_core::subagent::runtime::RuntimeTurn) {
        let conversation_id = runtime.session().conversation_id.clone();
        let store = crate::agent_subagent_store::SubAgentStore::new(self.db.clone());
        if matches!(
            &runtime,
            desk_diagnose_core::subagent::runtime::RuntimeTurn::Child { .. }
        ) && !self
            .delegated_device_available(&runtime.session().device_id)
            .await
        {
            let _ = store
                .defer_child_candidate(
                    &runtime,
                    Some(desk_diagnose_core::subagent::SubAgentWaitReason::DeviceUnavailable),
                    None,
                )
                .await;
            return;
        }
        let candidate = runtime.clone();
        let connections = self.connections.clone();
        let db = self.db.clone();
        let gate = self.gate.clone();
        match owned_task::run(async move {
            crate::ai_assistant_orchestrator::resume_subagent_turn(connections, db, &gate, runtime)
                .await
        })
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                if error.kind == desk_agent_protocol::AgentErrorKind::ModelRejected {
                    let _ = store
                        .defer_child_candidate(&candidate, None, Some("delegated_model_rejected"))
                        .await;
                } else if error.kind == desk_agent_protocol::AgentErrorKind::ModelUnavailable
                    && error.retryable
                {
                    let _ = store
                        .defer_child_candidate(
                            &candidate,
                            Some(desk_diagnose_core::subagent::SubAgentWaitReason::ModelCapacity),
                            None,
                        )
                        .await;
                }
                log::warn!(
                    "[subagent-executor] turn {} deferred: {}",
                    conversation_id,
                    error.message
                );
            }
            Err(error) => log::warn!(
                "[subagent-executor] turn {} interrupted: {}",
                conversation_id,
                error
            ),
        }
    }
}
