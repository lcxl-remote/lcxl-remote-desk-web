//! A framed result must name exactly the roots retained by its read proof.
//! Original source policy/native verification is performed by the host first.

use crate::ai_assistant::{
    data_tools::ParsedDataCall, history::serialized_tool_response_bytes, seam::AssistantDataReadUse,
};
use desk_agent_protocol::ai_assistant::{
    history::{ConversationReadResult, ConversationSearchResult},
    memory::{MemoryHistoryResult, MemorySearchResult, MemoryStatus},
    source::{AssistantSourceResolution, SourceAvailability},
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(super) fn validate(
    request: &ParsedDataCall,
    content: &str,
    usage: &AssistantDataReadUse,
    call_id: &str,
) -> Result<(), &'static str> {
    let mut returned = BTreeSet::new();
    let mut returned_memories = BTreeSet::new();
    if !matches!(request, ParsedDataCall::ReadConversation(_)) && !usage.decision_roots.is_empty() {
        return Err("only an original conversation window may have a decision anchor");
    }
    match request {
        ParsedDataCall::SearchConversations(request) => {
            let result: ConversationSearchResult = serde_json::from_str(content)
                .map_err(|_| "invalid stored conversation-search result")?;
            if result.hits.len() > request.limit as usize
                || result.scope != request.scope
                || result.has_more != result.next_cursor.is_some()
            {
                return Err("search result changed its original bounds");
            }
            for hit in result.hits {
                add(
                    usage,
                    &mut returned,
                    &hit.source_id,
                    hit.revision,
                    Some(&hit.locator.content_sha256),
                )?;
                if let Some(summary) = hit.native_status_summary {
                    add(
                        usage,
                        &mut returned,
                        &summary.source_id,
                        summary.source_revision,
                        None,
                    )?;
                    let mut native_sources = BTreeSet::from([summary.source_id.clone()]);
                    for source in summary.additional_sources {
                        if source.kind != desk_agent_protocol::ai_assistant::source::AssistantSourceKind::NativeStatus
                            || source.role != desk_agent_protocol::ai_assistant::source::SourceRefRole::Evidence
                            || !native_sources.insert(source.id.clone()) { return Err("invalid native status source family"); }
                        add(
                            usage,
                            &mut returned,
                            &source.id,
                            source
                                .revision
                                .ok_or("native status source revision is missing")?,
                            None,
                        )?;
                    }
                    for state in summary.items {
                        if !native_sources.contains(&state.source_id) {
                            return Err("native state replaced its original inventory source");
                        }
                        add_by_id(usage, &mut returned, &state.source_id)?;
                    }
                }
            }
        }
        ParsedDataCall::ReadConversation(request) => {
            if usage.decision_roots.len() != 1
                || usage.decision_roots[0].source_id != request.source_id
            {
                return Err("window decision anchor changed its original source");
            }
            let result: ConversationReadResult = serde_json::from_str(content)
                .map_err(|_| "invalid stored conversation-read result")?;
            if result.messages.len() > request.limit as usize
                || result.actual_bytes != serialized_tool_response_bytes(call_id, content, None)?
                || result.has_more
                    != (result.previous_cursor.is_some() || result.next_cursor.is_some())
            {
                return Err("read result changed its original count, framing or continuations");
            }
            for message in result.messages {
                add(
                    usage,
                    &mut returned,
                    &message.source_id,
                    message.revision,
                    Some(&format!("{:x}", Sha256::digest(message.content.as_bytes()))),
                )?;
            }
            for state in result.related_results {
                add_by_id(usage, &mut returned, &state.source_id)?;
            }
        }
        ParsedDataCall::SearchMemories(request) => {
            let result: MemorySearchResult =
                serde_json::from_str(content).map_err(|_| "invalid stored memory-search result")?;
            if result.memories.len() > request.limit as usize
                || result.memory_state_generation != usage.memory_state_generation
                || result.has_more != result.next_cursor.is_some()
            {
                return Err("memory result changed its original bounds");
            }
            for memory in result.memories {
                if !returned_memories.insert((memory.memory_id.clone(), memory.revision)) {
                    return Err("memory result contains duplicate versions");
                }
                if memory.source_ids.is_empty() {
                    return Err("memory result has no bounded retained evidence");
                }
                for source in memory.source_ids {
                    add_by_id(usage, &mut returned, &source)?;
                }
            }
        }
        ParsedDataCall::ReadMemoryHistory(request) => {
            let result: MemoryHistoryResult = serde_json::from_str(content)
                .map_err(|_| "invalid stored memory-history result")?;
            if result.memory_id != request.memory_id
                || result.current_revision == 0
                || usage.memories.iter().any(|memory| memory.purpose != desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose::HistoricalReference)
                || result.versions.len() > request.limit as usize
                || result.has_more != result.next_cursor.is_some()
                || result.versions.windows(2).any(|pair| pair[0].revision <= pair[1].revision)
            {
                return Err("memory history changed its original bounds");
            }
            for version in result.versions {
                if version.revision == 0
                    || version.revision > result.current_revision
                    || version.content.is_empty()
                    || version.recorded_at_unix_ms == 0
                    || version.status == MemoryStatus::Forgotten
                    || version.source_ids.is_empty()
                {
                    return Err("invalid retained memory history version");
                }
                returned_memories.insert((result.memory_id.clone(), version.revision));
                for source in version.source_ids {
                    add(usage, &mut returned, &source, version.revision, None)?;
                }
            }
        }
        ParsedDataCall::ReadMemorySource { source_id, .. } => {
            let result: AssistantSourceResolution =
                serde_json::from_str(content).map_err(|_| "invalid stored memory-source result")?;
            if result.source_id != *source_id {
                return Err("source result replaced its original source");
            }
            if let Some(body) = result.content {
                if result.availability == SourceAvailability::Unavailable {
                    return Err("unavailable source result contains text");
                }
                add(
                    usage,
                    &mut returned,
                    &result.source_id,
                    result.revision,
                    Some(&format!("{:x}", Sha256::digest(body.as_bytes()))),
                )?;
                // A source-only view binds its fact version without duplicating
                // the fact body in the source-resolution DTO.
                returned_memories.extend(
                    usage
                        .memories
                        .iter()
                        .map(|memory| (memory.memory_id.clone(), memory.revision)),
                );
            } else if result.availability != SourceAvailability::Unavailable
                || result.revision != 0
                || result.locator.is_some()
                || result.source_device.is_some()
                || result.recorded_at_unix_ms.is_some()
                || result.statement_timezone.is_some()
                || !usage.memories.is_empty()
            {
                return Err("unavailable source result contains retained metadata");
            }
        }
        _ => return Err("maintenance output cannot impersonate a retrieval result"),
    }
    let roots = usage
        .roots
        .iter()
        .map(|root| (root.source_id.clone(), root.source_revision))
        .collect::<BTreeSet<_>>();
    let memories = usage
        .memories
        .iter()
        .map(|memory| (memory.memory_id.clone(), memory.revision))
        .collect::<BTreeSet<_>>();
    if returned != roots || returned_memories != memories {
        return Err("result does not match its complete actual source roots");
    }
    Ok(())
}

fn add(
    usage: &AssistantDataReadUse,
    returned: &mut BTreeSet<(String, u64)>,
    id: &str,
    revision: u64,
    hash: Option<&str>,
) -> Result<(), &'static str> {
    if !usage
        .roots
        .iter()
        .any(|root| root.source_id == id && root.source_revision == revision)
        || !usage.history_dependencies.iter().any(|source| {
            source.source_id == id
                && source.source_revision == revision
                && hash.is_none_or(|hash| hash == source.content_sha256)
        })
    {
        return Err("result names an unretained source or different source body");
    }
    returned.insert((id.into(), revision));
    Ok(())
}
fn add_by_id(
    usage: &AssistantDataReadUse,
    returned: &mut BTreeSet<(String, u64)>,
    id: &str,
) -> Result<(), &'static str> {
    let mut matches = usage.roots.iter().filter(|root| root.source_id == id);
    let root = matches.next().ok_or("result names an unretained source")?;
    if matches.next().is_some() {
        return Err("source result has an ambiguous retained revision");
    }
    add(usage, returned, id, root.source_revision, None)
}
