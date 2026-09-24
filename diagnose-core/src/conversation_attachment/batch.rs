//! Prepare a whole tool delivery before either runtime changes storage or LRU.
use super::{
    AttachmentMetadata, Availability, ContentKind, MAX_SESSION_BYTES, digest, invalid, prepare_text,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_PARTS: usize = 8;
pub const ATTACHMENT_QUOTA_ERROR: &str = "Attachment batch exceeds the conversation quota";

fn capacity() -> AgentError {
    AgentError {
        kind: AgentErrorKind::AttachmentCapacity,
        message: ATTACHMENT_QUOTA_ERROR.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// The source chooses its format. JSON is never inferred from a text prefix.
#[derive(Debug, Clone)]
pub enum PartContent {
    Json(String),
    Text(String),
    ImageDataUrl(String),
}

#[derive(Debug, Clone)]
pub struct OutputPart {
    pub name: String,
    pub content: PartContent,
    /// Capturing/parsing the source was incomplete, independently of storage.
    pub source_truncated: bool,
}

pub struct DeliveryIdentity<'a> {
    pub conversation_id: &'a str,
    pub actor_id: &'a str,
    pub device_id: &'a str,
    pub message_id: &'a str,
    pub tool_call_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct PreparedAttachment {
    pub metadata: AttachmentMetadata,
    pub content: Vec<u8>,
}

/// This contains no body preview. It is also suitable for the conversation UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentReference {
    pub attachment_id: String,
    pub kind: ContentKind,
    pub original_bytes: u64,
    pub stored_bytes: u64,
    pub storage_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub enum DeliveredContent {
    Inline { kind: ContentKind, text: String },
    Attachment { reference: AttachmentReference },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredPart {
    pub name: String,
    pub source_truncated: bool,
    pub content: DeliveredContent,
}

/// Only publish `parts` after committing all `attachments` in one transaction.
#[derive(Debug)]
pub struct PreparedDelivery {
    pub parts: Vec<DeliveredPart>,
    pub attachments: Vec<PreparedAttachment>,
    pub stored_bytes: u64,
}

/// Apply under the parent conversation's database lock, never against a cache.
#[derive(Debug, PartialEq, Eq)]
pub struct BatchWritePlan {
    pub insert_indices: Vec<usize>,
    pub evict_ids: Vec<String>,
}

pub fn plan_batch(
    existing: &[AttachmentMetadata],
    incoming: &[PreparedAttachment],
) -> Result<BatchWritePlan, AgentError> {
    plan_batch_protecting(existing, incoming, &[])
}

/// Reserve current goal/checkpoint dependencies before choosing LRU victims.
/// The caller must read these IDs under the same parent-session lock used for
/// the attachment write.
pub fn plan_batch_protecting(
    existing: &[AttachmentMetadata],
    incoming: &[PreparedAttachment],
    protected_ids: &[String],
) -> Result<BatchWritePlan, AgentError> {
    if incoming.len() > MAX_PARTS {
        return Err(invalid("Too many attachment parts"));
    }
    let mut plan = BatchWritePlan {
        insert_indices: vec![],
        evict_ids: vec![],
    };
    let mut protected: BTreeSet<String> = protected_ids.iter().cloned().collect();
    let mut incoming_ids = BTreeSet::new();
    let mut additional = 0_u64;
    let subject = incoming
        .first()
        .map(|part| &part.metadata)
        .or_else(|| existing.first());
    if let Some(subject) = subject {
        for metadata in existing
            .iter()
            .chain(incoming.iter().map(|part| &part.metadata))
        {
            if metadata.conversation_id != subject.conversation_id
                || metadata.actor_id != subject.actor_id
                || metadata.device_id != subject.device_id
            {
                return Err(invalid("Attachment batch crosses a conversation boundary"));
            }
        }
    }
    let mut known_ids = BTreeSet::new();
    for metadata in existing {
        if !known_ids.insert(&metadata.attachment_id) {
            return Err(invalid("Duplicate attachment storage row"));
        }
    }
    for (index, part) in incoming.iter().enumerate() {
        part.metadata.verify(&part.content)?;
        if !incoming_ids.insert(part.metadata.attachment_id.clone()) {
            return Err(invalid("Duplicate incoming attachment identity"));
        }
        protected.insert(part.metadata.attachment_id.clone());
        if let Some(stored) = existing
            .iter()
            .find(|row| row.attachment_id == part.metadata.attachment_id)
        {
            if stored.availability != Availability::Available {
                return Err(invalid(
                    "Attachment was deleted or evicted; delivery cannot restore it",
                ));
            }
            let mut compared = part.metadata.clone();
            compared.created_at_unix_ms = stored.created_at_unix_ms;
            compared.last_accessed_at_unix_ms = stored.last_accessed_at_unix_ms;
            if &compared != stored {
                return Err(invalid(
                    "Attachment identity was reused with different content or metadata",
                ));
            }
        } else {
            additional = additional
                .checked_add(part.metadata.size_bytes)
                .ok_or_else(|| invalid("Attachment quota arithmetic overflow"))?;
            plan.insert_indices.push(index);
        }
    }
    for id in protected_ids {
        if !incoming_ids.contains(id)
            && !existing
                .iter()
                .any(|row| row.attachment_id == *id && row.availability == Availability::Available)
        {
            return Err(invalid("Required goal attachment is missing"));
        }
    }
    let mut total = existing
        .iter()
        .filter(|row| row.availability == Availability::Available)
        .try_fold(additional, |sum, row| sum.checked_add(row.size_bytes))
        .ok_or_else(|| invalid("Attachment quota arithmetic overflow"))?;
    let mut candidates = existing
        .iter()
        .filter(|row| {
            row.availability == Availability::Available && !protected.contains(&row.attachment_id)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
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
    for candidate in candidates {
        if total <= MAX_SESSION_BYTES {
            break;
        }
        total -= candidate.size_bytes;
        plan.evict_ids.push(candidate.attachment_id.clone());
    }
    if total > MAX_SESSION_BYTES {
        return Err(capacity());
    }
    Ok(plan)
}

pub fn prepare_delivery(
    identity: &DeliveryIdentity<'_>,
    parts: Vec<OutputPart>,
    created_at_unix_ms: u64,
) -> Result<PreparedDelivery, AgentError> {
    if parts.len() > MAX_PARTS
        || [
            identity.conversation_id,
            identity.actor_id,
            identity.device_id,
            identity.message_id,
            identity.tool_call_id,
        ]
        .iter()
        .any(|value| value.is_empty() || value.len() > 256)
    {
        return Err(invalid(
            "Invalid attachment delivery identity or part count",
        ));
    }
    let mut names = BTreeSet::new();
    let mut result = PreparedDelivery {
        parts: vec![],
        attachments: vec![],
        stored_bytes: 0,
    };
    for part in parts {
        if part.name.is_empty()
            || part.name.len() > 32
            || !part
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !names.insert(part.name.clone())
        {
            return Err(invalid("Attachment parts must have distinct bounded names"));
        }
        let (
            kind,
            content,
            original_bytes,
            original_sha256,
            storage_truncated,
            external,
            media_type,
        ) = match part.content {
            PartContent::Json(text) => prepare_body(ContentKind::Json, text, "application/json")?,
            PartContent::Text(text) => prepare_body(ContentKind::Text, text, "text/plain")?,
            PartContent::ImageDataUrl(url) => {
                let info = crate::image_input::validate_image_data_url(&url)
                    .map_err(|_| invalid("Invalid image attachment input"))?;
                let bytes = STANDARD
                    .decode(
                        url.split_once(',')
                            .ok_or_else(|| invalid("Invalid image URL"))?
                            .1,
                    )
                    .map_err(|_| invalid("Invalid image attachment encoding"))?;
                let size = bytes.len() as u64;
                let hash = digest(&bytes);
                (
                    ContentKind::Image,
                    bytes,
                    size,
                    hash,
                    false,
                    true,
                    info.media_type,
                )
            }
        };
        let delivered = if external {
            let attachment_id = format!(
                "attachment-{}",
                digest(
                    &serde_json::to_vec(&(
                        identity.conversation_id,
                        identity.actor_id,
                        identity.device_id,
                        identity.message_id,
                        identity.tool_call_id,
                        &part.name,
                    ))
                    .map_err(|_| invalid("Cannot encode attachment identity"))?
                )
            );
            let metadata = AttachmentMetadata {
                attachment_id: attachment_id.clone(),
                conversation_id: identity.conversation_id.into(),
                actor_id: identity.actor_id.into(),
                device_id: identity.device_id.into(),
                message_id: identity.message_id.into(),
                tool_call_id: identity.tool_call_id.into(),
                part: part.name.clone(),
                kind,
                media_type,
                original_bytes,
                size_bytes: content.len() as u64,
                original_sha256,
                sha256: digest(&content),
                source_truncated: part.source_truncated,
                storage_truncated,
                created_at_unix_ms,
                last_accessed_at_unix_ms: created_at_unix_ms,
                availability: Availability::Available,
                image_source: None,
                source_envelope: None,
            };
            metadata.verify(&content)?;
            result.stored_bytes = result
                .stored_bytes
                .checked_add(metadata.size_bytes)
                .filter(|total| *total <= MAX_SESSION_BYTES)
                .ok_or_else(capacity)?;
            let reference = AttachmentReference {
                attachment_id,
                kind,
                original_bytes,
                stored_bytes: metadata.size_bytes,
                storage_truncated,
            };
            result
                .attachments
                .push(PreparedAttachment { metadata, content });
            DeliveredContent::Attachment { reference }
        } else {
            DeliveredContent::Inline {
                kind,
                text: String::from_utf8(content)
                    .map_err(|_| invalid("Invalid inline result encoding"))?,
            }
        };
        result.parts.push(DeliveredPart {
            name: part.name,
            source_truncated: part.source_truncated,
            content: delivered,
        });
    }
    Ok(result)
}

type PreparedBody = (ContentKind, Vec<u8>, u64, String, bool, bool, String);

fn prepare_body(
    kind: ContentKind,
    text: String,
    media_type: &str,
) -> Result<PreparedBody, AgentError> {
    let prepared = prepare_text(kind, text)?;
    Ok((
        kind,
        prepared.content.into_bytes(),
        prepared.original_bytes as u64,
        prepared.original_sha256,
        prepared.storage_truncated,
        prepared.external,
        media_type.into(),
    ))
}
