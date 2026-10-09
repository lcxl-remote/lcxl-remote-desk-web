//! Model reads retain their body until an accepted response and a completed turn.
use super::{ContentKind, MAX_PAGE_BYTES, batch::PreparedAttachment, digest, invalid, read::*};
use crate::chat::ChatMessage;
use desk_agent_protocol::AgentError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub attachment_id: String,
    pub cursor: Option<String>,
    pub queries: Option<Vec<String>>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub before_context: usize,
    #[serde(default)]
    pub after_context: usize,
    pub max_bytes: Option<usize>,
}

impl Input {
    pub fn request(&self) -> Result<ReadRequest, AgentError> {
        if self.attachment_id.is_empty()
            || self.attachment_id.len() > 256
            || (self.queries.is_some() && (self.start_line.is_some() || self.end_line.is_some()))
            || (self.queries.is_none()
                && (self.ignore_case || self.before_context != 0 || self.after_context != 0))
        {
            return Err(invalid(
                "Invalid attachment selection; search and line ranges cannot be combined",
            ));
        }
        Ok(ReadRequest {
            attachment_id: self.attachment_id.clone(),
            selection: match &self.queries {
                Some(queries) => ReadMode::Search {
                    queries: queries.clone(),
                    ignore_case: self.ignore_case,
                    before_context: self.before_context,
                    after_context: self.after_context,
                },
                None => ReadMode::Read {
                    start_line: self.start_line,
                    end_line: self.end_line,
                },
            },
            max_bytes: self.max_bytes.unwrap_or(MAX_PAGE_BYTES),
            limit: 1000,
            cursor: self.cursor.clone(),
        })
    }

    pub fn validate_image(&self) -> Result<(), AgentError> {
        self.request()?;
        if self.cursor.is_some()
            || self.queries.is_some()
            || self.start_line.is_some()
            || self.end_line.is_some()
            || self.max_bytes.is_some()
        {
            return Err(invalid(
                "Images do not support text search, line ranges or paging",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadReceipt {
    pub input: Input,
    pub source_sha256: String,
    pub projection_version: u32,
    pub page_sha256: String,
    pub body_bytes: usize,
    pub original_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
    pub consumed: bool,
    pub released: bool,
}

/// Includes JSON escaping and protocol message framing, not just raw page bytes.
pub fn text_page(
    part: &PreparedAttachment,
    input: &Input,
    call_id: &str,
    budget: usize,
    observation: &crate::model_observability::tool::ToolObservation,
) -> Result<(String, ReadReceipt), AgentError> {
    if part.metadata.kind == ContentKind::Image {
        return Err(invalid("Use the image read path"));
    }
    let mut request = input.request()?;
    part.metadata.verify(&part.content)?;
    let projected = project_json_part(part)?;
    let page_part = projected.as_ref().unwrap_or(part);
    let mut upper = request.max_bytes;
    // Reduce only the new read; never discard an earlier read to make it fit.
    loop {
        request.max_bytes = upper;
        let page = read_page_observed(
            &page_part.metadata,
            &page_part.content,
            &request,
            observation,
        )?;
        let content =
            serde_json::to_string(&page).map_err(|_| invalid("Cannot encode attachment page"))?;
        if crate::trim::model_context_cost(&ChatMessage::tool_result(
            "attachment-page",
            call_id,
            &content,
        )) <= budget
        {
            let mut selected = input.clone();
            selected.max_bytes = Some(upper);
            return Ok((
                content.clone(),
                ReadReceipt {
                    input: selected,
                    source_sha256: part.metadata.sha256.clone(),
                    projection_version: if projected.is_some() { 2 } else { 1 },
                    page_sha256: digest(content.as_bytes()),
                    body_bytes: page.body_bytes,
                    original_envelope: None,
                    consumed: false,
                    released: false,
                },
            ));
        }
        if upper <= 4 {
            return Err(invalid(
                "Insufficient model context for this attachment page; complete or compress the current turn before reading more",
            ));
        }
        upper = (upper / 2).max(4);
    }
}

/// Model paging uses the same reducing view as inline results. Source proof and
/// storage remain bound to the original bytes, not the derived page representation.
fn project_json_part(part: &PreparedAttachment) -> Result<Option<PreparedAttachment>, AgentError> {
    if part.metadata.kind != ContentKind::Json {
        return Ok(None);
    }
    let Some(source) = &part.metadata.source_envelope else {
        return Ok(None);
    };
    let mut message = ChatMessage::tool_result(
        &part.metadata.message_id,
        &part.metadata.tool_call_id,
        std::str::from_utf8(&part.content)
            .map_err(|_| invalid("Invalid JSON attachment encoding"))?,
    );
    message.data_envelope = Some(source.clone());
    crate::ui_model_ids::project_tool_message(&mut message);
    if message.text.as_bytes() == part.content.as_slice() {
        return Ok(None);
    }
    let mut projected = part.clone();
    projected.content = message.text.into_bytes();
    projected.metadata.sha256 = digest(&projected.content);
    projected.metadata.original_sha256 = projected.metadata.sha256.clone();
    projected.metadata.size_bytes = projected.content.len() as u64;
    projected.metadata.original_bytes = projected.metadata.size_bytes;
    projected.metadata.storage_truncated = false;
    // This is a derived local view. The receipt retains the original source
    // proof; its signed label must never be relabelled as the projected bytes.
    projected.metadata.source_envelope = None;
    Ok(Some(projected))
}

fn receipt_content(receipt: &ReadReceipt) -> String {
    serde_json::json!({"attachment_read": {
            "attachment_id": receipt.input.attachment_id,
            "selection": receipt.input,
            "body_bytes": receipt.body_bytes,
            "page_sha256": receipt.page_sha256,
            "notice": "This page was consumed in a completed turn. Read the attachment again if its body is needed."
        }}).to_string()
}

/// Stable identity for summary proofs across consumption and body release.
/// The page digest still binds the exact body seen by the model.
pub fn canonical_source(message: &ChatMessage) -> Result<ChatMessage, AgentError> {
    let Some(receipt) = &message.attachment_read else {
        return Ok(message.clone());
    };
    if (receipt.released && message.text != receipt_content(receipt))
        || (!receipt.released && digest(message.text.as_bytes()) != receipt.page_sha256)
    {
        return Err(invalid("Attachment read proof does not match its body"));
    }
    let mut canonical = message.clone();
    canonical.text = receipt_content(receipt);
    canonical.data_envelope = receipt.original_envelope.clone();
    let record = canonical.attachment_read.as_mut().unwrap();
    record.consumed = false;
    record.released = false;
    Ok(canonical)
}

/// Unfinished reads cannot silently reuse a cached body after attachment eviction.
/// Internal verification does not count as user/model access for LRU purposes.
pub async fn validate_pending_reads(
    store: &dyn crate::seam::SessionSeam,
    session: &crate::session::PersistedAgentSession,
) -> Result<(), AgentError> {
    for message in &session.conversation {
        let Some(receipt) = &message.attachment_read else {
            continue;
        };
        canonical_source(message)?;
        if receipt.released {
            continue;
        }
        let part = store
            .read_attachment(session, &receipt.input.attachment_id, false)
            .await?;
        part.metadata.verify(&part.content)?;
        if part.metadata.sha256 != receipt.source_sha256
            || !matches!(receipt.projection_version, 1 | 2)
        {
            return Err(invalid(
                "Attachment source or page representation changed; the unfinished read cannot be replayed",
            ));
        }
    }
    Ok(())
}

pub fn mark_consumed(messages: &mut [ChatMessage], sent_ids: &std::collections::HashSet<String>) {
    for message in messages {
        if sent_ids.contains(&message.message_id)
            && let Some(receipt) = &mut message.attachment_read
        {
            receipt.consumed = true;
        }
    }
}

/// Caller invokes only after a successfully answered turn, never for a pause/error.
pub fn release_consumed(messages: &mut [ChatMessage]) -> Result<(), AgentError> {
    for message in messages {
        let Some(receipt) = &message.attachment_read else {
            continue;
        };
        if !receipt.consumed || receipt.released {
            continue;
        }
        if digest(message.text.as_bytes()) != receipt.page_sha256 {
            return Err(invalid(
                "Attachment page changed before its receipt was committed",
            ));
        }
        let content = receipt_content(receipt);
        message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            message.data_envelope.as_ref(),
            message
                .tool_call_id
                .as_deref()
                .unwrap_or(&message.message_id),
            &content,
            "attachment_read_receipt",
        )?;
        message.text = content;
        message.attachment_read.as_mut().unwrap().released = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_attachment::batch::*;
    #[test]
    fn json_paging_uses_short_ids_and_keeps_original_storage_proof() {
        let source = serde_json::json!({"title":"Observed title","url":"https://example.test/a"});
        let history = crate::result_model_ids::tests::evidence(
            "search_public_web",
            "search-call",
            serde_json::json!({"query":"test"}),
            serde_json::json!({"results":[source],"padding":"x".repeat(12000)}),
        );
        let result = history.last().unwrap();
        let mut delivery = prepare_delivery(
            &DeliveryIdentity {
                conversation_id: "run",
                actor_id: "owner",
                device_id: "device",
                message_id: "message",
                tool_call_id: "search-call",
            },
            vec![OutputPart {
                name: "result".into(),
                content: PartContent::Json(result.text.clone()),
                source_truncated: false,
            }],
            1,
        )
        .unwrap();
        let mut part = delivery.attachments.remove(0);
        part.metadata.source_envelope = result.data_envelope.clone();
        let original = part.content.clone();
        let original_digest = part.metadata.sha256.clone();
        let input = Input {
            attachment_id: part.metadata.attachment_id.clone(),
            cursor: None,
            queries: None,
            start_line: None,
            end_line: None,
            ignore_case: false,
            before_context: 0,
            after_context: 0,
            max_bytes: Some(32768),
        };
        let (content, receipt) = text_page(
            &part,
            &input,
            "read-part",
            100000,
            &crate::model_observability::tool::ToolObservation::default(),
        )
        .unwrap();
        assert!(content.contains("search_result_id"));
        assert!(content.contains("source_id"));
        assert_eq!(receipt.projection_version, 2);
        assert_eq!(receipt.source_sha256, original_digest);
        assert_eq!(part.content, original);
        part.metadata.verify(&part.content).unwrap();
        let (again, proof) = text_page(
            &part,
            &input,
            "read-again",
            100000,
            &crate::model_observability::tool::ToolObservation::default(),
        )
        .unwrap();
        assert_eq!(again, content);
        assert_eq!(proof.page_sha256, receipt.page_sha256);
    }
    fn fixture() -> (PreparedAttachment, Input) {
        let mut delivery = prepare_delivery(
            &DeliveryIdentity {
                conversation_id: "run",
                actor_id: "owner",
                device_id: "device",
                message_id: "message",
                tool_call_id: "call",
            },
            vec![OutputPart {
                name: "result".into(),
                content: PartContent::Text("\"\\\t\n".repeat(4000)),
                source_truncated: false,
            }],
            1,
        )
        .unwrap();
        let part = delivery.attachments.remove(0);
        let input = Input {
            attachment_id: part.metadata.attachment_id.clone(),
            cursor: None,
            queries: None,
            start_line: None,
            end_line: None,
            ignore_case: false,
            before_context: 0,
            after_context: 0,
            max_bytes: None,
        };
        (part, input)
    }
    #[test]
    fn page_budget_counts_escaped_protocol_text_and_read_receipt_waits_for_consumption() {
        let (part, input) = fixture();
        let (content, receipt) = text_page(
            &part,
            &input,
            "read-call",
            2500,
            &crate::model_observability::tool::ToolObservation::default(),
        )
        .unwrap();
        let mut message = ChatMessage::tool_result("page", "read-call", &content);
        message.attachment_read = Some(Box::new(receipt));
        assert!(crate::trim::model_context_cost(&message) <= 2500);
        let proof = serde_json::to_string(&canonical_source(&message).unwrap()).unwrap();
        let mut messages = vec![message];
        release_consumed(&mut messages).unwrap();
        assert_eq!(messages[0].text, content);
        mark_consumed(
            &mut messages,
            &std::collections::HashSet::from(["another-page".into()]),
        );
        release_consumed(&mut messages).unwrap();
        assert_eq!(messages[0].text, content);
        mark_consumed(
            &mut messages,
            &std::collections::HashSet::from(["page".into()]),
        );
        assert_eq!(
            messages[0].text, content,
            "acceptance alone cannot clear a still-running turn"
        );
        release_consumed(&mut messages).unwrap();
        assert!(messages[0].attachment_read.as_ref().unwrap().released);
        assert_eq!(
            proof,
            serde_json::to_string(&canonical_source(&messages[0]).unwrap()).unwrap()
        );
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("read-call"));
        let receipt = messages[0].text.clone();
        release_consumed(&mut messages).unwrap();
        assert_eq!(messages[0].text, receipt);
    }
    #[test]
    fn incompatible_selectors_and_insufficient_budget_fail_without_an_empty_page() {
        let (part, mut input) = fixture();
        input.queries = Some(vec!["x".into()]);
        input.start_line = Some(1);
        assert!(input.request().is_err());
        input.start_line = None;
        assert!(input.validate_image().is_err());
        input.queries = None;
        assert!(
            text_page(
                &part,
                &input,
                "call",
                20,
                &crate::model_observability::tool::ToolObservation::default()
            )
            .is_err()
        );
    }
}
