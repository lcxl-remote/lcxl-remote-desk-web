//! Durable OSS permission resumes; connection hints never grant device authority.
use crate::agent_session_store::SignalAgentSessionStore;
use actix_web::web;
use desk_agent_protocol::{AgentError, device_assistant::DeviceAssistantAsk};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use futures_util::{StreamExt, stream};
use sea_orm::DatabaseConnection;
use std::time::Duration;

const BATCH_SIZE: u64 = 32;
const LOCAL_CONCURRENCY: usize = 4;

#[derive(Default, Debug)]
pub struct ResumeScanReport {
    pub scanned: usize,
    pub attempted: usize,
    pub deferred: usize,
    pub next_cursor: Option<i64>,
}

#[derive(Clone)]
pub struct SignalPermissionResumeExecutor {
    db: DatabaseConnection,
    connections: web::Data<SharedConnectionMap>,
}

impl SignalPermissionResumeExecutor {
    pub fn new(db: DatabaseConnection, connections: web::Data<SharedConnectionMap>) -> Self {
        Self { db, connections }
    }

    pub async fn scan_once(&self, after_id: i64) -> Result<ResumeScanReport, AgentError> {
        let candidates = SignalAgentSessionStore::new(self.db.clone())
            .permission_resume_candidates(after_id, BATCH_SIZE)
            .await?;
        let mut report = ResumeScanReport {
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
                Ok(true) => report.attempted += 1,
                Ok(false) | Err(_) => report.deferred += 1,
            }
        }
        Ok(report)
    }

    async fn process(
        &self,
        candidate: crate::entity::agent_permission_resume::Model,
    ) -> Result<bool, AgentError> {
        // OSS has one owner identity. No public or code-session principal can
        // inherit a stored personal permission through this background path.
        if candidate.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
            return Ok(false);
        }
        let now = chrono::Utc::now();
        let Some(session) = SignalAgentSessionStore::new(self.db.clone())
            .pending_permission_resume(&candidate, now)
            .await?
        else {
            return Ok(false);
        };
        let connection_id = {
            let map = self.connections.read().await;
            let mut targets = map.values().filter(|target| {
                target.auth_context.auth_kind == AuthKind::TokenAuth
                    && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                    && target.model.version_info.client_id.as_deref()
                        == Some(session.device_id.as_str())
            });
            let Some(target) = targets.next() else {
                return Ok(false);
            };
            if targets.next().is_some() {
                return Ok(false);
            }
            target.model.connection_id.clone()
        };
        if crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&connection_id, now)
            .is_none()
        {
            return Ok(false);
        }
        let Some(question) =
            desk_diagnose_core::permission_resume::latest_user_requirement(&session.conversation)
                .map(|message| message.text.clone())
        else {
            return Ok(false);
        };
        crate::device_assistant_orchestrator::resume_after_permission_decision(
            self.connections.clone(),
            self.db.clone(),
            candidate.permission_id.clone(),
            connection_id,
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            session.device_id,
            session.conversation_id,
            candidate.request_id.clone(),
            DeviceAssistantAsk {
                question,
                client_message_id: candidate.permission_id.clone(),
                conversation_id: session.client_conversation_id,
                ..Default::default()
            },
        )
        .await;
        // The durable claim remains authoritative even if preflight returned or
        // the process exits before this maintenance pass.
        SignalAgentSessionStore::new(self.db.clone())
            .pending_permission_resume(&candidate, chrono::Utc::now())
            .await?;
        Ok(true)
    }

    pub async fn run(self) {
        let mut cursor = 0;
        loop {
            match self.scan_once(cursor).await {
                Ok(report) => cursor = report.next_cursor.unwrap_or(0),
                Err(_) => {
                    cursor = 0;
                    log::warn!("[permission-resume] durable scan unavailable; retrying");
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
