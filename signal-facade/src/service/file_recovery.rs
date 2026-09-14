//! Node-local private response channel. Manager routes HTTP to the socket-owning node.
use crate::{
    model::{
        auth_context::AuthKind,
        connection::{ConnectionState, SharedConnectionMap},
        signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
    },
    service::FileRecoveryObserver,
};
use desk_agent_protocol::{
    AgentScope, ExecutionMode, RiskLevel,
    authz::{
        AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthorizedControlPayload, AuthzActor,
        AuthzDevice, ExecAdmissionPolicy,
    },
    file_recovery::*,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy)]
pub enum RecoveryRequestError {
    Offline,
    Timeout,
    InvalidRequest,
    Capacity,
    InvalidReply,
}
struct Pending {
    connection: String,
    sender: oneshot::Sender<FileRecoveryReply>,
}
#[derive(Default)]
pub struct FileRecoveryHub {
    pending: Mutex<HashMap<String, Pending>>,
}
pub fn global_hub() -> Arc<FileRecoveryHub> {
    static HUB: OnceLock<Arc<FileRecoveryHub>> = OnceLock::new();
    HUB.get_or_init(|| Arc::new(FileRecoveryHub::default()))
        .clone()
}
struct Registration {
    hub: Arc<FileRecoveryHub>,
    id: String,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.pending.lock().unwrap().remove(&self.id);
    }
}
impl FileRecoveryHub {
    fn register(
        self: &Arc<Self>,
        connection: &str,
    ) -> Result<(Registration, oneshot::Receiver<FileRecoveryReply>), RecoveryRequestError> {
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= 256 {
            return Err(RecoveryRequestError::Capacity);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        pending.insert(
            id.clone(),
            Pending {
                connection: connection.into(),
                sender,
            },
        );
        Ok((
            Registration {
                hub: self.clone(),
                id,
            },
            receiver,
        ))
    }
    fn complete(&self, connection: &str, id: &str, reply: FileRecoveryReply) {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(id).is_none_or(|p| p.connection != connection) {
            return;
        }
        if let Some(p) = pending.remove(id) {
            let _ = p.sender.send(reply);
        }
    }
}
impl FileRecoveryObserver for FileRecoveryHub {
    fn on_file_recovery_reply<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if source.auth_context.auth_kind != AuthKind::TokenAuth
                || source.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
                || source.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Server
                || model.signaling_type != SignalingType::FileRecoveryManaged
                || model.to_connection_id.is_some()
                || model
                    .response_state
                    .as_ref()
                    .is_some_and(|s| s.error_code != 0)
            {
                return;
            }
            let Ok(reply) = model.get_data::<FileRecoveryReply>() else {
                return;
            };
            if reply.authority.len() != 64 || reply.os_user.len() > 128 {
                return;
            }
            match &reply.outcome {
                FileRecoveryOutcome::Export { zip_base64 } if zip_base64.len() > 900_000 => return,
                FileRecoveryOutcome::Page { page } if page.records.len() > 100 => return,
                FileRecoveryOutcome::Unavailable { reason } => {
                    log::warn!("File recovery device request unavailable: {:?}", reason);
                }
                _ => (),
            }
            self.complete(&source.model.connection_id, &model.request_id, reply);
        })
    }
}

/// Caller has already checked current device ownership and (for exports) that
/// the conversation is not deleted. No model or browser can provide `owner`.
pub async fn request_authorized(
    connections: &SharedConnectionMap,
    connection_id: &str,
    owner: i32,
    device_id: Option<i32>,
    request: FileRecoveryRequest,
) -> Result<FileRecoveryReply, RecoveryRequestError> {
    if owner <= 0
        || request.command.validate().is_err()
        || request
            .expected_os_user
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        || request
            .expected_authority
            .as_deref()
            .is_some_and(|s| s.len() != 64)
    {
        return Err(RecoveryRequestError::InvalidRequest);
    }
    let target = connections
        .read()
        .await
        .get(connection_id)
        .cloned()
        .ok_or(RecoveryRequestError::Offline)?;
    if target.auth_context.auth_kind != AuthKind::TokenAuth
        || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        || device_id.is_some_and(|id| target.auth_context.bound_device_id != Some(id))
    {
        return Err(RecoveryRequestError::Offline);
    }
    let audience = target
        .model
        .version_info
        .client_id
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or(RecoveryRequestError::Offline)?;
    let (registration, receiver) = global_hub().register(connection_id)?;
    let expires = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
    let expected_authority = request.expected_authority.clone();
    let expected_os_user = request.expected_os_user.clone();
    let wrapper = AuthorizedControlPayload {
        inner: request,
        authz: AuthorizationBlock {
            file_recovery_registration: None,
            version: AUTHORIZATION_BLOCK_VERSION,
            scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: Some("owner-file-recovery".into()),
            },
            orchestrator_grants: vec![MANAGEMENT_GRANT.into()],
            max_risk: RiskLevel::Low,
            exec_admission_policy: ExecAdmissionPolicy::TemplateOnly,
            actor: AuthzActor {
                user_id: Some(owner),
            },
            device: AuthzDevice { device_id },
            request_id: registration.id.clone(),
            session_id: None,
            expires_at: Some(expires),
            issuer: "owner-file-recovery".into(),
            audience: audience.into(),
            signature: None,
        },
    };
    let frame = SignalingModel::new(
        &registration.id,
        SignalingType::ManageFileRecovery,
        None,
        None,
        Some(serde_json::to_value(wrapper).map_err(|_| RecoveryRequestError::InvalidRequest)?),
        None,
    );
    let body = serde_json::to_string(&frame).map_err(|_| RecoveryRequestError::InvalidRequest)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        target.session.write().await.text(body).await
    })
    .await
    .map_err(|_| RecoveryRequestError::Timeout)?
    .map_err(|_| RecoveryRequestError::Offline)?;
    let reply = tokio::time::timeout(Duration::from_secs(25), receiver)
        .await
        .map_err(|_| RecoveryRequestError::Timeout)?
        .map_err(|_| RecoveryRequestError::InvalidReply)?;
    if !connections.read().await.contains_key(connection_id) {
        return Err(RecoveryRequestError::Offline);
    }
    if (expected_authority.is_some_and(|id| id != reply.authority)
        || expected_os_user.is_some_and(|id| id != reply.os_user))
        && !matches!(
            reply.outcome,
            FileRecoveryOutcome::Unavailable {
                reason: FileRecoveryFailure::IdentityChanged
            }
        )
    {
        return Err(RecoveryRequestError::InvalidReply);
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn private_reply_requires_exact_connection_and_cancellation_removes_waiter() {
        let hub = Arc::new(FileRecoveryHub::default());
        let (ticket, mut reply) = hub.register("device-connection").unwrap();
        let response = FileRecoveryReply {
            authority: "a".repeat(64),
            os_user: "501".into(),
            outcome: FileRecoveryOutcome::Deleted { complete: true },
        };
        hub.complete("wrong-connection", &ticket.id, response.clone());
        assert!(matches!(
            reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        hub.complete("device-connection", &ticket.id, response);
        assert!(reply.await.is_ok());
        assert!(hub.pending.lock().unwrap().is_empty());
        let (ticket, reply) = hub.register("device-connection").unwrap();
        drop(ticket);
        assert!(reply.await.is_err());
        assert!(hub.pending.lock().unwrap().is_empty());
    }
}
