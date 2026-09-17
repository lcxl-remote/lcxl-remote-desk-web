//! Immutable, typed tool attachments shared by local and clustered runtimes.
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod batch;
pub mod command;
pub mod delivery;
pub mod model_read;
pub mod read;
pub mod source;
#[cfg(test)]
mod tests;

pub const READ_ATTACHMENT_TOOL: &str = "read_conversation_attachment";
pub const INLINE_BYTES: usize = 4 * 1024;
pub const MAX_JSON_BYTES: usize = 32 * 1024;
pub const MAX_TEXT_BYTES: usize = 400_000;
pub const MAX_PAGE_BYTES: usize = 32 * 1024;
pub const MAX_SESSION_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Json,
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Availability {
    Available,
    Deleted { at_unix_ms: u64 },
    Evicted { at_unix_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMetadata {
    pub attachment_id: String,
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub message_id: String,
    pub tool_call_id: String,
    pub part: String,
    pub kind: ContentKind,
    pub media_type: String,
    pub original_bytes: u64,
    pub size_bytes: u64,
    pub original_sha256: String,
    pub sha256: String,
    pub source_truncated: bool,
    pub storage_truncated: bool,
    pub created_at_unix_ms: u64,
    pub last_accessed_at_unix_ms: u64,
    pub availability: Availability,
    /// Screenshot lineage only; pixels never live in metadata or session JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_source: Option<crate::conversation_image::ImageAttachment>,
    /// Authorized immutable source for text/JSON; never exposed in REST metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
}

impl AttachmentMetadata {
    pub fn verify(&self, content: &[u8]) -> Result<(), AgentError> {
        if [
            &self.attachment_id,
            &self.conversation_id,
            &self.actor_id,
            &self.device_id,
            &self.message_id,
            &self.tool_call_id,
        ]
        .iter()
        .any(|value| value.is_empty() || value.len() > 256)
            || self.part.is_empty()
            || self.part.len() > 32
            || self.media_type.len() > 128
        {
            return Err(invalid("Attachment metadata exceeds its identity bounds"));
        }
        match self.availability {
            Availability::Available => {}
            Availability::Deleted { .. } => return Err(invalid("Attachment was deleted")),
            Availability::Evicted { .. } => {
                return Err(invalid(
                    "Attachment was automatically evicted by the conversation storage quota; its content is unavailable",
                ));
            }
        }
        if self.size_bytes != content.len() as u64 || self.sha256 != digest(content) {
            return Err(invalid("Attachment integrity check failed"));
        }
        let limit = match self.kind {
            ContentKind::Json => MAX_JSON_BYTES,
            ContentKind::Text => MAX_TEXT_BYTES,
            ContentKind::Image => crate::image_input::MAX_IMAGE_DECODED_BYTES,
        };
        if content.len() > limit
            || self.original_bytes < self.size_bytes
            || (self.kind != ContentKind::Text && self.storage_truncated)
            || self.storage_truncated != (self.original_bytes > self.size_bytes)
            || (!self.storage_truncated && self.original_sha256 != self.sha256)
            || self.original_sha256.len() != 64
            || !self
                .original_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("Attachment metadata violates its content budget"));
        }
        match self.kind {
            ContentKind::Json => {
                serde_json::from_slice::<serde_json::Value>(content)
                    .map_err(|_| invalid("Invalid JSON attachment"))?;
            }
            ContentKind::Text => {
                std::str::from_utf8(content).map_err(|_| invalid("Invalid text attachment"))?;
            }
            ContentKind::Image => {
                if content.is_empty()
                    || !crate::image_input::ALLOWED_IMAGE_MEDIA_TYPES
                        .contains(&self.media_type.as_str())
                {
                    return Err(invalid("Invalid image attachment"));
                }
            }
        }
        if let Some(envelope) = &self.source_envelope {
            envelope
                .validate()
                .map_err(|_| invalid("Invalid attachment source label"))?;
            if envelope.digest_sha256 != self.sha256
                || match &envelope.content {
                    desk_agent_protocol::data_lineage::ContentRef::ImmutableBlob {
                        size_bytes,
                        ..
                    }
                    | desk_agent_protocol::data_lineage::ContentRef::EphemeralObservation {
                        size_bytes,
                        ..
                    }
                    | desk_agent_protocol::data_lineage::ContentRef::Artifact {
                        size_bytes, ..
                    } => *size_bytes,
                } != self.size_bytes
            {
                return Err(invalid(
                    "Attachment source label does not match saved bytes",
                ));
            }
        }
        if let Some(source) = &self.image_source {
            if self.kind != ContentKind::Image
                || source.frame.evidence_id != self.attachment_id
                || source.frame.conversation_id != self.conversation_id
                || source.frame.device_id != self.device_id
                || source.frame.tool_call_id != self.tool_call_id
                || source.frame.media_type.as_deref() != Some(self.media_type.as_str())
                || source.message.message_id != self.message_id
                || source.message.image_data_url.is_some()
                || source.frame.preview_data_url.is_some()
                || source.message.text.len() > MAX_JSON_BYTES
            {
                return Err(invalid("Screenshot attachment lineage mismatch"));
            }
            source.restore(content)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedText {
    pub kind: ContentKind,
    pub content: String,
    pub original_bytes: usize,
    pub original_sha256: String,
    pub sha256: String,
    pub storage_truncated: bool,
    pub external: bool,
}

/// Sources serialize JSON once, after redaction and model reference projection.
/// Validation never repairs, reorders, or coerces the source's business data.
pub fn prepare_text(kind: ContentKind, content: String) -> Result<PreparedText, AgentError> {
    match kind {
        ContentKind::Json => {
            if content.len() > MAX_JSON_BYTES {
                return Err(AgentError {
                    kind: AgentErrorKind::OutputLimitExceeded,
                    message: format!(
                        "JSON result is {} bytes, exceeding the {} byte limit. No attachment was saved. Narrow the source tool's supported query/range or page size. Do not repeat an already executed action.",
                        content.len(),
                        MAX_JSON_BYTES
                    ),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                });
            }
            serde_json::from_str::<serde_json::Value>(&content)
                .map_err(|_| invalid("Source tool returned invalid JSON"))?;
        }
        ContentKind::Text => {}
        ContentKind::Image => return Err(invalid("Images require the image validation path")),
    }
    let original_bytes = content.len();
    let original_sha256 = digest(content.as_bytes());
    let mut content = content;
    let storage_truncated = kind == ContentKind::Text && content.len() > MAX_TEXT_BYTES;
    if storage_truncated {
        let end = utf8_end(&content, MAX_TEXT_BYTES);
        content.truncate(end);
    }
    Ok(PreparedText {
        kind,
        external: content.len() > INLINE_BYTES,
        sha256: digest(content.as_bytes()),
        content,
        original_bytes,
        original_sha256,
        storage_truncated,
    })
}

/// Select without mutation. The storage transaction applies this plan only when
/// the complete incoming batch is ready to commit under the conversation fence.
pub fn eviction_candidates<'a>(
    rows: &'a [AttachmentMetadata],
    incoming_bytes: u64,
) -> Result<Vec<&'a AttachmentMetadata>, AgentError> {
    if incoming_bytes > MAX_SESSION_BYTES {
        return Err(invalid("Attachment batch exceeds the conversation quota"));
    }
    let mut available = rows
        .iter()
        .filter(|row| row.availability == Availability::Available)
        .collect::<Vec<_>>();
    if let Some(first) = rows.first()
        && rows.iter().any(|row| {
            row.conversation_id != first.conversation_id
                || row.actor_id != first.actor_id
                || row.device_id != first.device_id
        })
    {
        return Err(invalid(
            "Attachment quota rows cross a conversation boundary",
        ));
    }
    available.sort_by(|a, b| {
        (
            a.last_accessed_at_unix_ms,
            a.created_at_unix_ms,
            &a.attachment_id,
        )
            .cmp(&(
                b.last_accessed_at_unix_ms,
                b.created_at_unix_ms,
                &b.attachment_id,
            ))
    });
    let total = available.iter().try_fold(incoming_bytes, |sum, row| {
        sum.checked_add(row.size_bytes)
            .ok_or_else(|| invalid("Attachment quota arithmetic overflow"))
    })?;
    let mut needed = total.saturating_sub(MAX_SESSION_BYTES);
    let mut evicted = Vec::new();
    for row in available {
        if needed == 0 {
            break;
        }
        needed = needed.saturating_sub(row.size_bytes);
        evicted.push(row);
    }
    Ok(evicted)
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn utf8_end(text: &str, max: usize) -> usize {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub fn invalid(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
