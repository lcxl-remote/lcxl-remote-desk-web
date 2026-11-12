//! Optional personal-assistant data ports. Callers provide trusted run/call context.

use async_trait::async_trait;
use desk_agent_protocol::{
    AgentError,
    ai_assistant::{
        history::{
            ConversationReadRequest, ConversationReadResult, ConversationSearchRequest,
            ConversationSearchResult,
        },
        memory::{
            AssistantDataControlReceipt, MemoryHistoryRequest, MemoryHistoryResult, MemoryRecord,
            MemorySearchRequest, MemorySearchResult, MemoryWriteRequest, MemoryWriteResult,
        },
        source::{AssistantSourceResolution, HistoryDependency, SourceReadPurpose},
    },
    data_lineage::{DataEnvelope, DestinationIdentity},
};

use crate::{
    chat::{ChatMessage, ToolCall},
    session::PersistedAgentSession,
};

mod manifest;

/// Never deserialized from a model argument or owner-view REST response.
#[derive(Clone, Copy)]
pub struct AssistantDataCall<'a> {
    pub session: &'a PersistedAgentSession,
    pub tool_call: &'a ToolCall,
    /// Decoded once at the model-tool boundary; internal ports trust this value.
    pub arguments: &'a super::data_tools::ParsedDataCall,
    pub producer_message: &'a ChatMessage,
    pub logical_call_id: &'a str,
    /// The original session budget entry, persisted before any source query.
    /// Memory writes have no retrieval reservation and cannot create a query.
    pub retrieval_reservation: Option<&'a super::history::RetrievalReservation>,
    pub producer_output_envelope: &'a DataEnvelope,
    pub maximum_response_bytes: u32,
    pub destination: &'a DestinationIdentity,
    pub now_unix_ms: u64,
}

/// One decoded control batch and its server-owned invocation metadata.
pub struct MemoryForgetCall<'a> {
    pub producer_message_id: &'a str,
    pub targets: &'a [desk_agent_protocol::ai_assistant::memory::MemoryForgetTarget],
    pub operation_id: &'a str,
    pub destination: &'a DestinationIdentity,
    pub now_unix_ms: u64,
}

pub struct AssistantDataRead<T> {
    pub result: T,
    /// One complete aggregate result proof, never a physical send permission.
    pub projection: Option<Box<super::read_projection::ReadProjectionProof>>,
}

/// One source whose retained bytes were returned, rather than merely an input
/// used to produce those bytes. The complete source binding is in dependencies.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantReadRoot {
    pub source_id: String,
    pub source_revision: u64,
}

/// A server-stored read projection, never a physical model-send authorization.
/// The final native send transaction revalidates it and mints a fresh grant for
/// its own actual physical call. Owner-view API responses cannot create it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataReadUse {
    pub projection_id: String,
    pub source_envelope_id: String,
    pub purpose: SourceReadPurpose,
    pub retrieval_episode_id: String,
    pub scope: Box<super::data_access::AssistantScope>,
    pub policy_revision: u64,
    pub control_generation: u64,
    pub memory_state_generation: u64,
    /// Exact returned source roots, distinct from flattened producer inputs.
    pub roots: Vec<AssistantReadRoot>,
    /// A window's exact observed locator constrains the read even when its body
    /// is absent from the returned tail. It is not a returned-content root.
    pub decision_roots: Vec<AssistantReadRoot>,
    pub history_dependencies: Vec<HistoryDependency>,
    pub memories: Vec<super::data_control::CurrentMemoryUse>,
    pub expires_at_unix_ms: Option<u64>,
}

impl AssistantDataReadUse {
    pub fn validate(&self) -> Result<(), &'static str> {
        use crate::native_ids::{self, Kind};
        self.reference().validate()?;
        if self.decision_roots.len() > 1
            || (!self.decision_roots.is_empty() && self.purpose != SourceReadPurpose::ModelHistory)
        {
            return Err("invalid assistant read projection");
        }
        let mut sources = std::collections::BTreeSet::new();
        for dependency in &self.history_dependencies {
            if desk_agent_protocol::ai_assistant::source_ref::SourceRef::parse(
                &dependency.source_id,
            )
            .is_none()
                || dependency.origin_id <= 0
                || dependency.source_revision == 0
                || dependency.origin_deletion_generation == 0
                || dependency.policy_revision == 0
                || !sources.insert((&dependency.source_id, dependency.source_revision))
                || dependency.expires_at_unix_ms.is_some_and(|until| {
                    until == 0
                        || until > i64::MAX as u64
                        || self.expires_at_unix_ms.is_none_or(|expiry| expiry > until)
                })
            {
                return Err("invalid assistant read projection dependency");
            }
        }
        let mut roots = std::collections::BTreeSet::new();
        for root in &self.roots {
            if desk_agent_protocol::ai_assistant::source_ref::SourceRef::parse(&root.source_id)
                .is_none()
                || root.source_revision == 0
                || !roots.insert((&root.source_id, root.source_revision))
                || !sources.contains(&(&root.source_id, root.source_revision))
            {
                return Err("invalid assistant read source root");
            }
        }
        for root in &self.decision_roots {
            if desk_agent_protocol::ai_assistant::source_ref::SourceRef::parse(&root.source_id)
                .is_none()
                || root.source_revision == 0
                || !sources.contains(&(&root.source_id, root.source_revision))
            {
                return Err("invalid assistant read decision root");
            }
        }
        if !self.history_dependencies.is_empty()
            && self.roots.is_empty()
            && self.decision_roots.is_empty()
        {
            return Err("assistant read has no retained source root");
        }
        let mut memories = std::collections::BTreeSet::new();
        for memory in &self.memories {
            if !native_ids::is(Kind::Memory, &memory.memory_id)
                || memory.revision == 0
                || !memories.insert((&memory.memory_id, memory.revision, memory.purpose as u8))
            {
                return Err("invalid assistant read projection memory");
            }
        }
        Ok(())
    }

    pub fn reference(&self) -> AssistantDataProjectionRef {
        AssistantDataProjectionRef {
            projection_id: self.projection_id.clone(),
            source_envelope_id: self.source_envelope_id.clone(),
            purpose: self.purpose,
            retrieval_episode_id: self.retrieval_episode_id.clone(),
            scope: self.scope.clone(),
            policy_revision: self.policy_revision,
            control_generation: self.control_generation,
            memory_state_generation: self.memory_state_generation,
            expires_at_unix_ms: self.expires_at_unix_ms,
        }
    }
}

/// Session cache of a durable proof identity. Complete dependencies and memory
/// observations are loaded from the native projection store, never from this cache.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AssistantDataProjectionRef {
    pub projection_id: String,
    pub source_envelope_id: String,
    pub purpose: SourceReadPurpose,
    pub retrieval_episode_id: String,
    pub scope: Box<super::data_access::AssistantScope>,
    pub policy_revision: u64,
    pub control_generation: u64,
    pub memory_state_generation: u64,
    pub expires_at_unix_ms: Option<u64>,
}

impl AssistantDataProjectionRef {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !crate::native_ids::is(
            crate::native_ids::Kind::AssistantReadProjection,
            &self.projection_id,
        ) || self.purpose == SourceReadPurpose::OwnerHistory
            || self.policy_revision == 0
            || self.control_generation == 0
            || self.memory_state_generation == 0
            || self
                .expires_at_unix_ms
                .is_some_and(|until| until == 0 || until > i64::MAX as u64)
        {
            return Err("invalid assistant read projection reference");
        }
        desk_agent_protocol::ai_assistant::source::validate_source_id(&self.source_envelope_id)?;
        desk_agent_protocol::ai_assistant::source::validate_source_id(&self.retrieval_episode_id)
    }
}

pub struct CurrentMemoryBlock {
    pub memory_state_generation: u64,
    pub records: Vec<MemoryRecord>,
    pub source_envelopes: Vec<DataEnvelope>,
    pub uses: Vec<AssistantDataReadUse>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedAssistantEnvelope {
    pub envelope_id: String,
}

/// Host-only source manifest for an exact prepared request. It is not a grant.
/// The native model-receipt transaction checks these bindings, selected lineage,
/// current source policy and task authority before creating physical-call grants.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantModelDataManifest {
    pub actor_id: String,
    pub conversation_id: String,
    pub source_device_ref: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_memory_device_refs: Vec<String>,
    pub session_version: i64,
    pub lease_token: u64,
    pub input_revision: u64,
    pub actor_input_seq: u64,
    pub retrieval_episode_id: String,
    pub task_id: Option<String>,
    pub policy_revision: u64,

    pub memory_state_generation: u64,
    pub control_generation: u64,
    pub selected_envelopes: Vec<SelectedAssistantEnvelope>,
    pub read_projection_ids: Vec<String>,
    pub current_memories: Vec<super::data_control::CurrentMemoryUse>,
}

impl AssistantModelDataManifest {
    fn selected_memory_devices<'a>(
        data: &'a super::data_control::AssistantDataSession,
        projection_ids: &[String],
    ) -> &'a [String] {
        use desk_agent_protocol::ai_assistant::source::SourceReadPurpose;
        if data.read_uses.iter().any(|usage| {
            projection_ids.contains(&usage.projection_id)
                && matches!(
                    usage.purpose,
                    SourceReadPurpose::ModelMemory | SourceReadPurpose::MemoryEvidence
                )
        }) {
            data.request_memory_devices()
        } else {
            &[]
        }
    }

    pub fn validate_absence(request: &crate::seam::ModelRequest) -> Result<(), &'static str> {
        if request.messages.iter().any(|message| {
            message.data_envelope.as_ref().is_some_and(|envelope| {
                matches!(
                    envelope.provenance.source_provider_id.as_str(),
                    "assistant-data-source" | "assistant-conversation-snapshot"
                ) || (envelope.provenance.source_provider_id == "assistant-data-result"
                    && envelope.provenance.source_envelope_ids.len() > 1)
            })
        }) {
            return Err("assistant source content has no native request manifest");
        }
        Ok(())
    }

    #[inline(never)]
    pub fn validate_for(
        &self,
        session: &PersistedAgentSession,
        request: &crate::seam::ModelRequest,
    ) -> Result<(), &'static str> {
        use crate::native_ids::{self, Kind};
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing assistant model scope")?;
        let memory_devices = Self::selected_memory_devices(data, &self.read_projection_ids);
        if self.actor_id != session.actor_id
            || self.conversation_id != session.conversation_id
            || self.source_device_ref != data.source_device_ref
            || self.request_memory_device_refs != memory_devices
            || self.session_version != session.version
            || self.lease_token == 0
            || self.lease_token != session.lease_token
            || self.input_revision != session.input_revision
            || self.actor_input_seq != data.current_actor_input_seq
            || self.task_id != data.retrieval_task_id
            || self.retrieval_episode_id != data.retrieval_budget.episode_id
            || self.policy_revision == 0
            || self.control_generation != data.control_generation
            || self.memory_state_generation != data.memory_state_generation
        {
            return Err("assistant model manifest does not match its source session");
        }
        // A main assistant can share the delegation ledger, and a child always
        // spends its own request's. Bind that budget to this exact planner; the
        // host still validates its durable grant.
        if let Some(call) = &request.delegation_call {
            call.validate()?;
            let planner = match session.agent_role.binding() {
                None => {
                    call.task_id.is_none() && call.root_conversation_id == session.conversation_id
                }
                Some(binding) => {
                    call.task_id.as_deref() == Some(binding.task_id.as_str())
                        && call.root_conversation_id == binding.root_conversation_id
                        && call.group_id == binding.group_id
                }
            };
            if !planner
                || call.conversation_id != session.conversation_id
                || session.delegation_group_id.as_ref() != Some(&call.group_id)
                || call.input_revision != session.input_revision
                || call.control_revision != session.control_revision
                || call.planning_lease_token != Some(session.lease_token)
                || call.review_authority.is_some()
                || !matches!(
                    call.kind,
                    crate::subagent::reservation::DelegationCallKind::Model
                        | crate::subagent::reservation::DelegationCallKind::ContextSummary
                )
            {
                return Err("assistant model budget belongs to a different original planner");
            }
        }
        let mut selected = std::collections::BTreeSet::new();
        let mut labels = std::collections::BTreeMap::new();
        for message in &request.messages {
            if let Some(envelope) = &message.data_envelope {
                if envelope.provenance.source_provider_id == "assistant-conversation-snapshot"
                    && !session.conversation.iter().any(|saved| saved == message)
                {
                    return Err("selected snapshot is not its saved conversation message");
                }
                envelope
                    .validate()
                    .map_err(|_| "invalid selected assistant source envelope")?;
                if labels
                    .insert(&envelope.envelope_id, envelope)
                    .is_some_and(|old| old != envelope)
                {
                    return Err("selected source label changed under the same identity");
                }
                selected.insert(envelope.envelope_id.clone());
            }
        }
        crate::model_egress::sink_metadata_sha256(&labels)?;
        if selected.len() > crate::sink_authorizer::MAX_SINK_BYTES
            || self.selected_envelopes.len() != selected.len()
        {
            return Err("assistant selected-source manifest is incomplete");
        }
        let mut seen = std::collections::BTreeSet::new();
        for source in &self.selected_envelopes {
            if !seen.insert(&source.envelope_id) || !selected.contains(&source.envelope_id) {
                return Err("assistant selected-source manifest changed");
            }
        }
        let mut projections = std::collections::BTreeSet::new();
        if self
            .read_projection_ids
            .iter()
            .any(|id| !native_ids::is(Kind::AssistantReadProjection, id) || !projections.insert(id))
        {
            return Err("invalid selected assistant read projections");
        }
        let mut selected_reads = std::collections::BTreeSet::new();
        for message in &request.messages {
            let Some(envelope) = &message.data_envelope else {
                continue;
            };
            let direct_tool = message.role == crate::chat::ChatRole::Tool
                && envelope.provenance.source_provider_id == "assistant-data-result";
            let mut linked = false;
            for usage in &data.read_uses {
                if (message.role == crate::chat::ChatRole::User
                    && envelope.envelope_id == usage.source_envelope_id)
                    || (direct_tool
                        && envelope
                            .provenance
                            .source_envelope_ids
                            .contains(&usage.source_envelope_id))
                {
                    selected_reads.insert(&usage.projection_id);
                    linked = true;
                }
            }
            // A control/no-op acknowledgement may depend only on its original
            // model call. A returned source adds its own immutable read label.
            if direct_tool && envelope.provenance.source_envelope_ids.len() > 1 && !linked {
                return Err("selected assistant data result has no retained read proof");
            }
        }
        if selected_reads != projections {
            return Err("selected assistant read manifest is incomplete");
        }
        let mut memories = std::collections::BTreeSet::new();
        for memory in &self.current_memories {
            if !memories.insert((&memory.memory_id, memory.revision))
                || !data.current_memories.contains(memory)
            {
                return Err("assistant current-memory manifest changed");
            }
        }
        Ok(())
    }
}

/// Source lookup here is always model-purpose; owner UI lookup is a separate route.
#[async_trait(?Send)]
pub trait MemorySeam {
    async fn current_memories(
        &self,
        session: &PersistedAgentSession,
        destination: &DestinationIdentity,
        preparation_id: &str,
        maximum_bytes: usize,
        now_unix_ms: u64,
    ) -> Result<CurrentMemoryBlock, AgentError>;
    async fn search_memories(
        &self,
        context: AssistantDataCall<'_>,
        request: &MemorySearchRequest,
    ) -> Result<AssistantDataRead<MemorySearchResult>, AgentError>;
    async fn read_memory_history(
        &self,
        context: AssistantDataCall<'_>,
        request: &MemoryHistoryRequest,
    ) -> Result<AssistantDataRead<MemoryHistoryResult>, AgentError>;
    async fn read_memory_source(
        &self,
        context: AssistantDataCall<'_>,
        source_id: &str,
    ) -> Result<AssistantDataRead<AssistantSourceResolution>, AgentError>;
}

/// Mutation authority is independent of access to already saved facts.
#[async_trait(?Send)]
pub trait MemoryMutationSeam {
    async fn manage_memory(
        &self,
        context: AssistantDataCall<'_>,
        request: &MemoryWriteRequest,
    ) -> Result<AssistantDataRead<MemoryWriteResult>, AgentError>;
    /// The host recovers the complete original batch and native evidence from
    /// the saved producer. No model argument supplies control scope or cutoff.
    async fn forget_group(
        &self,
        session: &mut PersistedAgentSession,
        call: &MemoryForgetCall<'_>,
    ) -> Result<AssistantDataControlReceipt, AgentError>;
}

#[async_trait(?Send)]
pub trait ConversationSearchSeam {
    async fn search_conversations(
        &self,
        context: AssistantDataCall<'_>,
        request: &ConversationSearchRequest,
    ) -> Result<AssistantDataRead<ConversationSearchResult>, AgentError>;
    async fn read_conversation(
        &self,
        context: AssistantDataCall<'_>,
        request: &ConversationReadRequest,
    ) -> Result<AssistantDataRead<ConversationReadResult>, AgentError>;
}
