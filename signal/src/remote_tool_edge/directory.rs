//! Owner bootstrap resolution, without a file-content read or a tool grant.

use super::*;
use desk_agent_protocol::computer_use::{FileDirectoryResolveOutput, FileDirectoryResolveParams};
use desk_agent_protocol::{Capability, OperationOutput, ReadContextOutput};

pub(crate) async fn resolve_candidate(
    connections: &SharedConnectionMap,
    target_connection_id: &str,
    actor_id: &str,
    device_id: &str,
    path: &str,
) -> Result<FileDirectoryResolveOutput, AgentError> {
    let fail = || {
        error(
            AgentErrorKind::InvalidInput,
            "Invalid directory resolution receipt",
            false,
            true,
        )
    };
    if path.trim().is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
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
            adapter: Some("assistant-directory-selection".into()),
        },
        scope: AgentScope {
            granted: vec![Capability::FileMetadataRead],
            mode: ExecutionMode::ReadOnly,
            expires_at: Some(expiry.to_rfc3339()),
            policy_name: Some("assistant-directory-selection".into()),
        },
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::FileDirectoryResolve(FileDirectoryResolveParams {
                    path: path.into(),
                }),
            }),
        },
        audit: AuditMeta {
            approval_id: None,
            reason: Some("Resolve owner directory candidate without listing contents".into()),
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
                "Directory target is offline",
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
                "Directory resolution could not be sent",
                true,
                true,
            )
        })?;
        let output = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| {
                error(
                    AgentErrorKind::TransportError,
                    "Directory resolution timed out",
                    true,
                    true,
                )
            })?
            .map_err(|_| {
                error(
                    AgentErrorKind::TransportError,
                    "Directory resolution disconnected",
                    true,
                    true,
                )
            })??;
        match output.outcome {
            AgentOutcome::Ok(OperationOutput::ReadContext(
                ReadContextOutput::FileDirectoryResolve(output),
            )) => Ok(output),
            AgentOutcome::Err(error) => Err(error),
            _ => Err(fail()),
        }
    }
    .await;
    pending.cancel(&request_id);
    result
}
