//! Exact model-neutral framing and contiguous, whole-source response selection.
//! These helpers size already authorized data; they grant no source permission.

use super::{
    history::serialized_tool_response_bytes, retrieval::MAX_INDEX_FRAGMENT_BYTES,
    retrieval_state::QueryCursor,
};
use desk_agent_protocol::ai_assistant::{
    history::{ConversationReadResult, ConversationSearchResult},
    memory::{MemoryHistoryResult, MemorySearchResult},
    source::validate_source_id,
};
use serde::Serialize;
use std::ops::Range;

pub const RESPONSE_BYTE_LIMIT_REASON: &str = "response_byte_limit";

pub struct ToolReplyBudget<'a> {
    call_id: &'a str,
    maximum: u32,
}
pub struct FramedToolReply {
    pub content: String,
    pub serialized_bytes: u32,
}
impl<'a> ToolReplyBudget<'a> {
    pub fn new(call_id: &'a str, maximum: u32) -> Result<Self, &'static str> {
        validate_source_id(call_id)?;
        if maximum == 0 {
            return Err("invalid assistant response byte limit");
        }
        Ok(Self { call_id, maximum })
    }
    pub fn try_encode(
        &self,
        value: &impl Serialize,
    ) -> Result<Option<FramedToolReply>, &'static str> {
        struct Writer {
            bytes: Vec<u8>,
            maximum: usize,
            exceeded: bool,
        }
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self
                    .bytes
                    .len()
                    .checked_add(bytes.len())
                    .is_none_or(|size| size > self.maximum)
                {
                    self.exceeded = true;
                    return Err(std::io::Error::other(
                        "assistant response exceeds its byte limit",
                    ));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer {
            bytes: Vec::new(),
            maximum: self.maximum as usize,
            exceeded: false,
        };
        if serde_json::to_writer(&mut writer, value).is_err() {
            return if writer.exceeded {
                Ok(None)
            } else {
                Err("invalid assistant response encoding")
            };
        }
        let content =
            String::from_utf8(writer.bytes).map_err(|_| "invalid assistant response text")?;
        let serialized_bytes = serialized_tool_response_bytes(self.call_id, &content, None)?;
        Ok(
            (serialized_bytes <= self.maximum).then_some(FramedToolReply {
                content,
                serialized_bytes,
            }),
        )
    }
    pub fn encode(&self, value: &impl Serialize) -> Result<FramedToolReply, &'static str> {
        self.try_encode(value)?
            .ok_or("assistant response exceeds its framed byte limit")
    }
    /// The byte count is part of the response being measured. Re-encode until
    /// its decimal width and the complete escaped tool framing agree exactly.
    pub fn try_encode_window(
        &self,
        value: &mut ConversationReadResult,
    ) -> Result<Option<FramedToolReply>, &'static str> {
        value.actual_bytes = 0;
        for _ in 0..12 {
            let Some(encoded) = self.try_encode(value)? else {
                return Ok(None);
            };
            if value.actual_bytes == encoded.serialized_bytes {
                return Ok(Some(encoded));
            }
            value.actual_bytes = encoded.serialized_bytes;
        }
        Err("assistant response byte count did not converge")
    }
}

pub struct PreparedSearchReply {
    pub result: ConversationSearchResult,
    pub framed: FramedToolReply,
    /// Prefix of the original authorized candidates actually returned.
    pub selected_count: usize,
}
pub fn fit_search(
    mut result: ConversationSearchResult,
    budget: &ToolReplyBudget<'_>,
    next_token: &str,
) -> Result<PreparedSearchReply, &'static str> {
    QueryCursor::decode(next_token)?;
    validate_reasons(&result.coverage.reasons)?;
    let count = result.hits.len();
    let original_more = result.has_more;
    let original_partial = result.coverage.partial;
    let original_reasons = result.coverage.reasons.clone();
    loop {
        let omitted = result.hits.len() != count;
        result.has_more = original_more || omitted;
        result.next_cursor = result.has_more.then(|| next_token.into());
        result.coverage.partial = original_partial || omitted;
        result.coverage.reasons = reasons(&original_reasons, omitted)?;
        if let Some(framed) = budget.try_encode(&result)? {
            if count > 0 && result.hits.is_empty() {
                return Err("response budget cannot fit one search source; narrow the lookup");
            }
            let selected_count = result.hits.len();
            return Ok(PreparedSearchReply {
                result,
                framed,
                selected_count,
            });
        }
        if result.hits.pop().is_none() {
            return Err("response budget cannot fit the required search metadata");
        }
    }
}

pub struct PreparedMemorySearchReply {
    pub result: MemorySearchResult,
    pub framed: FramedToolReply,
    pub selected_count: usize,
}

/// Whole facts retain their original body, context, status and source set. Byte
/// pressure moves the omitted suffix to a cursor; it never edits saved facts.
pub fn fit_memories(
    mut result: MemorySearchResult,
    budget: &ToolReplyBudget<'_>,
    next_token: &str,
) -> Result<PreparedMemorySearchReply, &'static str> {
    QueryCursor::decode(next_token)?;
    if result
        .memories
        .iter()
        .any(|record| record.content.is_empty())
    {
        return Err("invalid original memory search candidates");
    }
    validate_reasons(&result.coverage.reasons)?;
    let count = result.memories.len();
    let original_more = result.has_more;
    let original_partial = result.coverage.partial;
    let original_reasons = result.coverage.reasons.clone();
    loop {
        let omitted = result.memories.len() != count;
        result.has_more = original_more || omitted;
        result.next_cursor = result.has_more.then(|| next_token.into());
        result.coverage.partial = original_partial || omitted;
        result.coverage.reasons = reasons(&original_reasons, omitted)?;
        if let Some(framed) = budget.try_encode(&result)? {
            if count > 0 && result.memories.is_empty() {
                return Err("response budget cannot fit one memory; narrow the lookup");
            }
            let selected_count = result.memories.len();
            return Ok(PreparedMemorySearchReply {
                result,
                framed,
                selected_count,
            });
        }
        if result.memories.pop().is_none() {
            return Err("response budget cannot fit the required memory metadata");
        }
    }
}

pub struct PreparedMemoryHistoryReply {
    pub result: MemoryHistoryResult,
    pub selected_count: usize,
}

/// Keep whole revisions and advance only beyond the prefix actually delivered.
pub fn fit_memory_history(
    mut result: MemoryHistoryResult,
    budget: &ToolReplyBudget<'_>,
    next_token: &str,
) -> Result<PreparedMemoryHistoryReply, &'static str> {
    QueryCursor::decode(next_token)?;
    let count = result.versions.len();
    let original_more = result.has_more;
    let original_partial = result.partial;
    let original_reasons = result.reasons.clone();
    validate_reasons(&original_reasons)?;
    loop {
        let omitted = result.versions.len() != count;
        result.has_more = original_more || omitted;
        result.next_cursor = result.has_more.then(|| next_token.into());
        result.partial = original_partial || omitted;
        result.reasons = reasons(&original_reasons, omitted)?;
        if budget.try_encode(&result)?.is_some() {
            if count > 0 && result.versions.is_empty() {
                return Err("response budget cannot fit one complete memory version");
            }
            return Ok(PreparedMemoryHistoryReply {
                selected_count: result.versions.len(),
                result,
            });
        }
        if result.versions.pop().is_none() {
            return Err("response budget cannot fit memory history metadata");
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum WindowLayout {
    Forward,
    Backward,
    Latest,
    Context { focus_index: usize },
}
pub struct WindowPageTokens<'a> {
    pub previous: &'a str,
    pub next: &'a str,
}
pub struct PreparedWindowReply {
    pub result: ConversationReadResult,
    pub framed: FramedToolReply,
    /// Contiguous range of the original ascending source list. Omitted sides
    /// must resume at the first/last returned position, not the scan frontier.
    pub selected: Range<usize>,
}

pub fn fit_window(
    mut result: ConversationReadResult,
    budget: &ToolReplyBudget<'_>,
    layout: WindowLayout,
    tokens: WindowPageTokens<'_>,
) -> Result<PreparedWindowReply, &'static str> {
    QueryCursor::decode(tokens.previous)?;
    QueryCursor::decode(tokens.next)?;
    if result
        .messages
        .iter()
        .any(|message| message.content.len() > MAX_INDEX_FRAGMENT_BYTES)
    {
        return Err("invalid original assistant window candidates");
    }
    validate_reasons(&result.reasons)?;
    let messages = std::mem::take(&mut result.messages);
    let count = messages.len();
    if matches!(layout, WindowLayout::Context { focus_index } if focus_index > count) {
        return Err("invalid original window focus");
    }
    let original_partial = result.partial;
    let original_reasons = result.reasons.clone();
    let previous = result.previous_cursor.is_some();
    let next = result.next_cursor.is_some();
    for length in (0..=count).rev() {
        let selected = selected_range(count, length, layout);
        let omitted = length != count;
        result.messages = messages[selected.clone()].to_vec();
        result.previous_cursor = (previous || selected.start > 0).then(|| tokens.previous.into());
        result.next_cursor = (next || selected.end < count).then(|| tokens.next.into());
        result.has_more = result.previous_cursor.is_some() || result.next_cursor.is_some();
        result.partial = original_partial || omitted;
        result.reasons = reasons(&original_reasons, omitted)?;
        if let Some(framed) = budget.try_encode_window(&mut result)? {
            if count > 0 && length == 0 {
                return Err(
                    "response budget cannot fit one original fragment; increase max_bytes within the original allowance",
                );
            }
            return Ok(PreparedWindowReply {
                result,
                framed,
                selected,
            });
        }
    }
    Err("response budget cannot fit the required original-window metadata")
}

fn selected_range(count: usize, length: usize, layout: WindowLayout) -> Range<usize> {
    let start = match layout {
        WindowLayout::Forward => 0,
        WindowLayout::Backward | WindowLayout::Latest => count - length,
        WindowLayout::Context { focus_index } if length > 0 => focus_index
            .min(count.saturating_sub(1))
            .saturating_sub((length - 1) / 2)
            .min(count - length),
        WindowLayout::Context { .. } => 0,
    };
    start..start + length
}
fn reasons(original: &[String], omitted: bool) -> Result<Vec<String>, &'static str> {
    let mut result = original.to_vec();
    if omitted {
        result.push(RESPONSE_BYTE_LIMIT_REASON.into());
    }
    result.sort();
    result.dedup();
    validate_reasons(&result)?;
    Ok(result)
}
fn validate_reasons(values: &[String]) -> Result<(), &'static str> {
    if values.iter().any(|value| {
        value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    }) {
        return Err("invalid assistant response reasons");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
