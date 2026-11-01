//! Shared scalar invariants used by both strict validation and diagnostics.
use super::{PolicyIssue, range};
use crate::ai_assistant::{
    limits::{AssistantDataLimits, AssistantProcessingLimits, RetrievalEpisodeLimits},
    user_instructions::{AssistantDataPolicy, MAX_RETRIEVAL_DOCUMENT_VISITS},
};

pub(crate) fn episode_errors(p: &RetrievalEpisodeLimits) -> Vec<PolicyIssue> {
    let mut issues = Vec::new();
    range(
        &mut issues,
        "limits.retrieval.episode.query_devices",
        u64::from(p.query_devices),
        1,
        u32::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.query_cursors",
        u64::from(p.query_cursors),
        1,
        u32::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.query_state_bytes",
        u64::from(p.query_state_bytes),
        1,
        u32::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.query_seen_sources",
        u64::from(p.query_seen_sources),
        1,
        u32::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.query_delivered_calls",
        u64::from(p.query_delivered_calls),
        1,
        u32::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.query_ttl_ms",
        p.query_ttl_ms,
        1,
        i64::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.unused_projection_ttl_ms",
        p.unused_projection_ttl_ms,
        1,
        i64::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.read_bytes",
        p.read_bytes,
        0,
        i64::MAX as u64,
    );
    range(
        &mut issues,
        "limits.retrieval.episode.response_bytes",
        u64::from(p.response_bytes),
        4,
        u32::MAX as u64,
    );
    issues
}

pub(crate) fn processing_errors(p: &AssistantProcessingLimits) -> Vec<PolicyIssue> {
    let mut issues = Vec::new();
    for (field, value) in [
        (
            "limits.processing.status_coverage",
            u64::from(p.status_coverage),
        ),
        (
            "limits.processing.native_status_rows",
            u64::from(p.native_status_rows),
        ),
        (
            "limits.processing.native_status_items",
            u64::from(p.native_status_items),
        ),
        (
            "limits.processing.native_status_model_items",
            u64::from(p.native_status_model_items),
        ),
        (
            "limits.processing.native_status_source_refs",
            u64::from(p.native_status_source_refs),
        ),
        (
            "limits.processing.output_gc_actor_batch_size",
            u64::from(p.output_gc_actor_batch_size),
        ),
        (
            "limits.processing.output_gc_batch_size",
            u64::from(p.output_gc_batch_size),
        ),
        (
            "limits.processing.native_retention_page_size",
            u64::from(p.native_retention_page_size),
        ),
        (
            "limits.processing.history_capture_batch_size",
            u64::from(p.history_capture_batch_size),
        ),
        (
            "limits.processing.history_scan_page_size",
            u64::from(p.history_scan_page_size),
        ),
        (
            "limits.processing.current_memory_page_size",
            u64::from(p.current_memory_page_size),
        ),
    ] {
        range(&mut issues, field, value, 1, u32::MAX as u64);
    }
    range(
        &mut issues,
        "limits.processing.output_gc_interval_ms",
        p.output_gc_interval_ms,
        1,
        i64::MAX as u64,
    );
    issues
}

pub(crate) fn limits_errors(p: &AssistantDataLimits) -> Vec<PolicyIssue> {
    let mut issues = episode_errors(&p.retrieval.episode);
    issues.extend(processing_errors(&p.processing));
    for (field, value) in [
        (
            "limits.conversation.question_bytes",
            u64::from(p.conversation.question_bytes),
        ),
        (
            "limits.conversation.page_bytes",
            u64::from(p.conversation.page_bytes),
        ),
        (
            "limits.conversation.citations",
            u64::from(p.conversation.citations),
        ),
        (
            "limits.conversation.citation_bytes",
            u64::from(p.conversation.citation_bytes),
        ),
        (
            "limits.conversation.citation_page_answers",
            u64::from(p.conversation.citation_page_answers),
        ),
        (
            "limits.retrieval.snippet_bytes",
            u64::from(p.retrieval.snippet_bytes),
        ),
        ("limits.memory.draft_bytes", u64::from(p.memory.draft_bytes)),
        ("limits.memory.source_refs", u64::from(p.memory.source_refs)),
        (
            "limits.memory.current_facts",
            u64::from(p.memory.current_facts),
        ),
        (
            "limits.memory.user_instructions_bytes",
            u64::from(p.memory.user_instructions_bytes),
        ),
        (
            "limits.retrieval.search_window_days",
            u64::from(p.retrieval.search_window_days),
        ),
        (
            "limits.retrieval.query_groups",
            u64::from(p.retrieval.query_groups),
        ),
        (
            "limits.retrieval.query_chars",
            u64::from(p.retrieval.query_chars),
        ),
        (
            "limits.retrieval.relevance_hits",
            u64::from(p.retrieval.relevance_hits),
        ),
        (
            "limits.retrieval.time_hits",
            u64::from(p.retrieval.time_hits),
        ),
        (
            "limits.retrieval.read_messages",
            u64::from(p.retrieval.read_messages),
        ),
        (
            "limits.retrieval.search_conversations",
            u64::from(p.retrieval.search_conversations),
        ),
        (
            "limits.retrieval.query_candidates",
            u64::from(p.retrieval.query_candidates),
        ),
        (
            "limits.retrieval.relevance_roots",
            u64::from(p.retrieval.relevance_roots),
        ),
        (
            "limits.retrieval.fragments_per_root",
            u64::from(p.retrieval.fragments_per_root),
        ),
        (
            "limits.retrieval.owner_scan_documents",
            u64::from(p.retrieval.owner_scan_documents),
        ),
    ] {
        range(&mut issues, field, value, 1, u32::MAX as u64);
    }
    for (field, value) in [
        (
            "limits.retrieval.history_keyword_scan_ms",
            p.retrieval.history_keyword_scan_ms,
        ),
        (
            "limits.retrieval.memory_keyword_scan_ms",
            p.retrieval.memory_keyword_scan_ms,
        ),
        (
            "limits.retrieval.history_time_scan_ms",
            p.retrieval.history_time_scan_ms,
        ),
        (
            "limits.retrieval.current_memory_selection_ms",
            p.retrieval.current_memory_selection_ms,
        ),
        ("limits.retrieval.owner_scan_ms", p.retrieval.owner_scan_ms),
    ] {
        range(&mut issues, field, value, 1, i64::MAX as u64);
    }
    issues
}

pub fn policy_errors(p: &AssistantDataPolicy) -> Vec<PolicyIssue> {
    let mut issues = limits_errors(&p.limits);
    for (field, value, minimum, maximum) in [
        (
            "conversation_title_max_chars",
            u64::from(p.conversation_title_max_chars),
            1,
            u32::MAX as u64,
        ),
        ("revision", p.revision, 1, u64::MAX),
        (
            "history_retention_days",
            u64::from(p.history_retention_days),
            1,
            3650,
        ),
        (
            "old_memory_revision_days",
            u64::from(p.old_memory_revision_days),
            1,
            365,
        ),
        (
            "max_memory_revisions_per_fact",
            u64::from(p.max_memory_revisions_per_fact),
            1,
            100,
        ),
        (
            "max_history_records",
            p.max_history_records,
            1,
            i64::MAX as u64,
        ),
        (
            "max_history_body_bytes",
            p.max_history_body_bytes,
            1,
            i64::MAX as u64,
        ),
        (
            "max_memory_records",
            p.max_memory_records,
            1,
            i64::MAX as u64,
        ),
        (
            "max_memory_body_bytes",
            p.max_memory_body_bytes,
            1,
            i64::MAX as u64,
        ),
        ("max_term_records", p.max_term_records, 1, i64::MAX as u64),
        (
            "max_metadata_bytes",
            p.max_metadata_bytes,
            8192,
            i64::MAX as u64,
        ),
        (
            "max_retrieval_document_visits",
            u64::from(p.max_retrieval_document_visits),
            40,
            u64::from(MAX_RETRIEVAL_DOCUMENT_VISITS),
        ),
    ] {
        range(&mut issues, field, value, minimum, maximum);
    }
    issues
}
