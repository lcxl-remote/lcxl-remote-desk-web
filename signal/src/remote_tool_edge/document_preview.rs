//! Owner UI paging for an existing device-private document preview.

use super::*;
use desk_agent_protocol::document_conversion::{
    DOCUMENT_PREVIEW_SPEC_VERSION, DocumentPreviewPageFrame, DocumentPreviewPageParams,
};
use desk_agent_protocol::{Capability, OperationOutput, ReadContextOutput};

pub(crate) async fn render_page(
    connections: &SharedConnectionMap,
    target_connection_id: &str,
    actor_id: &str,
    device_id: &str,
    conversation_id: &str,
    preview_id: &str,
    page: u32,
) -> Result<DocumentPreviewPageFrame, AgentError> {
    let fail = |message: &'static str| error(AgentErrorKind::InvalidInput, message, false, true);
    if conversation_id.trim().is_empty()
        || conversation_id.len() > 256
        || preview_id.trim().is_empty()
        || preview_id.len() > 256
        || page == 0
    {
        return Err(fail("Invalid document preview page request"));
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
            adapter: Some("assistant-document-preview-page".into()),
        },
        scope: AgentScope {
            granted: vec![Capability::DocumentPreview],
            mode: ExecutionMode::ReadOnly,
            expires_at: Some(expiry.to_rfc3339()),
            policy_name: Some("assistant-document-preview-page".into()),
        },
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::DocumentPreviewPage(DocumentPreviewPageParams {
                    conversation_id: conversation_id.into(),
                    preview_id: preview_id.into(),
                    page,
                    spec_version: DOCUMENT_PREVIEW_SPEC_VERSION,
                }),
            }),
        },
        audit: AuditMeta {
            approval_id: None,
            reason: Some("Render one owner-visible page from an existing preview".into()),
        },
    }
    .try_into()
    .map_err(|_| fail("Invalid document preview page request"))?;
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
                "Document preview target is offline",
                true,
                true,
            )
        })?;
    let pending = global_remote_tool_pending();
    let (tx, rx) = oneshot::channel();
    if !pending.register(request_id.clone(), target_connection_id.into(), tx) {
        return Err(fail("Duplicate document preview page request"));
    }
    let result = async {
        let frame =
            SignalingModel::new_request(SignalingType::InvokeRemoteTool, None, Some(&request))
                .map_err(|_| fail("Could not build document preview page request"))?;
        let text = serde_json::to_string(&frame)
            .map_err(|_| fail("Could not encode document preview page request"))?;
        target.session.write().await.text(text).await.map_err(|_| {
            error(
                AgentErrorKind::TransportError,
                "Document preview page request could not be sent",
                true,
                true,
            )
        })?;
        let output = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| {
                error(
                    AgentErrorKind::Timeout,
                    "Document preview page request timed out",
                    true,
                    true,
                )
            })?
            .map_err(|_| {
                error(
                    AgentErrorKind::TargetOffline,
                    "Document preview page request disconnected",
                    true,
                    true,
                )
            })??;
        match output.outcome {
            AgentOutcome::Ok(OperationOutput::ReadContext(
                ReadContextOutput::DocumentPreviewPage(_),
            )) => output
                .document_preview_page
                .ok_or_else(|| fail("Document preview page result did not contain an image")),
            AgentOutcome::Err(error) => Err(error),
            _ => Err(fail("Invalid document preview page result")),
        }
    }
    .await;
    pending.cancel(&request_id);
    result
}
