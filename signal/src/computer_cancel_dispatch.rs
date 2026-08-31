//! Single-instance delivery of durable stops to the original authenticated host.

use crate::capability_grant_store::{
    SignalCapabilityGrantStore,
    computer_cancel::{CancelCandidate, wire_request_id},
};
use chrono::Utc;
use desk_agent_protocol::{
    AgentScope, ExecutionMode, RiskLevel,
    authz::{
        AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthorizedControlPayload, AuthzActor,
        AuthzDevice, ExecAdmissionPolicy,
    },
    computer_use::ComputerActionCancel,
};
use desk_signal_facade::model::{
    auth_context::AuthKind,
    connection::SharedConnectionMap,
    signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
};
use sea_orm::{DatabaseConnection, DbErr};
use std::{sync::Arc, time::Duration};

pub struct SignalComputerCancelDispatcher {
    store: SignalCapabilityGrantStore,
    connections: Arc<SharedConnectionMap>,
}

fn invalid() -> DbErr {
    DbErr::Custom("original stop delivery unavailable".into())
}

impl SignalComputerCancelDispatcher {
    pub fn new(db: DatabaseConnection, connections: Arc<SharedConnectionMap>) -> Self {
        Self {
            store: SignalCapabilityGrantStore::new(db),
            connections,
        }
    }

    pub async fn scan_once(&self, after: i64) -> Result<(Option<i64>, usize), DbErr> {
        let (ids, next) = self.store.computer_cancel_page(after).await?;
        let mut sent = 0;
        for id in ids {
            match self.send_original(id).await {
                Ok(true) => sent += 1,
                Ok(false) => {}
                Err(_) => log::warn!(
                    "[computer-action] stop delivery unavailable; original outcome unchanged"
                ),
            }
        }
        Ok((next, sent))
    }

    pub(crate) async fn send_original(&self, id: i64) -> Result<bool, DbErr> {
        let Some(candidate) = self.store.computer_cancel_candidate(id).await? else {
            return Ok(false);
        };
        let target = self
            .connections
            .read()
            .await
            .get(&candidate.connection_id)
            .cloned();
        let Some(target) = target else {
            return Ok(false);
        };
        if target.model.connection_id != candidate.connection_id
            || target.model.version_info.client_id.as_deref() != Some(&candidate.audience)
            || target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Err(invalid());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut socket = target.session.write().await;
            // Re-read after waiting for the socket: a completion or an ACK may
            // already have settled this stop. Never look up a replacement host.
            if self.store.computer_cancel_candidate(id).await?.as_ref() != Some(&candidate) {
                return Ok(false);
            }
            let frame = stop_frame(&candidate)?;
            socket
                .text(serde_json::to_string(&frame).map_err(|_| invalid())?)
                .await
                .map_err(|_| invalid())?;
            // Successful socket write is NOT a stop acknowledgment.
            Ok(true)
        })
        .await
        .map_err(|_| invalid())?
    }

    pub async fn run(self) {
        let mut cursor = 0;
        loop {
            match self.scan_once(cursor).await {
                Ok((next, _)) => cursor = next.unwrap_or(0),
                Err(_) => {
                    cursor = 0;
                    log::warn!("[computer-action] stop scan unavailable");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

fn stop_frame(candidate: &CancelCandidate) -> Result<SignalingModel, DbErr> {
    let actor = candidate.actor_id.parse::<i32>().map_err(|_| invalid())?;
    if actor <= 0 || actor.to_string() != candidate.actor_id {
        return Err(invalid());
    }
    let request = wire_request_id(candidate.work_id, &candidate.execution_generation);
    let wrapper = AuthorizedControlPayload {
        authz: AuthorizationBlock {
            version: AUTHORIZATION_BLOCK_VERSION,
            exec_admission_policy: ExecAdmissionPolicy::OwnerInteractive,
            scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: Some("oss-provider-stop".into()),
            },
            orchestrator_grants: vec![],
            max_risk: RiskLevel::Low,
            actor: AuthzActor {
                user_id: Some(actor),
            },
            device: AuthzDevice { device_id: None },
            request_id: request.clone(),
            session_id: None,
            expires_at: Some((Utc::now() + chrono::Duration::seconds(10)).to_rfc3339()),
            issuer: "signal".into(),
            audience: candidate.audience.clone(),
            signature: None,
        },
        inner: ComputerActionCancel {
            work_id: candidate.work_id.to_string(),
            action_request_id: candidate.action_request_id.clone(),
            execution_generation: candidate.execution_generation.clone(),
            reason: "stop requested by the original owner".into(),
        },
    };
    Ok(SignalingModel::new(
        &request,
        SignalingType::CancelComputerAction,
        None,
        None,
        Some(serde_json::to_value(wrapper).map_err(|_| invalid())?),
        None,
    ))
}
