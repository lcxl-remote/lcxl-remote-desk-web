//! Shared query fingerprints and opaque server-stored continuations.

use super::retrieval::query_groups;
use crate::native_ids::{self, Kind};
use desk_agent_protocol::ai_assistant::{
    history::ConversationSearchRequest, memory::MemorySearchRequest, source::SourceReadPurpose,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySourceVersion {
    pub source_id: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PageAnchor {
    /// A bounded refresh starts again and skips already returned source versions.
    Start,
    Relevance {
        next_offset: u32,
    },
    /// `history_id` is the history row key that breaks ties at one time.
    Time {
        time_unix_ms: u64,
        history_id: i64,
    },
    MemoryRevision {
        before_revision: u64,
    },
    Window {
        admitted_generation: u64,
        fragment_index: u32,
        history_id: i64,
        forward: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageContinuation {
    pub token: String,
    pub anchor: PageAnchor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalQuerySnapshot {
    pub limits: desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits,
    pub query_id: String,
    pub actor_id: String,
    pub root_conversation_id: String,
    pub retrieval_episode_id: String,
    pub purpose: SourceReadPurpose,
    pub request_sha256: String,
    pub device_refs: Vec<String>,
    pub corpus_generation: u64,
    pub memory_generation: u64,
    pub control_generation: u64,
    pub policy_revision: u64,
    pub captured_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub candidates: Vec<QuerySourceVersion>,
    pub continuations: Vec<PageContinuation>,
}

impl RetrievalQuerySnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.limits.validate()?;
        if !native_ids::is(Kind::AssistantRetrievalQuery, &self.query_id)
            || self.request_sha256.len() != 64
            || !self.request_sha256.bytes().all(|v| v.is_ascii_hexdigit())
            || self.control_generation == 0
            || self.policy_revision == 0
            || self.memory_generation == 0
            || [
                self.control_generation,
                self.policy_revision,
                self.memory_generation,
                self.corpus_generation,
                self.captured_at_unix_ms,
                self.expires_at_unix_ms,
            ]
            .iter()
            .any(|value| *value > i64::MAX as u64)
            || self.captured_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.captured_at_unix_ms
            || self.device_refs.len() > self.limits.query_devices as usize
            || self.continuations.len() > self.limits.query_cursors as usize
        {
            return Err("invalid assistant retrieval snapshot");
        }
        for id in [
            &self.actor_id,
            &self.root_conversation_id,
            &self.retrieval_episode_id,
        ] {
            desk_agent_protocol::ai_assistant::source::validate_source_id(id)?;
        }
        let mut devices = BTreeSet::new();
        for id in &self.device_refs {
            desk_agent_protocol::ai_assistant::source::validate_source_id(id)?;
            if !devices.insert(id) {
                return Err("duplicate retrieval device binding");
            }
        }
        let mut sources = BTreeSet::new();
        for source in &self.candidates {
            desk_agent_protocol::ai_assistant::source::validate_source_id(&source.source_id)?;
            if !sources.insert((&source.source_id, source.revision))
                || source.revision == 0
                || source.revision > i64::MAX as u64
            {
                return Err("invalid retrieval candidate versions");
            }
        }
        let mut tokens = BTreeSet::new();
        for cursor in &self.continuations {
            if !native_ids::is(Kind::AssistantPageCursor, &cursor.token)
                || !tokens.insert(&cursor.token)
            {
                return Err("invalid retrieval continuation");
            }
            match &cursor.anchor {
                PageAnchor::Relevance { next_offset }
                    if *next_offset as usize > self.candidates.len() =>
                {
                    return Err("invalid retrieval candidate position");
                }
                PageAnchor::Time {
                    time_unix_ms,
                    history_id,
                } => {
                    if *time_unix_ms == 0 || *time_unix_ms > i64::MAX as u64 || *history_id <= 0 {
                        return Err("invalid retrieval time position");
                    }
                }
                PageAnchor::MemoryRevision { before_revision }
                    if *before_revision == 0 || *before_revision > i64::MAX as u64 =>
                {
                    return Err("invalid memory revision position");
                }
                PageAnchor::Window {
                    admitted_generation,
                    history_id,
                    ..
                } => {
                    if *admitted_generation == 0
                        || *admitted_generation > self.corpus_generation
                        || *history_id <= 0
                    {
                        return Err("invalid retrieval window position");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn continuation(
        &self,
        token: &str,
        actor: &str,
        root: &str,
        episode: &str,
        purpose: SourceReadPurpose,
        now: u64,
    ) -> Result<&PageAnchor, &'static str> {
        self.validate()?;
        if now == 0
            || now > i64::MAX as u64
            || self.actor_id != actor
            || self.root_conversation_id != root
            || self.retrieval_episode_id != episode
            || self.purpose != purpose
            || self.expires_at_unix_ms <= now
        {
            return Err("retrieval continuation no longer matches its original scope");
        }
        self.continuations
            .iter()
            .find(|cursor| cursor.token == token)
            .map(|cursor| &cursor.anchor)
            .ok_or("unknown retrieval continuation")
    }

    /// The host generates the random native token and persists the mapping before replying.
    pub fn add_continuation(
        &mut self,
        token: String,
        anchor: PageAnchor,
    ) -> Result<(), &'static str> {
        if !native_ids::is(Kind::AssistantPageCursor, &token) {
            return Err("invalid retrieval continuation identity");
        }
        if let Some(existing) = self
            .continuations
            .iter()
            .find(|cursor| cursor.token == token)
        {
            return if existing.anchor == anchor {
                Ok(())
            } else {
                Err("retrieval continuation identity conflict")
            };
        }
        if self.continuations.len() >= self.limits.query_cursors as usize {
            return Err("retrieval continuation limit reached");
        }
        let candidate = PageContinuation { token, anchor };
        self.continuations.push(candidate);
        if let Err(error) = self.validate() {
            self.continuations.pop();
            return Err(error);
        }
        Ok(())
    }

    pub fn current_device_intersection(&self, current: &BTreeSet<String>) -> (Vec<String>, bool) {
        let devices = self
            .device_refs
            .iter()
            .filter(|id| current.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        let changed = devices.len() != self.device_refs.len();
        (devices, changed)
    }
}

pub fn conversation_fingerprint(
    request: &ConversationSearchRequest,
) -> Result<String, &'static str> {
    let mut queries = query_groups(&request.queries)?;
    queries.sort();
    let mut conversations = request.conversation_ids.clone();
    conversations.sort();
    conversations.dedup();
    let mut kinds = request.source_kinds.clone();
    kinds.sort();
    kinds.dedup();
    digest(&(
        "assistant-history-query-v1",
        queries,
        request.scope,
        conversations,
        &request.time_range,
        request.time_field,
        kinds,
        request.exclude_current_conversation,
        request.sort,
        request.order,
        request.limit,
    ))
}

pub fn memory_fingerprint(request: &MemorySearchRequest) -> Result<String, &'static str> {
    let mut queries = query_groups(&request.queries)?;
    queries.sort();
    digest(&(
        "assistant-memory-query-v1",
        queries,
        request.scope,
        request.limit,
    ))
}

pub fn window_fingerprint(
    request: &desk_agent_protocol::ai_assistant::history::ConversationReadRequest,
) -> Result<String, &'static str> {
    digest(&(
        "assistant-history-window-v1",
        &request.source_id,
        request.view,
        request.limit,
        request.max_bytes,
    ))
}

fn digest(value: &impl Serialize) -> Result<String, &'static str> {
    let bytes = serde_json::to_vec(value).map_err(|_| "invalid assistant query fingerprint")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> RetrievalQuerySnapshot {
        RetrievalQuerySnapshot {
            limits: Default::default(),
            query_id: native_ids::stable(Kind::AssistantRetrievalQuery, &"query"),
            actor_id: "actor".into(),
            root_conversation_id: "root".into(),
            retrieval_episode_id: "episode".into(),
            purpose: SourceReadPurpose::ModelHistory,
            request_sha256: "a".repeat(64),
            device_refs: vec!["A".into(), "B".into()],
            corpus_generation: 10,
            memory_generation: 1,
            control_generation: 1,
            policy_revision: 1,
            captured_at_unix_ms: 100,
            expires_at_unix_ms: 1000,
            candidates: Vec::new(),
            continuations: Vec::new(),
        }
    }

    #[test]
    fn device_scope_only_shrinks_on_an_existing_page() {
        let query = snapshot();
        let current = BTreeSet::from(["A".into(), "C".into()]);
        let (devices, changed) = query.current_device_intersection(&current);
        assert_eq!(devices, ["A"]);
        assert!(changed);
    }

    #[test]
    fn opaque_continuation_cannot_cross_actor_episode_or_purpose() {
        let mut query = snapshot();
        let token = native_ids::from_bytes(Kind::AssistantPageCursor, &[7; 16]);
        query
            .add_continuation(token.clone(), PageAnchor::Relevance { next_offset: 0 })
            .unwrap();
        assert!(
            query
                .continuation(
                    &token,
                    "actor",
                    "root",
                    "episode",
                    SourceReadPurpose::ModelHistory,
                    200
                )
                .is_ok()
        );
        assert!(
            query
                .continuation(
                    &token,
                    "other",
                    "root",
                    "episode",
                    SourceReadPurpose::ModelHistory,
                    200
                )
                .is_err()
        );
        assert!(
            query
                .continuation(
                    &token,
                    "actor",
                    "root",
                    "new-episode",
                    SourceReadPurpose::ModelHistory,
                    200
                )
                .is_err()
        );
        assert!(
            query
                .continuation(
                    &token,
                    "actor",
                    "root",
                    "episode",
                    SourceReadPurpose::OwnerHistory,
                    200
                )
                .is_err()
        );
        assert!(
            query
                .continuation(
                    &token,
                    "actor",
                    "root",
                    "episode",
                    SourceReadPurpose::ModelHistory,
                    1000
                )
                .is_err()
        );
    }
}
