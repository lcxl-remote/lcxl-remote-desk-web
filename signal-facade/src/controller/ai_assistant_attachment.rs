//! Public attachment metadata excludes source envelopes and internal references.
use desk_diagnose_core::conversation_attachment::{AttachmentMetadata, Availability, ContentKind};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub struct AttachmentQuery {
    pub session: String,
    pub attachment: Option<String>,
    pub before: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteAttachments {
    pub session: String,
    pub attachment_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AttachmentDto {
    pub attachment_id: String,
    pub message_id: String,
    pub tool_call_id: String,
    pub part: String,
    pub kind: String,
    pub media_type: String,
    pub original_bytes: u64,
    pub stored_bytes: u64,
    pub source_truncated: bool,
    pub storage_truncated: bool,
    pub created_at_unix_ms: u64,
    pub last_accessed_at_unix_ms: u64,
    pub status: String,
    pub unavailable_at_unix_ms: Option<u64>,
}

impl From<AttachmentMetadata> for AttachmentDto {
    fn from(value: AttachmentMetadata) -> Self {
        let (status, unavailable_at_unix_ms) = match value.availability {
            Availability::Available => ("available", None),
            Availability::Deleted { at_unix_ms } => ("deleted", Some(at_unix_ms)),
            Availability::Evicted { at_unix_ms } => ("evicted", Some(at_unix_ms)),
        };
        Self {
            attachment_id: value.attachment_id,
            message_id: value.message_id,
            tool_call_id: value.tool_call_id,
            part: value.part,
            kind: match value.kind {
                ContentKind::Json => "json",
                ContentKind::Text => "text",
                ContentKind::Image => "image",
            }
            .into(),
            media_type: value.media_type,
            original_bytes: value.original_bytes,
            stored_bytes: value.size_bytes,
            source_truncated: value.source_truncated,
            storage_truncated: value.storage_truncated,
            created_at_unix_ms: value.created_at_unix_ms,
            last_accessed_at_unix_ms: value.last_accessed_at_unix_ms,
            status: status.into(),
            unavailable_at_unix_ms,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AttachmentListDto {
    pub attachments: Vec<AttachmentDto>,
    pub cursor: Option<String>,
    pub used_bytes: u64,
    pub capacity_bytes: u64,
}

/// Binary download only; failures use the standard JSON business-error body.
#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct AttachmentBytes(pub Vec<u8>);

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadAttachment {
    pub session: String,
    pub attachment_id: String,
    pub cursor: Option<String>,
    pub queries: Option<Vec<String>>,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub before_context: usize,
    #[serde(default)]
    pub after_context: usize,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

impl ReadAttachment {
    pub fn request(&self) -> desk_diagnose_core::conversation_attachment::read::ReadRequest {
        use desk_diagnose_core::conversation_attachment::read::{ReadMode, ReadRequest};
        ReadRequest {
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
            cursor: self.cursor.clone(),
            limit: 1000,
            max_bytes: desk_diagnose_core::conversation_attachment::MAX_PAGE_BYTES,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AttachmentLineDto {
    pub line: usize,
    pub byte_offset_in_line: usize,
    pub text: String,
    pub matched_queries: Vec<usize>,
    pub context: bool,
    pub line_complete: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AttachmentPageDto {
    pub attachment_id: String,
    pub queries: Vec<String>,
    pub lines: Vec<AttachmentLineDto>,
    pub body_bytes: usize,
    pub has_more: bool,
    pub cursor: Option<String>,
    pub storage_truncated: bool,
    pub json_fragment: bool,
}

impl From<desk_diagnose_core::conversation_attachment::read::ReadPage> for AttachmentPageDto {
    fn from(page: desk_diagnose_core::conversation_attachment::read::ReadPage) -> Self {
        Self {
            attachment_id: page.attachment_id,
            queries: page.queries,
            lines: page
                .lines
                .into_iter()
                .map(|line| AttachmentLineDto {
                    line: line.line,
                    byte_offset_in_line: line.byte_offset_in_line,
                    text: line.text,
                    matched_queries: line.matched_queries,
                    context: line.context,
                    line_complete: line.line_complete,
                })
                .collect(),
            body_bytes: page.body_bytes,
            has_more: page.has_more,
            cursor: page.cursor,
            storage_truncated: page.storage_truncated,
            json_fragment: page.json_fragment,
        }
    }
}
