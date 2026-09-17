//! Bounded reads over an already-authorized immutable model-visible attachment.
use super::{AttachmentMetadata, ContentKind, MAX_PAGE_BYTES, digest, invalid, utf8_end};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use desk_agent_protocol::AgentError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_PAGE_METADATA_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadMode {
    Read {
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    Search {
        queries: Vec<String>,
        ignore_case: bool,
        before_context: usize,
        after_context: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub attachment_id: String,
    pub selection: ReadMode,
    pub limit: usize,
    pub max_bytes: usize,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadLine {
    pub line: usize,
    pub byte_offset_in_line: usize,
    pub text: String,
    /// Zero-based indices into ReadPage::queries.
    pub matched_queries: Vec<usize>,
    pub context: bool,
    pub line_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadPage {
    pub attachment_id: String,
    pub queries: Vec<String>,
    pub lines: Vec<ReadLine>,
    pub body_bytes: usize,
    pub has_more: bool,
    pub cursor: Option<String>,
    pub storage_truncated: bool,
    pub json_fragment: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    binding: String,
    index: usize,
    byte_offset: usize,
}

pub fn read_page(
    metadata: &AttachmentMetadata,
    bytes: &[u8],
    request: &ReadRequest,
) -> Result<ReadPage, AgentError> {
    metadata.verify(bytes)?;
    if request.attachment_id != metadata.attachment_id
        || request.max_bytes < 4
        || request.max_bytes > MAX_PAGE_BYTES
        || request.limit == 0
        || request.limit > 1000
    {
        return Err(invalid("Invalid attachment read budget or identity"));
    }
    match metadata.kind {
        ContentKind::Image => return Err(invalid("Image attachments require image reading")),
        ContentKind::Json => {
            if !matches!(request.selection, ReadMode::Read { .. }) {
                return Err(invalid(
                    "JSON attachments cannot be searched; narrow the source tool's query instead",
                ));
            }
        }
        ContentKind::Text => {}
    }
    let content = std::str::from_utf8(bytes).map_err(|_| invalid("Invalid attachment encoding"))?;
    // Keep terminators so concatenating a complete range reproduces its bytes.
    let lines = content.split_inclusive('\n').collect::<Vec<_>>();
    let (selected, hits) = select_lines(&lines, &request.selection)?;
    let binding = digest(
        &serde_json::to_vec(&(
            &metadata.attachment_id,
            &metadata.sha256,
            &request.selection,
            request.limit,
        ))
        .map_err(|_| invalid("Cannot encode attachment query"))?,
    );
    let mut position = match &request.cursor {
        None => Cursor {
            binding: binding.clone(),
            index: 0,
            byte_offset: 0,
        },
        Some(encoded) => {
            if encoded.len() > 1024 {
                return Err(invalid("Invalid attachment cursor"));
            }
            let decoded = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| invalid("Invalid attachment cursor"))?;
            let cursor: Cursor = serde_json::from_slice(&decoded)
                .map_err(|_| invalid("Invalid attachment cursor"))?;
            if cursor.binding != binding || cursor.index >= selected.len() {
                return Err(invalid("Attachment cursor does not match this query"));
            }
            cursor
        }
    };
    let mut page = ReadPage {
        attachment_id: metadata.attachment_id.clone(),
        queries: match &request.selection {
            ReadMode::Read { .. } => vec![],
            ReadMode::Search { queries, .. } => queries.clone(),
        },
        lines: Vec::new(),
        body_bytes: 0,
        has_more: false,
        cursor: None,
        storage_truncated: metadata.storage_truncated,
        json_fragment: metadata.kind == ContentKind::Json,
    };
    // Reserve the cursor and outer fields independently of encoded body bytes.
    let mut metadata_bytes = serde_json::to_vec(&page)
        .map_err(|_| invalid("Cannot encode attachment page"))?
        .len()
        + 1024;
    let mut matched_lines = 0;
    while let Some(&line_index) = selected.get(position.index) {
        let line = lines[line_index];
        if position.byte_offset >= line.len() || !line.is_char_boundary(position.byte_offset) {
            return Err(invalid("Invalid attachment cursor offset"));
        }
        let hit = !hits[line_index].is_empty();
        if matched_lines >= request.limit && hit && position.byte_offset == 0 {
            break;
        }
        let rest = &line[position.byte_offset..];
        let take = utf8_end(rest, request.max_bytes - page.body_bytes);
        if take == 0 {
            break;
        }
        let complete = take == rest.len();
        let row = ReadLine {
            line: line_index + 1,
            byte_offset_in_line: position.byte_offset,
            text: rest[..take].into(),
            matched_queries: hits[line_index].clone(),
            context: matches!(request.selection, ReadMode::Search { .. }) && !hit,
            line_complete: complete,
        };
        let row_bytes = serde_json::to_vec(&row)
            .map_err(|_| invalid("Cannot encode attachment row"))?
            .len();
        let text_bytes = serde_json::to_vec(&row.text)
            .map_err(|_| invalid("Cannot encode attachment text"))?
            .len();
        let overhead = row_bytes - text_bytes + 1;
        if metadata_bytes + overhead > MAX_PAGE_METADATA_BYTES {
            break;
        }
        metadata_bytes += overhead;
        if position.byte_offset == 0 && (hit || matches!(request.selection, ReadMode::Read { .. }))
        {
            matched_lines += 1;
        }
        page.lines.push(row);
        page.body_bytes += take;
        if complete {
            position.index += 1;
            position.byte_offset = 0;
        } else {
            position.byte_offset += take;
        }
        if page.body_bytes == request.max_bytes
            || (matches!(request.selection, ReadMode::Read { .. })
                && matched_lines >= request.limit)
            || page.lines.len() >= 1000
        {
            break;
        }
    }
    page.has_more = position.index < selected.len();
    if page.has_more {
        page.cursor = Some(
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&position)
                    .map_err(|_| invalid("Cannot encode attachment cursor"))?,
            ),
        );
    }
    page.json_fragment = metadata.kind == ContentKind::Json
        && (request.cursor.is_some() || page.body_bytes != bytes.len());
    Ok(page)
}

fn select_lines(
    lines: &[&str],
    selection: &ReadMode,
) -> Result<(Vec<usize>, Vec<Vec<usize>>), AgentError> {
    let mut selected = BTreeSet::new();
    let mut hits = vec![Vec::new(); lines.len()];
    match selection {
        ReadMode::Read {
            start_line,
            end_line,
        } => {
            let start = start_line.unwrap_or(1);
            let end = end_line.unwrap_or(lines.len());
            if start == 0 || (end_line.is_some() && end < start) {
                return Err(invalid("Invalid attachment line range"));
            }
            selected.extend(start.saturating_sub(1).min(lines.len())..end.min(lines.len()));
        }
        ReadMode::Search {
            queries,
            ignore_case,
            before_context,
            after_context,
        } => {
            if queries.is_empty()
                || queries.len() > 16
                || serde_json::to_vec(queries)
                    .map_err(|_| invalid("Invalid attachment text search"))?
                    .len()
                    > 4096
                || queries.iter().any(|query| {
                    query.trim().is_empty() || query.len() > 256 || query.contains(['\r', '\n'])
                })
                || *before_context > 20
                || *after_context > 20
            {
                return Err(invalid("Invalid attachment text search"));
            }
            let needles = queries
                .iter()
                .map(|q| {
                    if *ignore_case {
                        q.to_lowercase()
                    } else {
                        q.clone()
                    }
                })
                .collect::<Vec<_>>();
            for (index, line) in lines.iter().enumerate() {
                let haystack = if *ignore_case {
                    line.to_lowercase()
                } else {
                    (*line).into()
                };
                for (query_index, needle) in needles.iter().enumerate() {
                    if haystack.contains(needle) {
                        hits[index].push(query_index);
                    }
                }
                if !hits[index].is_empty() {
                    selected.extend(
                        index.saturating_sub(*before_context)
                            ..index
                                .saturating_add(*after_context)
                                .saturating_add(1)
                                .min(lines.len()),
                    );
                }
            }
        }
    }
    Ok((selected.into_iter().collect(), hits))
}
