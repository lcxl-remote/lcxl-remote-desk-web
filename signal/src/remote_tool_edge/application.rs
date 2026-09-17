//! Native identity preflight for an exact owner-approved application launch.

use super::*;
use desk_agent_protocol::application_launch::{LaunchApplicationRequest, LaunchPreflightReceipt};
use desk_agent_protocol::{Capability, OperationOutput, ReadContextOutput};

#[cfg(test)]
mod tests;

pub(crate) async fn resolve_candidate(
    connections: &SharedConnectionMap,
    target_connection_id: &str,
    actor_id: &str,
    device_id: &str,
    request: &LaunchApplicationRequest,
) -> Result<LaunchPreflightReceipt, AgentError> {
    let fail = || {
        error(
            AgentErrorKind::InvalidInput,
            "Invalid application resolution receipt",
            false,
            true,
        )
    };
    if request.validate().is_err() {
        return Err(fail());
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let expiry = chrono::Utc::now() + chrono::Duration::seconds(30);
    let envelope = AgentEnvelope {
        protocol_version: ProtocolVersion::default(),
        request_id: RequestId(request_id.clone()),
        parent_task_id: None,
        target: TargetRef {
            device_id: device_id.into(),
            session_id: None,
            worker_id: None,
        },
        actor: ActorRef {
            actor_type: ActorType::User,
            actor_id: actor_id.into(),
        },
        caller: CallerRef {
            caller_type: CallerType::Human,
            model_provider: None,
            model_name: None,
            adapter: Some("assistant-application-preflight".into()),
        },
        scope: AgentScope {
            granted: vec![Capability::ApplicationList],
            mode: ExecutionMode::ReadOnly,
            expires_at: Some(expiry.to_rfc3339()),
            policy_name: Some("assistant-application-preflight".into()),
        },
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::ApplicationLaunchResolve(request.clone()),
            }),
        },
        audit: AuditMeta {
            approval_id: None,
            reason: Some("Resolve application identity without starting a process".into()),
        },
    }
    .try_into()
    .map_err(|_| fail())?;
    let request = RemoteToolRequest {
        request_id: request_id.clone(),
        tool_call_id: request_id.clone(),
        envelope,
    };
    let target = connections
        .read()
        .await
        .get(target_connection_id)
        .cloned()
        .ok_or_else(|| {
            error(
                AgentErrorKind::TargetOffline,
                "Application target is offline",
                true,
                true,
            )
        })?;
    let pending = global_remote_tool_pending();
    let (tx, rx) = oneshot::channel();
    if !pending.register(request_id.clone(), target_connection_id.into(), tx) {
        return Err(fail());
    }
    let result = async {
        let frame =
            SignalingModel::new_request(SignalingType::InvokeRemoteTool, None, Some(&request))
                .map_err(|_| fail())?;
        let text = serde_json::to_string(&frame).map_err(|_| fail())?;
        target.session.write().await.text(text).await.map_err(|_| {
            error(
                AgentErrorKind::TransportError,
                "Application resolution could not be sent",
                true,
                true,
            )
        })?;
        let output = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| {
                error(
                    AgentErrorKind::TransportError,
                    "Application resolution timed out",
                    true,
                    true,
                )
            })?
            .map_err(|_| {
                error(
                    AgentErrorKind::TransportError,
                    "Application resolution disconnected",
                    true,
                    true,
                )
            })??;
        match output.outcome {
            AgentOutcome::Ok(OperationOutput::ReadContext(
                ReadContextOutput::ApplicationLaunchResolve(output),
            )) => Ok(output),
            AgentOutcome::Err(error) => Err(error),
            _ => Err(fail()),
        }
    }
    .await;
    pending.cancel(&request_id);
    result
}
