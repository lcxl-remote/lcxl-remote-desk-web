//! Frozen query metadata and bounded continuations. None of these values grant
//! source access, renew a retrieval episode, or authorize a native operation.

use super::history::{RetrievalCallKind, RetrievalReservation};
use super::retrieval_query::{
    PageAnchor, QuerySourceVersion, RetrievalQuerySnapshot, conversation_fingerprint,
    memory_fingerprint, window_fingerprint,
};
use crate::native_ids::{self, Kind};
use desk_agent_protocol::{
    ai_assistant::{
        history::{
            AssistantDeviceScope, ConversationReadRequest, ConversationReadView,
            ConversationSearchRequest, HistoryAccessScope, HistorySort,
        },
        memory::{MemoryHistoryRequest, MemorySearchRequest},
        source::{SourceReadPurpose, validate_optional_device_ref, validate_source_id},
    },
    data_lineage::DestinationIdentity,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Destination binds an ephemeral result set, not the ownership of stored facts.
/// Another model can start a fresh query under the same original episode budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalQueryBinding {
    pub actor_id: String,
    pub root_conversation_id: String,
    pub retrieval_episode_id: String,
    pub source_device_ref: String,
    pub task_id: Option<String>,
    pub scope_sha256: String,
    pub history_scope: Option<Box<HistoryAccessScope>>,
    /// Original memory-read upper scope, frozen for automatic tasks. Current
    /// settings must still permit use; enabling later cannot widen an old task.
    pub memory_device_rule: Option<AssistantDeviceScope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_memory_device_refs: Vec<String>,
    pub destination: DestinationIdentity,
    /// Original task authority, never a planner lease or the quota idle window.
    pub authority_expires_at_unix_ms: Option<u64>,
}

impl RetrievalQueryBinding {
    /// The host still owns all original native actor/task/version fences. This
    /// selects the matching frozen consent and the persisted logical reservation.
    pub fn for_call(
        call: &super::seam::AssistantDataCall<'_>,
        purpose: SourceReadPurpose,
    ) -> Result<Self, &'static str> {
        let data = call
            .session
            .assistant_data
            .as_ref()
            .ok_or("missing query source session")?;
        let (history_scope, memory_device_rule, deadline) = match purpose {
            SourceReadPurpose::ModelMemory
                if matches!(
                    call.tool_call.name.as_str(),
                    super::data_tools::SEARCH_MEMORIES | super::data_tools::READ_MEMORY_HISTORY
                ) =>
            {
                let scope =
                    super::memory_access::usable_memory_scope(call.session, call.now_unix_ms)
                        .ok_or("original memory scope is unavailable")?;
                (None, Some(scope.device_rule), scope.expires_at_unix_ms)
            }
            SourceReadPurpose::ModelHistory
                if matches!(
                    call.tool_call.name.as_str(),
                    super::data_tools::SEARCH_CONVERSATIONS | super::data_tools::READ_CONVERSATION
                ) =>
            {
                let scope =
                    super::data_access::usable_history_scope(call.session, call.now_unix_ms)
                        .ok_or("original history scope is unavailable")?;
                (
                    Some(Box::new(scope.clone())),
                    None,
                    scope.expires_at_unix_ms,
                )
            }
            _ => return Err("query purpose does not match its original tool"),
        };
        Ok(Self {
            actor_id: call.session.actor_id.clone(),
            root_conversation_id: call.session.conversation_id.clone(),
            retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
            source_device_ref: data.source_device_ref.clone(),
            task_id: data.retrieval_task_id.clone(),
            scope_sha256: super::data_access::scope_sha256(call.session)?,
            history_scope,
            memory_device_rule,
            request_memory_device_refs: if memory_device_rule.is_some() {
                data.request_memory_devices().to_vec()
            } else {
                Vec::new()
            },
            destination: call.destination.clone(),
            authority_expires_at_unix_ms: deadline,
        })
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [
            &self.actor_id,
            &self.root_conversation_id,
            &self.retrieval_episode_id,
        ] {
            validate_source_id(id)?;
        }
        validate_optional_device_ref(&self.source_device_ref)?;
        desk_agent_protocol::ai_assistant::validate_target_refs(
            self.request_memory_device_refs.iter().map(String::as_str),
        )?;
        if !self.request_memory_device_refs.is_empty()
            && (!self.source_device_ref.is_empty()
                || self.memory_device_rule != Some(AssistantDeviceScope::CurrentDevice)
                || self.history_scope.is_some())
        {
            return Err("retrieval query request devices do not match its original scope");
        }
        if let Some(task) = &self.task_id {
            validate_source_id(task)?;
        }
        self.destination
            .validate()
            .map_err(|_| "invalid retrieval recipient")?;
        if !matches!(self.destination, DestinationIdentity::Model { .. })
            || !digest(&self.scope_sha256)
            || (self.task_id.is_some() && self.authority_expires_at_unix_ms.is_none())
            || self
                .authority_expires_at_unix_ms
                .is_some_and(|until| until == 0 || until > i64::MAX as u64)
        {
            return Err("invalid original retrieval binding");
        }
        if let Some(scope) = &self.history_scope {
            scope.validate()?;
            if scope.actor_id != self.actor_id
                || scope.root_conversation_id != self.root_conversation_id
                || scope.current_device_ref != self.source_device_ref
                || scope.retrieval_episode_id != self.retrieval_episode_id
                || scope.task_id != self.task_id
            {
                return Err("retrieval query exceeded its original history binding");
            }
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String, &'static str> {
        self.validate()?;
        crate::model_egress::sink_metadata_sha256(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrozenRetrievalRequest {
    Conversations {
        request: ConversationSearchRequest,
    },
    Memories {
        request: MemorySearchRequest,
    },
    MemoryHistory {
        request: MemoryHistoryRequest,
        current_revision: u64,
    },
    Window {
        request: ConversationReadRequest,
        anchor: QuerySourceVersion,
        conversation_id: String,
    },
}
impl FrozenRetrievalRequest {
    pub fn without_cursor(mut self) -> Self {
        match &mut self {
            Self::Conversations { request } => {
                request.cursor = None;
                request.conversation_ids.sort();
                request.conversation_ids.dedup();
                request.source_kinds.sort();
                request.source_kinds.dedup();
            }
            Self::Memories { request } => request.cursor = None,
            Self::MemoryHistory { request, .. } => request.cursor = None,
            Self::Window { request, .. } => request.cursor = None,
        }
        self
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Conversations { request } => {
                request.validate()?;
                if request.cursor.is_some() {
                    return Err("stored query contains an external cursor");
                }
            }
            Self::Memories { request } => {
                request.validate()?;
                if request.cursor.is_some() {
                    return Err("stored query contains an external cursor");
                }
            }
            Self::MemoryHistory {
                request,
                current_revision,
            } => {
                request.validate()?;
                if request.cursor.is_some()
                    || *current_revision == 0
                    || *current_revision > i64::MAX as u64
                {
                    return Err("invalid frozen memory history binding");
                }
            }
            Self::Window {
                request,
                anchor,
                conversation_id,
            } => {
                request.validate()?;
                validate_source_id(conversation_id)?;
                validate_version(anchor)?;
                if request.cursor.is_some() || request.source_id != anchor.source_id {
                    return Err("window query changed its original source anchor");
                }
            }
        }
        Ok(())
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Conversations { request } if request.sort == HistorySort::Time => "history_time",
            Self::Conversations { .. } => "history_relevance",
            Self::Memories { .. } => "memory_relevance",
            Self::MemoryHistory { .. } => "memory_history",
            Self::Window { request, .. } if request.view == ConversationReadView::Latest => {
                "history_latest"
            }
            Self::Window { .. } => "history_context",
        }
    }
    pub fn purpose(&self) -> SourceReadPurpose {
        if matches!(self, Self::Memories { .. } | Self::MemoryHistory { .. }) {
            SourceReadPurpose::ModelMemory
        } else {
            SourceReadPurpose::ModelHistory
        }
    }
    pub fn fingerprint(&self) -> Result<String, &'static str> {
        self.validate()?;
        match self {
            Self::Conversations { request } => conversation_fingerprint(request),
            Self::Memories { request } => memory_fingerprint(request),
            Self::MemoryHistory { request, .. } => {
                let mut request = request.clone();
                request.cursor = None;
                crate::model_egress::sink_metadata_sha256(&("memory-history-v1", request))
            }
            Self::Window {
                request,
                anchor,
                conversation_id,
            } => crate::model_egress::sink_metadata_sha256(&(
                window_fingerprint(request)?,
                anchor,
                conversation_id,
            )),
        }
    }
    fn accepts_anchor(&self, anchor: &PageAnchor) -> bool {
        match anchor {
            PageAnchor::Start => true,
            PageAnchor::Relevance { .. } => {
                matches!(self, Self::Memories { .. })
                    || matches!(self, Self::Conversations { request } if request.sort == HistorySort::Relevance)
            }
            PageAnchor::Time { .. } => {
                matches!(self, Self::Conversations { request } if request.sort == HistorySort::Time)
            }
            PageAnchor::MemoryRevision { before_revision } => matches!(self,
                Self::MemoryHistory { current_revision, .. } if before_revision <= current_revision),
            PageAnchor::Window { .. } => matches!(self, Self::Window { .. }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrievalWatermark {
    pub corpus_generation: u64,
    pub memory_generation: u64,
    pub control_generation: u64,
    pub policy_revision: u64,
}
impl RetrievalWatermark {
    fn valid(&self) -> bool {
        self.corpus_generation <= i64::MAX as u64
            && self.memory_generation > 0
            && self.memory_generation <= i64::MAX as u64
            && self.control_generation > 0
            && self.control_generation <= i64::MAX as u64
            && self.policy_revision > 0
            && self.policy_revision <= i64::MAX as u64
    }
}

pub struct RetrievalSelection {
    pub devices: Vec<String>,
    pub watermark: RetrievalWatermark,
    pub candidates: Vec<QuerySourceVersion>,
    pub reasons: Vec<String>,
}

/// A query plus its exact original parameters and emitted-version ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenRetrievalQuery {
    pub snapshot: RetrievalQuerySnapshot,
    pub binding: RetrievalQueryBinding,
    pub initial_logical_call_id: String,
    pub request: FrozenRetrievalRequest,
    /// Original source admission watermark; permission refreshes cannot add
    /// later records to an already issued page or latest-observation window.
    pub source_observation_generation: u64,
    /// Immutable original source snapshot time, independent of reservation TTL
    /// and later permission refreshes. Generations define admission order.
    pub source_observed_at_unix_ms: u64,
    pub seen_sources: Vec<QuerySourceVersion>,
    /// A missing response proof cannot turn an already delivered logical call
    /// into a scan of the next page. This ledger is metadata, not a new budget.
    pub delivered_logical_call_ids: Vec<String>,
    pub reasons: Vec<String>,
    pub refreshed_at_unix_ms: u64,
}

impl FrozenRetrievalQuery {
    pub fn query_id(
        binding: &RetrievalQueryBinding,
        logical_call_id: &str,
    ) -> Result<String, &'static str> {
        binding.validate()?;
        validate_source_id(logical_call_id)?;
        Ok(native_ids::stable(
            Kind::AssistantRetrievalQuery,
            &(
                "assistant-query-v1",
                &binding.actor_id,
                &binding.root_conversation_id,
                &binding.retrieval_episode_id,
                logical_call_id,
            ),
        ))
    }
    pub fn prepare(
        binding: RetrievalQueryBinding,
        reservation: &RetrievalReservation,
        request: FrozenRetrievalRequest,
        mut selection: RetrievalSelection,
        now: u64,
        limits: desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits,
    ) -> Result<Self, &'static str> {
        binding.validate()?;
        reservation.validate()?;
        let request = request.without_cursor();
        if !selection.watermark.valid() || now == 0 || now > i64::MAX as u64 {
            return Err("invalid retrieval query watermark or time");
        }
        let kind = match &request {
            FrozenRetrievalRequest::Conversations { request } => {
                super::history::search_call_kind(request)
            }
            FrozenRetrievalRequest::Memories { .. } => RetrievalCallKind::Relevance,
            FrozenRetrievalRequest::Window { .. }
            | FrozenRetrievalRequest::MemoryHistory { .. } => RetrievalCallKind::Read,
        };
        if reservation.kind != kind || reservation.actual_bytes.is_some() {
            return Err("query has no in-flight original reservation");
        }
        let captured = reservation.reserved_at_unix_ms;
        let mut expiry = captured
            .checked_add(limits.query_ttl_ms)
            .ok_or("retrieval query deadline overflow")?;
        if let Some(until) = binding.authority_expires_at_unix_ms {
            expiry = expiry.min(until);
        }
        if let Some(until) = binding
            .history_scope
            .as_ref()
            .and_then(|scope| scope.expires_at_unix_ms)
        {
            expiry = expiry.min(until);
        }
        if expiry <= now {
            return Err("original retrieval reservation expired");
        }
        selection.devices.sort();
        selection.devices.dedup();
        selection.reasons.sort();
        selection.reasons.dedup();
        let snapshot = RetrievalQuerySnapshot {
            limits,
            query_id: Self::query_id(&binding, &reservation.logical_call_id)?,
            actor_id: binding.actor_id.clone(),
            root_conversation_id: binding.root_conversation_id.clone(),
            retrieval_episode_id: binding.retrieval_episode_id.clone(),
            purpose: request.purpose(),
            request_sha256: request.fingerprint()?,
            device_refs: selection.devices,
            corpus_generation: selection.watermark.corpus_generation,
            memory_generation: selection.watermark.memory_generation,
            control_generation: selection.watermark.control_generation,
            policy_revision: selection.watermark.policy_revision,
            captured_at_unix_ms: captured,
            expires_at_unix_ms: expiry,
            candidates: selection.candidates,
            continuations: Vec::new(),
        };
        let value = Self {
            source_observation_generation: selection.watermark.corpus_generation,
            source_observed_at_unix_ms: now,
            snapshot,
            binding,
            initial_logical_call_id: reservation.logical_call_id.clone(),
            request,
            seen_sources: Vec::new(),
            delivered_logical_call_ids: Vec::new(),
            reasons: selection.reasons,
            refreshed_at_unix_ms: now.max(captured),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_shape()?;
        self.bounded_fingerprint().map(|_| ())
    }
    fn validate_shape(&self) -> Result<(), &'static str> {
        self.snapshot.validate()?;
        self.binding.validate()?;
        self.request.validate()?;
        if self.snapshot.query_id != Self::query_id(&self.binding, &self.initial_logical_call_id)?
            || self.snapshot.actor_id != self.binding.actor_id || self.snapshot.root_conversation_id != self.binding.root_conversation_id
            || self.snapshot.retrieval_episode_id != self.binding.retrieval_episode_id
            || self.snapshot.purpose != self.request.purpose() || self.snapshot.request_sha256 != self.request.fingerprint()?
            || self.source_observation_generation > self.snapshot.corpus_generation
            || self.source_observed_at_unix_ms < self.snapshot.captured_at_unix_ms || self.source_observed_at_unix_ms >= self.snapshot.expires_at_unix_ms
            || self.source_observed_at_unix_ms > self.refreshed_at_unix_ms
            || self.refreshed_at_unix_ms < self.snapshot.captured_at_unix_ms || self.refreshed_at_unix_ms >= self.snapshot.expires_at_unix_ms
            || self.snapshot.expires_at_unix_ms > i64::MAX as u64 || !self.watermark().valid()
            || self.binding.authority_expires_at_unix_ms.is_some_and(|until| self.snapshot.expires_at_unix_ms > until)
            || self.binding.history_scope.as_ref().and_then(|scope| scope.expires_at_unix_ms).is_some_and(|until| self.snapshot.expires_at_unix_ms > until)
            || self.snapshot.continuations.iter().any(|page| !self.request.accepts_anchor(&page.anchor))
            || self.snapshot.continuations.iter().any(|page| matches!(&page.anchor,
                PageAnchor::Window { admitted_generation, .. } if *admitted_generation > self.source_observation_generation))
            || self.reasons.windows(2).any(|values| values[0] >= values[1])
            || self.reasons.iter().any(|reason| reason.is_empty() || reason.len() > 64
                || !reason.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'))
            || self.seen_sources.len() > self.snapshot.limits.query_seen_sources as usize || self.delivered_logical_call_ids.len() > self.snapshot.limits.query_delivered_calls as usize
            || self.delivered_logical_call_ids.windows(2).any(|ids| ids[0] >= ids[1]) {
            return Err("frozen retrieval query changed its original bindings");
        }
        if self.request.purpose() == SourceReadPurpose::ModelHistory
            && self.binding.history_scope.is_none()
        {
            return Err("history query has no original history authority");
        }
        if let FrozenRetrievalRequest::Conversations { request } = &self.request {
            super::history::validate_search_scope(
                self.binding
                    .history_scope
                    .as_deref()
                    .ok_or("missing history scope")?,
                request,
                self.snapshot.captured_at_unix_ms,
            )?;
        }
        let current_only = match &self.request {
            FrozenRetrievalRequest::Memories { request } => {
                let rule = self
                    .binding
                    .memory_device_rule
                    .ok_or("memory query has no original memory authority")?;
                if rule == AssistantDeviceScope::CurrentDevice
                    && request.scope == AssistantDeviceScope::AllMyDevices
                {
                    return Err("memory query expanded its original device rule");
                }
                rule == AssistantDeviceScope::CurrentDevice
                    || request.scope == AssistantDeviceScope::CurrentDevice
            }
            FrozenRetrievalRequest::MemoryHistory { .. } => {
                self.binding
                    .memory_device_rule
                    .ok_or("memory history has no original memory authority")?
                    == AssistantDeviceScope::CurrentDevice
            }
            FrozenRetrievalRequest::Conversations { request } => {
                request.scope == AssistantDeviceScope::CurrentDevice
            }
            FrozenRetrievalRequest::Window { .. } => self
                .binding
                .history_scope
                .as_ref()
                .is_some_and(|scope| scope.device_rule == AssistantDeviceScope::CurrentDevice),
        };
        if current_only
            && self.snapshot.device_refs.iter().any(|device| {
                if self.binding.request_memory_device_refs.is_empty() {
                    device != &self.binding.source_device_ref
                } else {
                    !self.binding.request_memory_device_refs.contains(device)
                }
            })
        {
            return Err("retrieval snapshot contains a device outside its query rule");
        }
        let mut seen = BTreeSet::new();
        for id in &self.delivered_logical_call_ids {
            validate_source_id(id)?;
        }
        for source in &self.seen_sources {
            validate_version(source)?;
            if !seen.insert((&source.source_id, source.revision)) {
                return Err("duplicate emitted source version");
            }
        }
        Ok(())
    }
    pub fn watermark(&self) -> RetrievalWatermark {
        RetrievalWatermark {
            corpus_generation: self.snapshot.corpus_generation,
            memory_generation: self.snapshot.memory_generation,
            control_generation: self.snapshot.control_generation,
            policy_revision: self.snapshot.policy_revision,
        }
    }
    pub fn same_initial_request(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.snapshot.limits == other.snapshot.limits
            && self.initial_logical_call_id == other.initial_logical_call_id
            && self.snapshot.query_id == other.snapshot.query_id
            && self.request.kind() == other.request.kind()
            && self.snapshot.request_sha256 == other.snapshot.request_sha256
            && match (&self.request, &other.request) {
                (
                    FrozenRetrievalRequest::MemoryHistory {
                        current_revision: left,
                        ..
                    },
                    FrozenRetrievalRequest::MemoryHistory {
                        current_revision: right,
                        ..
                    },
                ) => left == right,
                _ => true,
            }
    }
    pub fn validate_successor(&self, next: &Self) -> Result<(), &'static str> {
        self.validate()?;
        next.validate()?;
        let refreshed = self.needs_refresh(next.watermark())?
            || self.snapshot.device_refs != next.snapshot.device_refs;
        if !self.same_initial_request(next)
            || self.snapshot.captured_at_unix_ms != next.snapshot.captured_at_unix_ms
            || self.snapshot.expires_at_unix_ms != next.snapshot.expires_at_unix_ms
            || self.source_observation_generation != next.source_observation_generation
            || self.source_observed_at_unix_ms != next.source_observed_at_unix_ms
            || self
                .seen_sources
                .iter()
                .any(|source| !next.seen_sources.contains(source))
            || self
                .delivered_logical_call_ids
                .iter()
                .any(|id| !next.delivered_logical_call_ids.contains(id))
            || next
                .snapshot
                .device_refs
                .iter()
                .any(|device| !self.snapshot.device_refs.contains(device))
            || next.refreshed_at_unix_ms < self.refreshed_at_unix_ms
            || (!refreshed
                && (self.snapshot.candidates != next.snapshot.candidates
                    || self.reasons != next.reasons
                    || self.refreshed_at_unix_ms != next.refreshed_at_unix_ms))
        {
            return Err("retrieval mutation replaced its original query or emitted ledger");
        }
        for page in &self.snapshot.continuations {
            let Some(updated) = next
                .snapshot
                .continuations
                .iter()
                .find(|candidate| candidate.token == page.token)
            else {
                return Err("retrieval mutation removed an issued cursor");
            };
            if (!refreshed && updated.anchor != page.anchor)
                || (refreshed && updated.anchor != PageAnchor::Start)
            {
                return Err("retrieval mutation changed an issued page position");
            }
        }
        Ok(())
    }
    pub fn needs_refresh(&self, current: RetrievalWatermark) -> Result<bool, &'static str> {
        let old = self.watermark();
        if !current.valid()
            || current.corpus_generation < old.corpus_generation
            || current.memory_generation < old.memory_generation
            || current.control_generation < old.control_generation
            || current.policy_revision < old.policy_revision
        {
            return Err("retrieval query watermark moved backwards");
        }
        Ok(old != current)
    }
    pub fn remember(&mut self, sources: &[QuerySourceVersion]) -> Result<(), &'static str> {
        let mut next = self.clone();
        for source in sources {
            validate_version(source)?;
            if let Some(old) = next
                .seen_sources
                .iter()
                .find(|old| old.source_id == source.source_id && old.revision == source.revision)
            {
                if old != source {
                    return Err("emitted source version changed its digest");
                }
            } else {
                next.seen_sources.push(source.clone());
            }
        }
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn was_seen(&self, source: &QuerySourceVersion) -> bool {
        self.seen_sources.contains(source)
    }
    pub fn was_delivered(&self, logical_call_id: &str) -> bool {
        self.delivered_logical_call_ids
            .iter()
            .any(|id| id == logical_call_id)
    }
    pub fn mark_delivered(&mut self, logical_call_id: &str) -> Result<bool, &'static str> {
        self.validate()?;
        validate_source_id(logical_call_id)?;
        let index = match self
            .delivered_logical_call_ids
            .binary_search_by(|id| id.as_str().cmp(logical_call_id))
        {
            Ok(_) => return Ok(false),
            Err(index) => index,
        };
        let mut next = self.clone();
        next.delivered_logical_call_ids
            .insert(index, logical_call_id.into());
        next.validate()?;
        *self = next;
        Ok(true)
    }
    pub fn add_page(&mut self, token: String, anchor: PageAnchor) -> Result<String, &'static str> {
        self.validate()?;
        QueryCursor::new(&self.snapshot.query_id, &token)?;
        if !self.request.accepts_anchor(&anchor) {
            return Err("retrieval cursor changed its query mode");
        }
        if let Some(existing) = self
            .snapshot
            .continuations
            .iter()
            .find(|page| page.anchor == anchor)
        {
            return QueryCursor::new(&self.snapshot.query_id, &existing.token)
                .map(|cursor| cursor.encode());
        }
        let mut next = self.clone();
        next.snapshot.add_continuation(token.clone(), anchor)?;
        next.validate()?;
        let cursor = QueryCursor::new(&next.snapshot.query_id, &token)?;
        *self = next;
        Ok(cursor.encode())
    }
    pub fn page<'a>(
        &'a self,
        raw_cursor: &str,
        binding: &RetrievalQueryBinding,
        request: &FrozenRetrievalRequest,
        now: u64,
    ) -> Result<&'a PageAnchor, &'static str> {
        self.validate()?;
        let cursor = QueryCursor::decode(raw_cursor)?;
        if cursor.query_id != self.snapshot.query_id
            || binding != &self.binding
            || request.kind() != self.request.kind()
            || request.clone().without_cursor().fingerprint()? != self.snapshot.request_sha256
        {
            return Err("retrieval cursor changed its original query scope");
        }
        self.snapshot.continuation(
            &cursor.page_id,
            &binding.actor_id,
            &binding.root_conversation_id,
            &binding.retrieval_episode_id,
            request.purpose(),
            &self.snapshot.request_sha256,
            now,
        )
    }
    pub fn refresh(
        &mut self,
        mut selection: RetrievalSelection,
        now: u64,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let changed = self.needs_refresh(selection.watermark)?;
        if now >= self.snapshot.expires_at_unix_ms {
            return Err("retrieval query expired");
        }
        let old_devices = self.snapshot.device_refs.iter().collect::<BTreeSet<_>>();
        if selection
            .devices
            .iter()
            .any(|device| !old_devices.contains(device))
        {
            return Err("an existing query cannot add devices");
        }
        selection.devices.sort();
        selection.devices.dedup();
        if !changed && self.snapshot.device_refs == selection.devices {
            return Err("retrieval query does not require refresh");
        }
        let mut next = self.clone();
        if next.snapshot.device_refs != selection.devices {
            selection.reasons.push("scope_changed".into());
        }
        selection.reasons.push("query_refreshed".into());
        selection.reasons.sort();
        selection.reasons.dedup();
        next.snapshot.device_refs = selection.devices;
        next.snapshot.candidates = selection.candidates;
        next.snapshot.corpus_generation = selection.watermark.corpus_generation;
        next.snapshot.memory_generation = selection.watermark.memory_generation;
        next.snapshot.control_generation = selection.watermark.control_generation;
        next.snapshot.policy_revision = selection.watermark.policy_revision;
        for page in &mut next.snapshot.continuations {
            page.anchor = PageAnchor::Start;
        }
        next.refreshed_at_unix_ms = now.max(next.refreshed_at_unix_ms);
        next.reasons = selection.reasons;
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String, &'static str> {
        self.validate_shape()?;
        self.bounded_fingerprint()
    }
    pub fn encode(&self) -> Result<String, &'static str> {
        self.validate_shape()?;
        String::from_utf8(self.encode_bounded()?).map_err(|_| "invalid retrieval query encoding")
    }
    pub fn decode(raw: &str) -> Result<Self, &'static str> {
        let value: Self =
            serde_json::from_str(raw).map_err(|_| "invalid frozen retrieval query")?;
        value.validate()?;
        Ok(value)
    }
    fn bounded_fingerprint(&self) -> Result<String, &'static str> {
        struct Writer {
            bytes: usize,
            hash: Sha256,
            maximum: usize,
        }
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes = self
                    .bytes
                    .checked_add(bytes.len())
                    .filter(|size| *size <= self.maximum)
                    .ok_or_else(|| {
                        std::io::Error::other("retrieval query metadata exceeds its bound")
                    })?;
                self.hash.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer {
            bytes: 0,
            hash: Sha256::new(),
            maximum: self.snapshot.limits.query_state_bytes as usize,
        };
        serde_json::to_writer(&mut writer, self)
            .map_err(|_| "retrieval query metadata exceeds its bound")?;
        Ok(format!("{:x}", writer.hash.finalize()))
    }
    fn encode_bounded(&self) -> Result<Vec<u8>, &'static str> {
        struct Writer(Vec<u8>, usize);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self
                    .0
                    .len()
                    .checked_add(bytes.len())
                    .is_none_or(|size| size > self.1)
                {
                    return Err(std::io::Error::other(
                        "retrieval query metadata exceeds its bound",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer(Vec::new(), self.snapshot.limits.query_state_bytes as usize);
        serde_json::to_writer(&mut writer, self)
            .map_err(|_| "retrieval query metadata exceeds its bound")?;
        Ok(writer.0)
    }
}

use sha2::{Digest, Sha256};
fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn validate_version(value: &QuerySourceVersion) -> Result<(), &'static str> {
    validate_source_id(&value.source_id)?;
    if value.revision == 0 || value.revision > i64::MAX as u64 {
        return Err("invalid query source version");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryCursor {
    pub query_id: String,
    pub page_id: String,
}
impl QueryCursor {
    pub fn new(query_id: &str, page_id: &str) -> Result<Self, &'static str> {
        if !native_ids::is(Kind::AssistantRetrievalQuery, query_id)
            || !native_ids::is(Kind::AssistantPageCursor, page_id)
        {
            return Err("invalid retrieval cursor identity");
        }
        Ok(Self {
            query_id: query_id.into(),
            page_id: page_id.into(),
        })
    }
    pub fn encode(&self) -> String {
        format!("{}.{}", self.query_id, self.page_id)
    }
    pub fn decode(raw: &str) -> Result<Self, &'static str> {
        if raw.len() > 128 {
            return Err("retrieval cursor exceeds its bound");
        }
        let (query_id, page_id) = raw.split_once('.').ok_or("invalid retrieval cursor")?;
        Self::new(query_id, page_id)
    }
}

#[cfg(test)]
mod tests;
