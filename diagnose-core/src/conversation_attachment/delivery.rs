//! Authorize original bytes, persist typed bodies, then publish bounded references.
use super::{batch::*, source::*, *};
use crate::{
    chat::ChatMessage,
    seam::{ModelRequest, SessionSeam, ToolOutputFormat},
    session::PersistedAgentSession,
};
use desk_agent_protocol::{
    AgentError, OperationOutput, ReadContextOutput,
    data_lineage::{ContentRef, DataEnvelope},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSlot {
    pub path: String,
    pub attachment_id: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawResult {
    pub original_sha256: String,
    pub original_envelope: Option<DataEnvelope>,
    pub restorable: bool,
    /// None means the entire original text/JSON is in one attachment.
    pub template: Option<Value>,
    pub slots: Vec<RawSlot>,
    pub sha256: String,
    pub envelope: Option<DataEnvelope>,
}

/// Correlate a durable completion with its original receipt after externalization.
/// This is only an identity check; action authorization still needs live sources.
pub fn matches_original(
    message: &ChatMessage,
    text: &str,
    envelope: Option<&DataEnvelope>,
) -> bool {
    if let Some(raw) = &message.raw_result {
        raw.original_sha256 == digest(text.as_bytes()) && raw.original_envelope.as_ref() == envelope
    } else {
        message.text == text && message.data_envelope.as_ref() == envelope
    }
}

pub fn failed_delivery(
    mut original: ChatMessage,
    error: &AgentError,
) -> Result<ChatMessage, AgentError> {
    let proof = RawResult {
        original_sha256: digest(original.text.as_bytes()),
        original_envelope: original.data_envelope.clone(),
        restorable: false,
        template: None,
        slots: vec![],
        sha256: digest(original.text.as_bytes()),
        envelope: None,
    };
    let content = crate::model_input::describe_error("result_delivery", &error.message);
    original.data_envelope = projection_envelope(
        original.data_envelope.as_ref(),
        content.as_bytes(),
        "delivery-error",
    )?;
    original.text = content;
    original.raw_result = Some(Box::new(proof));
    original.resolved_result = None;
    Ok(original)
}

/// Derive bytes without broadening authority. The caller must first verify and
/// authorize the original result, including its complete byte charge.
pub fn projection_envelope(
    parent: Option<&DataEnvelope>,
    bytes: &[u8],
    purpose: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    parent
        .validate()
        .map_err(|_| invalid("Invalid original result label"))?;
    let hash = digest(bytes);
    let mut envelope = parent.clone();
    envelope.envelope_id = format!(
        "attachment-{purpose}-{}",
        &digest(format!("{}:{hash}", parent.envelope_id).as_bytes())[..32]
    );
    envelope.content = ContentRef::ImmutableBlob {
        blob_id: envelope.envelope_id.clone(),
        sha256: hash.clone(),
        size_bytes: bytes.len() as u64,
        media_type: "application/octet-stream".into(),
    };
    envelope.digest_sha256 = hash;
    envelope.provenance.source_envelope_ids = vec![parent.envelope_id.clone()];
    envelope
        .validate()
        .map_err(|_| invalid("Invalid attachment projection label"))?;
    Ok(Some(envelope))
}

struct Prepared {
    delivery: PreparedDelivery,
    template: Option<Value>,
    paths: Vec<String>,
    command: Option<(i32, u32, bool)>,
}

fn prepare(
    identity: &DeliveryIdentity<'_>,
    text: &str,
    format: ToolOutputFormat,
    now: u64,
) -> Result<Prepared, AgentError> {
    let mut template = None;
    let mut paths = vec![];
    let mut parts = vec![];
    let mut command = None;
    let mut field =
        |value: &Value, path: String, name: String, truncated: bool| -> Result<(), AgentError> {
            let body = value
                .pointer(&path)
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Typed result body is missing"))?;
            parts.push(OutputPart {
                name,
                content: PartContent::Text(body.into()),
                source_truncated: truncated,
            });
            paths.push(path);
            Ok(())
        };
    match format {
        ToolOutputFormat::Text => parts.push(OutputPart {
            name: "result".into(),
            content: PartContent::Text(text.into()),
            source_truncated: false,
        }),
        ToolOutputFormat::Json => parts.push(OutputPart {
            name: "result".into(),
            content: PartContent::Json(text.into()),
            source_truncated: false,
        }),
        ToolOutputFormat::WebDocument => {
            let value: Value =
                serde_json::from_str(text).map_err(|_| invalid("Invalid web document result"))?;
            field(&value, "/excerpt".into(), "body".into(), false)?;
            template = Some(value);
        }
        ToolOutputFormat::Operation => {
            let output = crate::ui_model_output::deserialize(text)
                .map_err(|_| invalid("Invalid typed operation result"))?;
            match source_format(&output) {
                SourceFormat::Json => {
                    prepare_text(ContentKind::Json, text.into())?;
                    parts.push(OutputPart {
                        name: "result".into(),
                        content: PartContent::Json(text.into()),
                        source_truncated: serde_json::to_value(&output)
                            .ok()
                            .and_then(|value| {
                                value
                                    .get("ReadContext")?
                                    .as_object()?
                                    .values()
                                    .next()?
                                    .get("truncated")?
                                    .as_bool()
                            })
                            .unwrap_or(false),
                    });
                }
                SourceFormat::Image => {
                    return Err(invalid("Image output must use the reviewed image path"));
                }
                SourceFormat::FileText => {
                    let value = serde_json::to_value(&output)
                        .map_err(|_| invalid("Cannot encode file result"))?;
                    field(
                        &value,
                        "/ReadContext/FileContentRead/content_utf8".into(),
                        "body".into(),
                        false,
                    )?;
                    template = Some(value);
                }
                SourceFormat::TerminalText => {
                    let OperationOutput::ReadContext(ReadContextOutput::TerminalOutputInspect(
                        ref terminal,
                    )) = output
                    else {
                        unreachable!()
                    };
                    let value = serde_json::to_value(&output)
                        .map_err(|_| invalid("Cannot encode terminal result"))?;
                    for (index, entry) in terminal.entries.iter().enumerate() {
                        field(
                            &value,
                            format!("/ReadContext/TerminalOutputInspect/entries/{index}/content"),
                            format!("terminal_{index}"),
                            entry.truncated,
                        )?;
                    }
                    template = Some(value);
                }
                SourceFormat::Command => {
                    let OperationOutput::Exec(ref exec) = output else {
                        unreachable!()
                    };
                    command = Some((
                        exec.exit_code,
                        exec.duration_ms,
                        !exec.redactions.is_empty(),
                    ));
                    let value = serde_json::to_value(&output)
                        .map_err(|_| invalid("Cannot encode command result"))?;
                    use desk_agent_protocol::ExecOutputStreams;
                    match &exec.streams {
                        ExecOutputStreams::Split {
                            stdout_truncated,
                            stderr_truncated,
                            ..
                        } => {
                            field(
                                &value,
                                "/Exec/streams/stdout".into(),
                                "stdout".into(),
                                *stdout_truncated,
                            )?;
                            field(
                                &value,
                                "/Exec/streams/stderr".into(),
                                "stderr".into(),
                                *stderr_truncated,
                            )?;
                        }
                        ExecOutputStreams::PtyCombined { truncated, .. } => field(
                            &value,
                            "/Exec/streams/terminal".into(),
                            "terminal".into(),
                            *truncated,
                        )?,
                    }
                    template = Some(value);
                }
            }
        }
    }
    Ok(Prepared {
        delivery: prepare_delivery(identity, parts, now)?,
        template,
        paths,
        command,
    })
}

pub async fn externalize(
    store: &dyn SessionSeam,
    session: &PersistedAgentSession,
    message: ChatMessage,
    format: ToolOutputFormat,
    policy: Option<&crate::model_egress::ModelEgressPolicy>,
    now: Result<u64, AgentError>,
) -> Result<ChatMessage, AgentError> {
    externalize_with(store, session, message, format, policy, now, &[]).await
}

#[allow(clippy::too_many_arguments)]
pub async fn externalize_with(
    store: &dyn SessionSeam,
    session: &PersistedAgentSession,
    mut message: ChatMessage,
    format: ToolOutputFormat,
    policy: Option<&crate::model_egress::ModelEgressPolicy>,
    now: Result<u64, AgentError>,
    extra: &[PreparedAttachment],
) -> Result<ChatMessage, AgentError> {
    let identity = DeliveryIdentity {
        conversation_id: &session.conversation_id,
        actor_id: &session.actor_id,
        device_id: &session.device_id,
        message_id: &message.message_id,
        tool_call_id: message
            .tool_call_id
            .as_deref()
            .ok_or_else(|| invalid("Tool result has no call identity"))?,
    };
    if matches!(format, ToolOutputFormat::Json | ToolOutputFormat::Operation) {
        let mut projected = message.clone();
        crate::ui_model_ids::project_tool_message(&mut projected);
        if projected.text != message.text {
            prepare_text(ContentKind::Json, projected.text)?;
        }
    }
    let prepared = prepare(&identity, &message.text, format, 0)?;
    if prepared.delivery.attachments.is_empty() {
        if !extra.is_empty() {
            store.store_attachment_batch(session, extra).await?;
        }
        return Ok(message);
    }
    // The stored reference is never used as a substitute for original-byte authorization.
    let source = if let Some(policy) = policy {
        let mut history = session.conversation.clone();
        history.push(message.clone());
        policy
            .authorize_request_with_history(
                ModelRequest::text_only(
                    vec![message.clone()],
                    crate::prompt::ResponseFormatSpec::None,
                ),
                &history,
            )
            .map_err(|error| error.agent_error())?
            .request
            .messages
            .remove(0)
    } else {
        message.clone()
    };
    let now = now?;
    let mut delivery = prepared.delivery;
    for part in &mut delivery.attachments {
        part.metadata.created_at_unix_ms = now;
        part.metadata.last_accessed_at_unix_ms = now;
        part.metadata.source_envelope = projection_envelope(
            source.data_envelope.as_ref(),
            &part.content,
            &part.metadata.part,
        )?;
    }
    let canonical = prepared
        .template
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_else(|| source.text.clone());
    let mut raw = RawResult {
        original_sha256: digest(message.text.as_bytes()),
        original_envelope: message.data_envelope.clone(),
        restorable: !delivery
            .attachments
            .iter()
            .any(|part| part.metadata.storage_truncated),
        template: prepared.template.clone(),
        slots: vec![],
        sha256: digest(canonical.as_bytes()),
        envelope: if prepared.template.is_some() {
            projection_envelope(source.data_envelope.as_ref(), canonical.as_bytes(), "raw")?
        } else {
            source.data_envelope.clone()
        },
    };
    for (index, part) in delivery.parts.iter().enumerate() {
        if let DeliveredContent::Attachment { reference } = &part.content {
            let stored = delivery
                .attachments
                .iter()
                .find(|a| a.metadata.attachment_id == reference.attachment_id)
                .unwrap();
            let path = prepared.paths.get(index).cloned().unwrap_or_default();
            if let Some(template) = &mut raw.template {
                *template
                    .pointer_mut(&path)
                    .ok_or_else(|| invalid("Invalid raw result slot"))? = Value::Null;
            }
            raw.slots.push(RawSlot {
                path,
                attachment_id: reference.attachment_id.clone(),
                sha256: stored.metadata.sha256.clone(),
            });
        }
    }
    if raw
        .template
        .as_ref()
        .is_some_and(|value| value.to_string().len() > MAX_JSON_BYTES)
    {
        return Err(invalid(
            "Tool result metadata exceeds 32768 bytes; narrow the source query. The operation must not be repeated.",
        ));
    }
    let content = if let Some((exit_code, duration_ms, redactions_applied)) = prepared.command {
        json!({"exit_code":exit_code,"duration_ms":duration_ms,"redactions_applied":redactions_applied,"streams":delivery.parts,
            "read_with": READ_ATTACHMENT_TOOL}).to_string()
    } else {
        let mut value = json!({"result_externalized":true,"parts":delivery.parts,"read_with":READ_ATTACHMENT_TOOL,
            "notice":"Read the attachment before interpreting its content or taking dependent actions. Do not repeat an already executed action."});
        if let Some(tool) = source
            .data_envelope
            .as_ref()
            .map(|e| &e.provenance.source_tool_name)
        {
            value["tool"] = json!(tool);
        }
        // Only carry a bounded status declared by the source, never infer success.
        if let Ok(original) = serde_json::from_str::<Value>(&source.text) {
            if let Some(status) = original
                .get("result")
                .filter(|status| status.to_string().len() <= 256)
            {
                value["result"] = status.clone();
            }
        }
        value.to_string()
    };
    let mut batch = delivery.attachments.clone();
    batch.extend_from_slice(extra);
    store.store_attachment_batch(session, &batch).await?;
    message.text = content;
    message.data_envelope = projection_envelope(
        source.data_envelope.as_ref(),
        message.text.as_bytes(),
        "reference",
    )?;
    // A truncated text prefix cannot be a trusted full result for later actions.
    if !delivery
        .attachments
        .iter()
        .any(|part| part.metadata.storage_truncated)
    {
        let mut resolved = source;
        resolved.text = canonical;
        resolved.data_envelope = raw.envelope.clone();
        message.resolved_result = Some(Box::new(resolved));
    }
    message.raw_result = Some(Box::new(raw));
    Ok(message)
}

/// Internal validation never consumes content or updates attachment access time.
/// Missing content removes operation authority, but leaves the historical reference.
pub async fn resolve_session(
    store: &dyn SessionSeam,
    session: &mut PersistedAgentSession,
) -> Result<(), AgentError> {
    let subject = session.clone();
    resolve_with(session, |id| {
        let subject = &subject;
        async move { store.read_attachment(subject, &id, false).await }
    })
    .await
}

/// Runtime adapters can resolve under an already held transaction/owner fence.
pub async fn resolve_with<F, Fut>(
    session: &mut PersistedAgentSession,
    mut read: F,
) -> Result<(), AgentError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<PreparedAttachment, AgentError>>,
{
    for index in 0..session.conversation.len() {
        let Some(raw) = session.conversation[index].raw_result.clone() else {
            continue;
        };
        session.conversation[index].resolved_result = None;
        if !raw.restorable {
            continue;
        }
        let mut template = raw.template.clone();
        let mut whole = None;
        let mut missing = false;
        for slot in &raw.slots {
            let part = match read(slot.attachment_id.clone()).await {
                Ok(part) => part,
                Err(_) => {
                    missing = true;
                    break;
                }
            };
            if part.metadata.message_id != session.conversation[index].message_id
                || part.metadata.sha256 != slot.sha256
            {
                return Err(invalid("Raw result attachment binding changed"));
            }
            part.metadata.verify(&part.content)?;
            let text = String::from_utf8(part.content)
                .map_err(|_| invalid("Invalid raw result encoding"))?;
            if let Some(template) = &mut template {
                *template
                    .pointer_mut(&slot.path)
                    .ok_or_else(|| invalid("Invalid raw result slot"))? = Value::String(text);
            } else {
                whole = Some(text);
            }
        }
        if missing {
            continue;
        }
        let text = template
            .map(|value| value.to_string())
            .or(whole)
            .ok_or_else(|| invalid("Raw result body missing"))?;
        if digest(text.as_bytes()) != raw.sha256 {
            return Err(invalid(
                "Raw result reconstruction failed integrity verification",
            ));
        }
        let mut resolved = session.conversation[index].clone();
        resolved.text = text;
        resolved.data_envelope = raw.envelope;
        resolved.raw_result = None;
        resolved.resolved_result = None;
        session.conversation[index].resolved_result = Some(Box::new(resolved));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    #[derive(Default)]
    struct Store(RefCell<Vec<PreparedAttachment>>);
    #[async_trait::async_trait(?Send)]
    impl SessionSeam for Store {
        async fn claim_turn(
            &self,
            _: crate::seam::ClaimTurnParams,
        ) -> Result<PersistedAgentSession, crate::seam::ClaimError> {
            unreachable!()
        }
        async fn save(&self, _: &mut PersistedAgentSession) -> Result<(), AgentError> {
            Ok(())
        }
        async fn store_attachment_batch(
            &self,
            _: &PersistedAgentSession,
            parts: &[PreparedAttachment],
        ) -> Result<Vec<AttachmentMetadata>, AgentError> {
            self.0.borrow_mut().extend_from_slice(parts);
            Ok(parts.iter().map(|part| part.metadata.clone()).collect())
        }
        async fn read_attachment(
            &self,
            _: &PersistedAgentSession,
            id: &str,
            consume: bool,
        ) -> Result<PreparedAttachment, AgentError> {
            assert!(!consume, "internal resolution must not touch LRU");
            self.0
                .borrow()
                .iter()
                .find(|part| part.metadata.attachment_id == id)
                .cloned()
                .ok_or_else(|| invalid("evicted"))
        }
    }
    fn session() -> PersistedAgentSession {
        PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::SuggestOnly,
                expires_at: None,
                policy_name: None,
            },
            "now",
        )
    }
    #[tokio::test]
    async fn json_is_externalized_without_preview_and_restored_only_for_internal_checks() {
        let store = Store::default();
        let mut session = session();
        let original =
            json!({"parent_window_id":"private-id", "body":"data".repeat(2000)}).to_string();
        let message = externalize(
            &store,
            &session,
            ChatMessage::tool_result("message", "call", &original),
            ToolOutputFormat::Json,
            None,
            Ok(1),
        )
        .await
        .unwrap();
        assert!(matches_original(&message, &original, None));
        assert!(!matches_original(&message, "different result", None));
        assert!(!message.text.contains("private-id"));
        assert!(message.text.contains(READ_ATTACHMENT_TOOL));
        assert_eq!(message.trusted_tool_result().text, original);
        session.conversation.push(message);
        let encoded = session.encode_json_for_storage().unwrap();
        assert!(!encoded.contains("private-id"));
        let mut restored = PersistedAgentSession::decode_json(&encoded).unwrap();
        resolve_session(&store, &mut restored).await.unwrap();
        assert_eq!(
            restored.conversation[0].trusted_tool_result().text,
            original
        );
        store.0.borrow_mut().clear();
        resolve_session(&store, &mut restored).await.unwrap();
        assert!(
            !restored.conversation[0]
                .trusted_tool_result()
                .text
                .contains("private-id")
        );
        assert!(restored.conversation[0].text.contains(READ_ATTACHMENT_TOOL));
    }
    #[tokio::test]
    async fn split_command_streams_stay_text_and_reconstruct_typed_original() {
        let store = Store::default();
        let mut session = session();
        let command = OperationOutput::Exec(desk_agent_protocol::ExecOutput {
            exit_code: 19,
            duration_ms: 80,
            redactions: vec![],
            streams: desk_agent_protocol::ExecOutputStreams::Split {
                stdout: "{JSON-looking text}\n".repeat(1000),
                stderr: "small error".into(),
                stdout_truncated: true,
                stderr_truncated: false,
            },
        });
        let original = serde_json::to_string(&command).unwrap();
        let message = externalize(
            &store,
            &session,
            ChatMessage::tool_result("message", "call", original),
            ToolOutputFormat::Operation,
            None,
            Ok(1),
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_str(&message.text).unwrap();
        assert_eq!(body["exit_code"], 19);
        assert_eq!(body["streams"][0]["content"]["storage"], "attachment");
        assert_eq!(body["streams"][1]["content"]["storage"], "inline");
        assert_eq!(store.0.borrow()[0].metadata.kind, ContentKind::Text);
        session.conversation.push(message);
        let mut restored =
            PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap())
                .unwrap();
        resolve_session(&store, &mut restored).await.unwrap();
        assert_eq!(
            serde_json::from_str::<OperationOutput>(
                &restored.conversation[0].trusted_tool_result().text
            )
            .unwrap(),
            command
        );
    }
    #[tokio::test]
    async fn oversized_json_never_creates_an_attachment() {
        let store = Store::default();
        let error = externalize(
            &store,
            &session(),
            ChatMessage::tool_result(
                "message",
                "call",
                json!({"body":"x".repeat(MAX_JSON_BYTES)}).to_string(),
            ),
            ToolOutputFormat::Json,
            None,
            Ok(1),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.kind,
            desk_agent_protocol::AgentErrorKind::OutputLimitExceeded
        );
        assert!(store.0.borrow().is_empty());
    }
}
