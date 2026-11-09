//! Row keys of model-visible source ids. A malformed id or an id of another
//! table maps to 0, which names no row, so lookups keep "not found" semantics.

use desk_agent_protocol::ai_assistant::source_ref::{SourceRef, SourceTable};

fn row(table: SourceTable, source_id: &str) -> i64 {
    SourceRef::parse(source_id)
        .filter(|source| source.table == table)
        .map_or(0, |source| source.id)
}

/// `agent_assistant_history.id` of a source id.
pub fn history(source_id: impl AsRef<str>) -> i64 {
    row(SourceTable::History, source_id.as_ref())
}

/// `agent_memory_source.id` of a source id.
pub fn memory(source_id: impl AsRef<str>) -> i64 {
    row(SourceTable::Memory, source_id.as_ref())
}

/// History row keys of several source ids.
pub fn histories<I: IntoIterator<Item = S>, S: AsRef<str>>(source_ids: I) -> Vec<i64> {
    source_ids.into_iter().map(history).collect()
}

/// Memory source row keys of several source ids.
pub fn memories<I: IntoIterator<Item = S>, S: AsRef<str>>(source_ids: I) -> Vec<i64> {
    source_ids.into_iter().map(memory).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_require_the_matching_table() {
        let history =
            desk_agent_protocol::ai_assistant::source_ref::SourceRef::history(42).to_string();
        let memory =
            desk_agent_protocol::ai_assistant::source_ref::SourceRef::memory(42).to_string();
        assert_eq!(super::history(&history), 42);
        assert_eq!(super::history(&memory), 0);
        assert_eq!(super::memory(&memory), 42);
        assert_eq!(super::history("src_h42x"), 0);
        assert_eq!(super::histories([history.as_str(), "bad"]), vec![42, 0]);
    }
}
