//! Original assistant-data invocation bindings shared by both hosts.
//! Native receipt reads and current source/task permissions remain host responsibilities.

use super::{
    data_access::{AssistantDataToolAccess, assistant_scope, eligible_origin},
    data_tools::ParsedDataCall,
    history::{RetrievalCallKind, search_call_kind},
    memory_access::usable_memory_scope,
    read_projection::request_sha256,
    seam::AssistantDataCall,
};
use crate::{
    model_egress::{NativeModelOutputSources, model_output_message_envelope},
    native_ids::{self, Kind},
    session::{PersistedAgentSession, TurnState},
};
use desk_agent_protocol::{ai_assistant::source::SourceReadPurpose, data_lineage::DataEnvelope};

/// Read reservations and fact drafts share session and native-output checks.
/// Their distinct budgets and producer-time bounds stay explicit here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginalAssistantDataCall {
    Retrieval(OriginalRetrievalCall),
    MemoryWrite(OriginalMemoryMutationCall),
    MemoryControl(OriginalMemoryControlCall),
}

impl OriginalAssistantDataCall {
    pub fn retrieval(context: &AssistantDataCall<'_>, now: u64) -> Result<Self, &'static str> {
        original_retrieval(context, now).map(Self::Retrieval)
    }

    pub fn memory_write(context: &AssistantDataCall<'_>, now: u64) -> Result<Self, &'static str> {
        OriginalMemoryMutationCall::prepare_kind(context, now).map(Self::MemoryWrite)
    }

    pub fn memory_control(
        context: &AssistantDataCall<'_>,
        call: &super::seam::MemoryForgetCall<'_>,
    ) -> Result<Self, &'static str> {
        OriginalMemoryControlCall::prepare(context, call).map(Self::MemoryControl)
    }

    pub fn purpose(&self) -> SourceReadPurpose {
        match self {
            Self::Retrieval(value) => value.purpose,
            Self::MemoryWrite(_) | Self::MemoryControl(_) => SourceReadPurpose::ModelMemory,
        }
    }

    pub fn maximum_response_bytes(&self) -> u32 {
        match self {
            Self::Retrieval(value) => value.maximum_response_bytes,
            Self::MemoryWrite(value) => value.maximum_response_bytes,
            Self::MemoryControl(value) => value.invocation.maximum_response_bytes,
        }
    }

    pub fn unused_read_deadline(&self) -> Option<u64> {
        match self {
            Self::Retrieval(value) => Some(value.expires_at_unix_ms),
            Self::MemoryWrite(_) | Self::MemoryControl(_) => None,
        }
    }

    pub fn is_memory_write(&self) -> bool {
        matches!(self, Self::MemoryWrite(_))
    }

    pub fn requires_native_archive(&self) -> bool {
        !matches!(self, Self::Retrieval(_))
    }

    pub fn scope_deadline(
        &self,
        context: &AssistantDataCall<'_>,
        access: &AssistantDataToolAccess,
        now: u64,
    ) -> Result<Option<u64>, &'static str> {
        let data = context
            .session
            .assistant_data
            .as_ref()
            .ok_or("missing original assistant-data state")?;
        if data.memory_state_generation != access.memory_state_generation
            || data.control_generation != access.control_generation
        {
            return Err("assistant-data invocation requires the current reconciled source state");
        }
        match self.purpose() {
            SourceReadPurpose::ModelHistory if access.settings.model_history_search_enabled => {
                super::data_access::usable_history_scope(context.session, now)
                    .map(|scope| scope.expires_at_unix_ms)
                    .ok_or("retrieval has no original history scope")
            }
            SourceReadPurpose::ModelMemory | SourceReadPurpose::MemoryEvidence
                if access.settings.automatic_memory_enabled =>
            {
                let scope = usable_memory_scope(context.session, now)
                    .ok_or("assistant-data invocation has no original memory scope")?;
                if self.requires_native_archive() && scope.policy_revision != access.policy_revision
                {
                    return Err("memory mutation policy changed");
                }
                Ok(scope.expires_at_unix_ms)
            }
            _ => Err("assistant-data invocation is not enabled for its original purpose"),
        }
    }

    /// The host retains every original SELECT, version, lease and raw-state fence.
    pub fn persisted_producer<'a>(
        &self,
        context: &AssistantDataCall<'_>,
        persisted: &'a PersistedAgentSession,
        now: u64,
    ) -> Result<&'a DataEnvelope, &'static str> {
        self.validate_request(context, persisted)?;
        persisted_producer(context, persisted, now)
    }

    fn validate_request(
        &self,
        context: &AssistantDataCall<'_>,
        persisted: &PersistedAgentSession,
    ) -> Result<(), &'static str> {
        // This is a fresh database read boundary, not an internal hand-off.
        // Verify the stored reservation against the already decoded model call.
        let expected_logical = if let Self::Retrieval(original) = self {
            let data = persisted
                .assistant_data
                .as_ref()
                .ok_or("missing persisted retrieval state")?;
            let reservation = context
                .retrieval_reservation
                .ok_or("missing persisted retrieval reservation")?;
            if reservation.actual_bytes.is_some()
                || reservation.kind != original.kind
                || reservation.reserved_bytes != context.maximum_response_bytes
                || reservation.logical_call_id != context.logical_call_id
                || reservation.request_sha256
                    != request_sha256(&context.tool_call.name, &context.tool_call.arguments_json)?
                || !data.retrieval_budget.reservations.contains(reservation)
            {
                return Err("persisted retrieval reservation changed its original call");
            }
            native_ids::stable(
                Kind::AssistantReadAuthorization,
                &(
                    &persisted.actor_id,
                    &persisted.conversation_id,
                    &data.retrieval_budget.episode_id,
                    context.producer_message.message_id.as_str(),
                    &context.tool_call.id,
                ),
            )
        } else {
            native_ids::stable(
                Kind::AssistantReadAuthorization,
                &(
                    &persisted.conversation_id,
                    context.producer_message.message_id.as_str(),
                    &context.tool_call.id,
                ),
            )
        };
        if expected_logical != context.logical_call_id {
            return Err("persisted assistant-data call changed its logical identity");
        }
        Ok(())
    }

    /// Native acceptance does not grant current source or task access.
    pub fn accepted_producer(
        &self,
        context: &AssistantDataCall<'_>,
        sources: &NativeModelOutputSources,
        now: u64,
    ) -> Result<(), &'static str> {
        sources.validate(context.producer_output_envelope)?;
        let evidence = &sources.evidence;
        if evidence.context_conversation_id != context.session.conversation_id
            || &evidence.destination != context.destination
        {
            return Err("assistant-data producer changed its native context or destination");
        }
        match self {
            Self::Retrieval(_) => {
                let reservation = context
                    .retrieval_reservation
                    .ok_or("missing original retrieval reservation")?;
                if evidence.completed_at_unix_ms > reservation.reserved_at_unix_ms {
                    return Err("retrieval producer was not accepted before its reservation");
                }
            }
            Self::MemoryWrite(_) | Self::MemoryControl(_) => {
                let data = context
                    .session
                    .assistant_data
                    .as_ref()
                    .ok_or("missing memory mutation source session")?;
                if evidence.completed_at_unix_ms > now
                    || evidence.provider_started_at_unix_ms < data.current_input_recorded_at_unix_ms
                {
                    return Err("memory mutation changed its actual accepted producer");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginalRetrievalCall {
    pub kind: RetrievalCallKind,
    pub purpose: SourceReadPurpose,
    /// Includes the original request's narrower read limit.
    pub maximum_response_bytes: u32,
    /// An unused result cannot outlive its first durable reservation.
    pub expires_at_unix_ms: u64,
}

/// Validate the original call, without asserting that an in-memory producer is
/// itself a native accepted output. The host must establish that independently.
fn original_retrieval(
    context: &AssistantDataCall<'_>,
    now: u64,
) -> Result<OriginalRetrievalCall, &'static str> {
    let (kind, purpose, maximum) = match context.arguments {
        ParsedDataCall::SearchMemories(_) => (
            RetrievalCallKind::Relevance,
            SourceReadPurpose::ModelMemory,
            context.maximum_response_bytes,
        ),
        ParsedDataCall::ReadMemoryHistory(_) => (
            RetrievalCallKind::Read,
            SourceReadPurpose::ModelMemory,
            context.maximum_response_bytes,
        ),
        ParsedDataCall::SearchConversations(request) => (
            search_call_kind(request),
            SourceReadPurpose::ModelHistory,
            context.maximum_response_bytes,
        ),
        ParsedDataCall::ReadConversation(request) => (
            RetrievalCallKind::Read,
            SourceReadPurpose::ModelHistory,
            context.maximum_response_bytes.min(request.max_bytes),
        ),
        ParsedDataCall::ReadMemorySource { max_bytes, .. } => (
            RetrievalCallKind::Read,
            SourceReadPurpose::MemoryEvidence,
            context.maximum_response_bytes.min(*max_bytes),
        ),
        _ => return Err("original call is not a retrieval tool"),
    };
    let reservation = context
        .retrieval_reservation
        .ok_or("missing durable retrieval reservation")?;
    let limits = &context
        .session
        .assistant_data
        .as_ref()
        .ok_or("missing original retrieval budget")?
        .retrieval_budget
        .limits;
    let expires = reservation
        .reserved_at_unix_ms
        .checked_add(limits.unused_projection_ttl_ms)
        .filter(|until| *until <= i64::MAX as u64)
        .ok_or("retrieval reservation deadline overflow")?;
    if reservation.reserved_at_unix_ms > now || expires <= now {
        return Err("retrieval outlived its original reservation");
    }
    let producer = context.producer_output_envelope;
    let expires = producer
        .retention
        .expires_at_unix_ms
        .map_or(expires, |until| expires.min(until));
    Ok(OriginalRetrievalCall {
        kind,
        purpose,
        maximum_response_bytes: maximum,
        expires_at_unix_ms: expires,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalMemoryMutationCall {
    pub request_sha256: String,
    pub maximum_response_bytes: u32,
}

impl OriginalMemoryMutationCall {
    pub(crate) fn prepare_kind(
        context: &AssistantDataCall<'_>,
        _now: u64,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            request_sha256: request_sha256(
                &context.tool_call.name,
                &context.tool_call.arguments_json,
            )?,
            maximum_response_bytes: context.maximum_response_bytes,
        })
    }
}

/// Complete call-message binding only. Native acceptance, the original human
/// input and observed target versions must still be proved by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalMemoryControlCall {
    invocation: OriginalMemoryMutationCall,
    owner_input_seq: u64,
    call_message_sha256: String,
    targets: Vec<desk_agent_protocol::ai_assistant::memory::MemoryForgetTarget>,
}
impl OriginalMemoryControlCall {
    pub fn prepare(
        context: &AssistantDataCall<'_>,
        call: &super::seam::MemoryForgetCall<'_>,
    ) -> Result<Self, &'static str> {
        let invocation = OriginalMemoryMutationCall::prepare_kind(context, context.now_unix_ms)?;
        let message = context.producer_message;
        let data = context
            .session
            .assistant_data
            .as_ref()
            .ok_or("missing original memory control input")?;
        Ok(Self {
            invocation,
            owner_input_seq: data.current_actor_input_seq,
            call_message_sha256: super::data_control::tool_group_sha256(std::slice::from_ref(
                message,
            ))?,
            targets: call.targets.to_vec(),
        })
    }
    pub fn owner_input_seq(&self) -> u64 {
        self.owner_input_seq
    }
    pub fn call_message_sha256(&self) -> &str {
        &self.call_message_sha256
    }
    pub fn targets(&self) -> &[desk_agent_protocol::ai_assistant::memory::MemoryForgetTarget] {
        &self.targets
    }
}

/// Shared exact native producer identity. A read or mutation caller must also
/// validate its own original invocation and current purpose before using it.
fn persisted_producer<'a>(
    context: &AssistantDataCall<'_>,
    persisted: &'a PersistedAgentSession,
    now: u64,
) -> Result<&'a DataEnvelope, &'static str> {
    let expected = context.session;
    if !eligible_origin(persisted)
        || persisted.turn_state != TurnState::Running
        || expected.turn_state != TurnState::Running
        || persisted.actor_id != expected.actor_id
        || persisted.device_id != expected.device_id
        || persisted.conversation_id != expected.conversation_id
        || persisted.version != expected.version
        || persisted.lease_token != expected.lease_token
        || persisted.input_revision != expected.input_revision
        || persisted.control_revision != expected.control_revision
        || persisted.current_turn_id != expected.current_turn_id
        || persisted.trigger_origin != expected.trigger_origin
        || persisted.focus_epoch != expected.focus_epoch
        || persisted.agent_role != expected.agent_role
        || persisted.assistant_data != expected.assistant_data
        || assistant_scope(persisted)? != assistant_scope(expected)?
    {
        return Err("original persisted assistant-data session changed");
    }
    producer_on(context, persisted, now)
}

fn producer_on<'a>(
    context: &AssistantDataCall<'_>,
    session: &'a PersistedAgentSession,
    now: u64,
) -> Result<&'a DataEnvelope, &'static str> {
    let mut messages = session
        .conversation
        .iter()
        .filter(|message| message.message_id == context.producer_message.message_id);
    let producer = messages
        .next()
        .ok_or("original assistant-data producer is unavailable")?;
    if messages.next().is_some() {
        return Err("ambiguous original assistant-data producer");
    }
    let envelope = model_output_message_envelope(producer)
        .map_err(|_| "invalid original assistant-data producer payload")?;
    if envelope != context.producer_output_envelope
        || !envelope.allowed_destinations.contains(context.destination)
        || envelope
            .retention
            .expires_at_unix_ms
            .is_some_and(|until| until <= now)
    {
        return Err("original assistant-data producer label changed or expired");
    }
    let mut calls = producer
        .tool_calls
        .iter()
        .filter(|call| call.id == context.tool_call.id);
    let call = calls
        .next()
        .ok_or("original assistant-data tool call is unavailable")?;
    if calls.next().is_some()
        || call.name != context.tool_call.name
        || call.arguments_json != context.tool_call.arguments_json
    {
        return Err("original assistant-data tool arguments changed");
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests;
