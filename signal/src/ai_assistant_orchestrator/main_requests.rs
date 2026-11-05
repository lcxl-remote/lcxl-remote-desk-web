//! Resume ordinary owner input independently of the submitting browser.

use super::*;
use crate::{
    agent_run_event_store, agent_session_store::SignalAgentSessionStore, entity::agent_session,
};
use desk_agent_protocol::ai_assistant::MainRequestFailure;
use desk_assistant_data_store::{entity::agent_main_request, main_request::FrozenRequestInput};
use desk_diagnose_core::session::PersistedAgentSession;
use futures_util::{StreamExt, stream};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};
use std::{sync::Arc, time::Duration};

mod coordination;
pub(super) use coordination::fail_destination;

const BATCH_SIZE: u64 = 32;

pub struct SignalMainRequestExecutor {
    db: DatabaseConnection,
    connections: web::Data<SharedConnectionMap>,
    gate: Arc<crate::ai_assistant_gate::AiAssistantGate>,
}

impl SignalMainRequestExecutor {
    pub fn new(
        db: DatabaseConnection,
        connections: web::Data<SharedConnectionMap>,
        gate: Arc<crate::ai_assistant_gate::AiAssistantGate>,
    ) -> Self {
        Self {
            db,
            connections,
            gate,
        }
    }

    pub async fn run(self) {
        tokio::join!(self.run_requests(), self.run_replies(), self.run_sources());
    }

    async fn run_sources(&self) {
        let mut cursor = 0;
        let mut routing_cursor = 0;
        loop {
            let (scan, routing) = tokio::join!(
                crate::issue_event_intake::scan_once(&self.db, &self.gate, cursor),
                crate::issue_event_intake::scan_routing_once(&self.db, &self.gate, routing_cursor),
            );
            cursor = scan.unwrap_or_else(|_| {
                log::warn!("[issue-source] durable scan deferred");
                0
            });
            routing_cursor = routing.unwrap_or_else(|_| {
                log::warn!("[issue-source] durable routing deferred");
                0
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn run_replies(&self) {
        loop {
            let token = uuid::Uuid::new_v4().to_string();
            let delivery = if self.gate.is_enabled() {
                crate::issue_replies::deliver_next(&self.db, "local", &token).await
            } else {
                Ok(false)
            };
            if delivery.is_err() {
                log::warn!("[issue-reply] durable delivery deferred");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn run_requests(&self) {
        let mut cursor = 0;
        loop {
            cursor = match self.scan_once(cursor).await {
                Ok(cursor) => cursor,
                Err(error) => {
                    log::warn!("[main-request] durable scan unavailable: {}", error.message);
                    0
                }
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn scan_once(&self, cursor: i64) -> Result<i64, AgentError> {
        if !self.gate.is_enabled() {
            return Ok(0);
        }
        let rows = main_request::recovery_heads(&self.db, cursor, BATCH_SIZE)
            .await
            .map_err(|error| error.into_agent_error())?;
        let next = if rows.len() == BATCH_SIZE as usize {
            rows.last().unwrap().id
        } else {
            0
        };
        let mut work =
            stream::iter(rows.into_iter().map(|row| self.process(row))).buffer_unordered(4);
        while let Some(result) = work.next().await {
            if let Err(error) = result {
                log::debug!("[main-request] activation deferred: {:?}", error.kind);
            }
        }
        Ok(next)
    }

    async fn process(&self, row: agent_main_request::Model) -> Result<(), AgentError> {
        let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
        if row.actor_id != owner.to_string() {
            return Ok(());
        }
        let Some(execution_conversation_id) = self.ready(&row).await? else {
            return Ok(());
        };
        let frozen = FrozenRequestInput::decode(&row).map_err(|error| error.into_agent_error())?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let issue_authority = frozen
            .issue_queue_authority(&self.db, &row.actor_id, now_ms)
            .await
            .map_err(|error| error.into_agent_error())?;
        if issue_authority == main_request::IssueQueueAuthority::Paused {
            main_request::defer_paused_source(&self.db, &row, now_ms)
                .await
                .map_err(|error| error.into_agent_error())?;
            return Ok(());
        }
        let failure = if main_request::coordination::exhausted(&row) {
            Some(MainRequestFailure::CoordinationExhausted)
        } else if frozen.authorization_expired(now_ms)
            || issue_authority == main_request::IssueQueueAuthority::Unavailable
            || !self.targets_authorized(&row, &frozen).await?
            || !crate::issue_connections::permits_issues(
                &self.db,
                &row.actor_id,
                &frozen.reads.external_issues,
            )
            .await
            .map_err(|_| super::transport_error("External connection settings are unavailable"))?
        {
            Some(MainRequestFailure::SourceUnavailable)
        } else {
            None
        };
        if let Some(reason) = failure {
            if row.input_revision.is_none() {
                main_request::fail_unactivated(&self.db, &row, now_ms, reason)
                    .await
                    .map_err(|error| error.into_agent_error())?;
            } else {
                coordination::close_unclaimed(
                    &self.db,
                    &row,
                    &execution_conversation_id,
                    &frozen.device_id,
                    reason,
                )
                .await?;
            }
            return Ok(());
        }
        let target = if frozen.device_id.is_empty() {
            String::new()
        } else {
            let map = self.connections.read().await;
            let Some(target) = map.current_oss_server(&frozen.device_id).filter(|target| {
                target
                    .model
                    .ai_assistant_settings
                    .as_ref()
                    .is_some_and(|settings| settings.enabled)
            }) else {
                return Ok(());
            };
            target.model.connection_id.clone()
        };
        if row.input_revision.is_none()
            && !main_request::coordination::has_route(&row)
            && self.coordinate(&row, &frozen.device_id, owner).await?
        {
            return Ok(());
        }
        let request_id = row.request_id;
        let browser = {
            let connections = self.connections.read().await;
            row.reply_connection_id
                .filter(|id| {
                    connections
                        .get(id)
                        .is_some_and(|conn| conn.auth_context.is_browser_user(owner))
                })
                .unwrap_or_default()
        };
        let queue = Box::new(OwnerQueueTurn::Activate {
            request_id: request_id.clone(),
            conversation_id: row.conversation_id,
            execution_conversation_id,
            destination: frozen.destination,
            issue_source: frozen.issue_source,
        });
        compose_turn_with_queue(
            self.connections.clone(),
            self.db.clone(),
            request_id,
            browser,
            target,
            owner,
            frozen.device_id,
            frozen.ask,
            None,
            None,
            None,
            None,
            Some(queue),
        )
        .await?;
        Ok(())
    }

    async fn targets_authorized(
        &self,
        row: &agent_main_request::Model,
        input: &FrozenRequestInput,
    ) -> Result<bool, AgentError> {
        let references = if input.device_id.is_empty() {
            input
                .targets
                .iter()
                .map(|target| target.device_id.clone())
                .collect::<Vec<_>>()
        } else {
            vec![input.device_id.clone()]
        };
        if references.is_empty() {
            return Ok(true);
        }
        if agent_run_event_store::eligible_request_device_ids(&self.db, &references)
            .await?
            .len()
            != references.len()
        {
            return Ok(false);
        }
        input
            .roster_matches(&self.db, &row.actor_id, &row.conversation_id)
            .await
            .map_err(|error| error.into_agent_error())
    }

    async fn ready(&self, row: &agent_main_request::Model) -> Result<Option<String>, AgentError> {
        let snapshot = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&row.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&row.actor_id))
            .one(&self.db)
            .await
            .map_err(|_| transport_error("queued conversation unavailable"))?;
        let Some(mut snapshot) = snapshot else {
            return Ok(row
                .input_revision
                .is_none()
                .then(|| row.conversation_id.clone()));
        };
        let mut session = PersistedAgentSession::decode_boxed_json(&snapshot.state_json)
            .map_err(|_| transport_error("queued conversation state unavailable"))?;
        let now = chrono::Utc::now();
        let execution = main_request::workbench_execution(&session, row)
            .map_err(|error| error.into_agent_error())?;
        if let Some(execution) = &execution {
            if session.main_stopped || session.has_live_main_coordination(now.timestamp_millis()) {
                return Ok(None);
            }
            if let Some(active) =
                agent_run_event_store::main_requests::active_work_on(&self.db, &session).await?
            {
                if active.lease_deadline.is_none_or(|deadline| deadline < now) {
                    SignalAgentSessionStore::new(self.db.clone())
                        .settle_lapsed_session(&active, now)
                        .await?;
                }
                return Ok(None);
            }
            if row.input_revision.is_none() {
                return Ok(Some(execution.clone()));
            }
            snapshot = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(execution))
                .filter(agent_session::Column::ActorId.eq(&row.actor_id))
                .one(&self.db)
                .await
                .map_err(|_| transport_error("queued conversation unavailable"))?
                .ok_or_else(|| transport_error("queued conversation state unavailable"))?;
            session = PersistedAgentSession::decode_boxed_json(&snapshot.state_json)
                .map_err(|_| transport_error("queued conversation state unavailable"))?;
            if session.agent_role.workbench_id() != Some(row.conversation_id.as_str())
                || session.agent_role.workbench_request_id() != Some(row.request_id.as_str())
            {
                return Err(transport_error("queued conversation state unavailable"));
            }
        }
        let execution = execution.unwrap_or_else(|| row.conversation_id.clone());
        if row
            .input_revision
            .is_some_and(|revision| revision < session.input_revision as i64)
        {
            main_request::retire_superseded(&self.db, row, now.timestamp_millis())
                .await
                .map_err(|error| error.into_agent_error())?;
            return Ok(None);
        }
        if session.main_stopped || session.has_live_main_coordination(now.timestamp_millis()) {
            return Ok(None);
        }
        if session.turn_state.is_active() {
            if snapshot
                .lease_deadline
                .is_none_or(|deadline| deadline < now)
            {
                SignalAgentSessionStore::new(self.db.clone())
                    .settle_lapsed_session(&snapshot, now)
                    .await?;
            }
            return Ok(None);
        }
        if let Some(revision) = row.input_revision {
            return Ok((revision == session.input_revision as i64).then_some(execution));
        }
        let txn = self
            .db
            .begin()
            .await
            .map_err(|_| transport_error("queued conversation unavailable"))?;
        Ok(
            (!agent_run_event_store::main_requests::slot_held(&txn, &session).await?)
                .then_some(execution),
        )
    }
}
