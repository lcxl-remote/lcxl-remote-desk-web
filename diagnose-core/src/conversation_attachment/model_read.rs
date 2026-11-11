//! Model reads retain their body until an accepted response and a completed turn.
use super::{ContentKind, MAX_PAGE_BYTES, batch::PreparedAttachment, invalid, read::*};
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
        {
            // max_bytes bounds text pages only. Argument preparation fills its schema
            // default, so its presence never means the model asked to page an image.
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
    pub projection_version: u32,
    pub body_bytes: usize,
    pub original_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
    pub consumed: bool,
    pub released: bool,
}

#[cfg(test)]
/// Includes JSON escaping and protocol message framing, not just raw page bytes.
pub fn text_page(
    part: &PreparedAttachment,
    input: &Input,
    call_id: &str,
    budget: usize,
    observation: &crate::model_observability::tool::ToolObservation,
) -> Result<(String, ReadReceipt), AgentError> {
    text_page_impl(part, input, call_id, budget, None, observation, None)
}

/// `framed_limit` bounds the encoded tool response, including JSON escaping
/// and protocol framing, when the turn shares a response byte budget.
pub(crate) fn text_page_with_references(
    part: &PreparedAttachment,
    input: &Input,
    call_id: &str,
    budget: usize,
    framed_limit: Option<u32>,
    observation: &crate::model_observability::tool::ToolObservation,
    references: &mut crate::ui_references::UiReferenceState,
) -> Result<(String, ReadReceipt), AgentError> {
    text_page_impl(
        part,
        input,
        call_id,
        budget,
        framed_limit,
        observation,
        Some(references),
    )
}

fn text_page_impl(
    part: &PreparedAttachment,
    input: &Input,
    call_id: &str,
    budget: usize,
    framed_limit: Option<u32>,
    observation: &crate::model_observability::tool::ToolObservation,
    mut references: Option<&mut crate::ui_references::UiReferenceState>,
) -> Result<(String, ReadReceipt), AgentError> {
    if part.metadata.kind == ContentKind::Image {
        return Err(invalid("Use the image read path"));
    }
    let mut request = input.request()?;
    part.metadata.verify(&part.content)?;
    let projected = project_json_part(part, references.as_deref())?;
    let page_part = projected.as_ref().unwrap_or(part);
    let mut upper = request.max_bytes;
    // Reduce only the new read; never discard an earlier read to make it fit.
    loop {
        request.max_bytes = upper;
        let mut page = read_page_observed(
            &page_part.metadata,
            &page_part.content,
            &request,
            observation,
        )?;
        let native_cursor = page.cursor.clone();
        if let Some(references) = references.as_deref() {
            if let Some(cursor) = native_cursor.as_deref() {
                page.cursor = Some(references.attachment_cursor_alias(cursor)?);
            }
            if let Some(alias) = references.attachment_alias(&page.attachment_id) {
                page.attachment_id = alias.into();
            }
        }
        let content =
            serde_json::to_string(&page).map_err(|_| invalid("Cannot encode attachment page"))?;
        let framed_fits = match framed_limit {
            Some(maximum) => {
                crate::ai_assistant::history::serialized_tool_response_bytes(
                    call_id, &content, None,
                )
                .map_err(invalid)?
                    <= maximum
            }
            None => true,
        };
        if framed_fits
            && crate::trim::model_context_cost(&ChatMessage::tool_result(
                "attachment-page",
                call_id,
                &content,
            )) <= budget
        {
            if let (Some(references), Some(cursor)) =
                (references.as_deref_mut(), native_cursor.as_deref())
            {
                let registered = references.register_attachment_cursor(
                    &part.metadata.attachment_id,
                    &part.metadata.message_id,
                    cursor,
                )?;
                if page.cursor.as_deref() != Some(registered.as_str()) {
                    return Err(invalid("Attachment cursor changed before page commit"));
                }
            }
            let mut selected = input.clone();
            selected.max_bytes = Some(upper);
            return Ok((
                content.clone(),
                ReadReceipt {
                    input: selected,
                    projection_version: if projected.is_some() { 2 } else { 1 },
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
fn project_json_part(
    part: &PreparedAttachment,
    references: Option<&crate::ui_references::UiReferenceState>,
) -> Result<Option<PreparedAttachment>, AgentError> {
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
    crate::ui_model_ids::project_result_view(&mut message, references);
    if message.text.as_bytes() == part.content.as_slice() {
        return Ok(None);
    }
    let mut projected = part.clone();
    projected.content = message.text.into_bytes();
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
            "notice": "This page was consumed in a completed turn. Read the attachment again if its body is needed."
        }}).to_string()
}

/// Stable identity for summary proofs across consumption and body release.
/// It binds the attachment, selection and body size, not the rendered page
/// body: the page comes from the same immutable attachment in trusted storage.
pub fn canonical_source(message: &ChatMessage) -> Result<ChatMessage, AgentError> {
    let Some(receipt) = &message.attachment_read else {
        return Ok(message.clone());
    };
    if receipt.released && message.text != receipt_content(receipt) {
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
        if !matches!(receipt.projection_version, 1 | 2) {
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
            serde_json::json!({"results":[source],"padding":"x".repeat(20000)}),
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
        assert_eq!(proof, receipt);
    }
    #[test]
    fn short_page_cursor_survives_unrelated_registration() {
        let (part, mut input) = fixture();
        input.max_bytes = Some(1024);
        let mut references = crate::ui_references::UiReferenceState::default();
        let (first, _) = text_page_with_references(
            &part,
            &input,
            "first",
            100000,
            None,
            &crate::model_observability::tool::ToolObservation::default(),
            &mut references,
        )
        .unwrap();
        let page: ReadPage = serde_json::from_str(&first).unwrap();
        let cursor = page.cursor.unwrap();
        assert_eq!(cursor, "c1");
        references
            .register_attachment_cursor("other", "other-message", "unrelated-query")
            .unwrap();
        let call = crate::chat::ToolCall {
            id: "next".into(),
            name: super::super::READ_ATTACHMENT_TOOL.into(),
            arguments_json:
                serde_json::json!({"attachment_id":input.attachment_id,"cursor":cursor}).to_string(),
        };
        let (expanded, _) = references.expand_call(&call).unwrap();
        input.cursor = serde_json::from_str::<serde_json::Value>(&expanded.arguments_json).unwrap()
            ["cursor"]
            .as_str()
            .map(str::to_owned);
        let (second, proof) = text_page_with_references(
            &part,
            &input,
            "second",
            100000,
            None,
            &crate::model_observability::tool::ToolObservation::default(),
            &mut references,
        )
        .unwrap();
        let second_page: ReadPage = serde_json::from_str(&second).unwrap();
        assert_ne!(page.lines[0].line, second_page.lines[0].line);
        let mut message = ChatMessage::tool_result("page", "second", &second);
        message.attachment_read = Some(Box::new(proof));
        canonical_source(&message).unwrap();
        let projected =
            crate::ui_model_ids::project_request(&crate::seam::ModelRequest::text_only(
                vec![message.clone()],
                crate::prompt::ResponseFormatSpec::None,
            ));
        assert_eq!(projected.messages[0].text, second);
    }

    #[test]
    fn pages_and_receipts_name_registered_attachments_by_alias() {
        let (part, input) = fixture();
        let native = part.metadata.attachment_id.clone();
        let observation = crate::model_observability::tool::ToolObservation::default();
        let mut unregistered = crate::ui_references::UiReferenceState::default();
        let (content, _) = text_page_with_references(
            &part,
            &input,
            "read",
            100000,
            None,
            &observation,
            &mut unregistered,
        )
        .unwrap();
        let page: ReadPage = serde_json::from_str(&content).unwrap();
        assert_eq!(page.attachment_id, native);

        let mut references = crate::ui_references::UiReferenceState::default();
        references
            .register_attachment(&native, &part.metadata.message_id)
            .unwrap();
        let (content, mut receipt) = text_page_with_references(
            &part,
            &input,
            "read",
            100000,
            None,
            &observation,
            &mut references,
        )
        .unwrap();
        let page: ReadPage = serde_json::from_str(&content).unwrap();
        assert_eq!(page.attachment_id, "att1");
        assert!(!content.contains(&native));
        assert_eq!(receipt.input.attachment_id, native);

        receipt.consumed = true;
        let mut message = ChatMessage::tool_result("page", "read", &content);
        message.attachment_read = Some(Box::new(receipt));
        release_consumed(std::slice::from_mut(&mut message)).unwrap();
        assert!(message.text.contains(&native));
        let mut projected = message.clone();
        references.project_message(&mut projected);
        let value: serde_json::Value = serde_json::from_str(&projected.text).unwrap();
        assert_eq!(value["attachment_read"]["attachment_id"], "att1");
        assert_eq!(
            value["attachment_read"]["selection"]["attachment_id"],
            "att1"
        );
        assert!(!projected.text.contains(&native));
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
    fn a_full_escaped_page_shrinks_to_the_shared_response_limit() {
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
                content: PartContent::Text(format!("{}\n", "\"\\".repeat(400)).repeat(100)),
                source_truncated: false,
            }],
            1,
        )
        .unwrap();
        let part = delivery.attachments.remove(0);
        let (_, mut input) = fixture();
        input.attachment_id = part.metadata.attachment_id.clone();
        input.max_bytes = Some(32768);
        let observation = crate::model_observability::tool::ToolObservation::default();
        let framed = |content: &str| {
            crate::ai_assistant::history::serialized_tool_response_bytes("read", content, None)
                .unwrap()
        };
        let mut unbounded = crate::ui_references::UiReferenceState::default();
        let (full, _) = text_page_with_references(
            &part,
            &input,
            "read",
            1_000_000,
            None,
            &observation,
            &mut unbounded,
        )
        .unwrap();
        // The default page alone exceeds the shared limit once escaped.
        assert!(framed(&full) > 32768);
        let mut references = crate::ui_references::UiReferenceState::default();
        let (content, receipt) = text_page_with_references(
            &part,
            &input,
            "read",
            1_000_000,
            Some(32768),
            &observation,
            &mut references,
        )
        .unwrap();
        assert!(framed(&content) <= 32768);
        let page: ReadPage = serde_json::from_str(&content).unwrap();
        assert!(page.cursor.is_some());
        assert!(receipt.input.max_bytes.unwrap() < 32768);
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
    fn image_read_accepts_the_prepared_arguments_of_a_bare_attachment_id() {
        let mut prepared = serde_json::json!({"attachment_id":"visual-1"});
        crate::model_input::fill_versions(super::super::READ_ATTACHMENT_TOOL, &mut prepared);
        assert_eq!(prepared["max_bytes"], serde_json::json!(32768));
        let input: Input = serde_json::from_value(prepared).unwrap();
        input.validate_image().unwrap();
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
