//! Pure current-fact preparation. Native owner/session/task/source gates remain
//! mandatory at storage and at every actual physical model send.

use super::{ReadProjectionKind, ReadProjectionProof, request_sha256};
use crate::ai_assistant::{
    current_memory::CurrentMemoryPayload,
    data_access::{AssistantDataToolAccess, assistant_scope, eligible_origin},
    memory_access::usable_memory_scope,
    seam::AssistantDataReadUse,
};
use crate::{
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use desk_agent_protocol::{
    ai_assistant::{
        memory::MemoryUsePurpose,
        source::{SourceReadPurpose, validate_source_id},
    },
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Host-created context for one read preparation, never a physical send grant.
#[derive(Clone, Copy)]
pub struct CurrentMemoryPreparation<'a> {
    pub session: &'a PersistedAgentSession,
    pub access: &'a AssistantDataToolAccess,
    pub destination: &'a DestinationIdentity,
    pub preparation_id: &'a str,
    pub maximum_bytes: usize,
    pub authority_deadline: u64,
    pub now: u64,
}

impl ReadProjectionProof {
    pub fn current_logical_id(
        session: &PersistedAgentSession,
        preparation_id: &str,
    ) -> Result<String, &'static str> {
        validate_source_id(preparation_id)?;
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing current-memory session")?;
        Ok(native_ids::stable(
            Kind::AssistantReadAuthorization,
            &(
                "current-memory-preparation-v1",
                &session.actor_id,
                &session.conversation_id,
                session.input_revision,
                data.current_actor_input_seq,
                &data.retrieval_budget.episode_id,
                preparation_id,
            ),
        ))
    }

    pub fn for_current_memories(
        context: CurrentMemoryPreparation<'_>,
        content: &str,
        selected: &[AssistantDataReadUse],
    ) -> Result<Self, &'static str> {
        let CurrentMemoryPreparation {
            session,
            access,
            destination,
            preparation_id,
            maximum_bytes,
            authority_deadline,
            now,
        } = context;
        if !eligible_origin(session)
            || !matches!(destination, DestinationIdentity::Model { .. })
            || authority_deadline <= now
            || destination.validate().is_err()
        {
            return Err("invalid current-memory preparation context");
        }
        access.validate()?;
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing current-memory session")?;
        let scope = usable_memory_scope(session, now)
            .ok_or("original current-memory consent is unavailable")?;
        scope.validate()?;
        if now < scope.accepted_at_unix_ms
            || !access.settings.automatic_memory_enabled
            || data.memory_state_generation != access.memory_state_generation
            || data.control_generation != access.control_generation
        {
            return Err("current-memory preparation changed its original consent or generation");
        }
        let payload = CurrentMemoryPayload::decode(content, maximum_bytes, now)?;
        if payload.memory_state_generation != access.memory_state_generation
            || payload.memories.iter().any(|memory| {
                memory
                    .source_device
                    .as_ref()
                    .is_some_and(|device| !scope.current_device_refs().contains(&device.device_ref))
            })
        {
            return Err("current-memory payload generation or current device changed");
        }
        let logical_id = Self::current_logical_id(session, preparation_id)?;
        let arguments =
            serde_json::to_string(&("current-memory-request-v1", preparation_id, maximum_bytes))
                .map_err(|_| "invalid current-memory request")?;
        let request_hash = request_sha256("current_memories", &arguments)?;
        let projection_id = native_ids::stable(
            Kind::AssistantReadProjection,
            &("current-memory-read-v1", &logical_id, &request_hash),
        );
        let mut usage = AssistantDataReadUse {
            projection_id: projection_id.clone(),
            source_envelope_id: "pending-current-memory-label".into(),
            purpose: SourceReadPurpose::ModelMemory,
            retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
            scope: assistant_scope(session)?,
            policy_revision: access.policy_revision,
            control_generation: access.control_generation,
            memory_state_generation: access.memory_state_generation,
            roots: Vec::new(),
            decision_roots: Vec::new(),
            history_dependencies: Vec::new(),
            memories: Vec::new(),
            expires_at_unix_ms: None,
        };
        super::prepare::aggregate(&mut usage, selected, now)?;
        if !usage.decision_roots.is_empty()
            || usage
                .memories
                .iter()
                .any(|memory| memory.purpose != MemoryUsePurpose::CurrentFact)
        {
            return Err("current-memory payload contains historical or decision-only sources");
        }
        let expected = payload
            .memories
            .iter()
            .map(|memory| (memory.memory_id.as_str(), memory.revision))
            .collect::<BTreeSet<_>>();
        let actual = usage
            .memories
            .iter()
            .map(|memory| (memory.memory_id.as_str(), memory.revision))
            .collect::<BTreeSet<_>>();
        let roots = usage
            .roots
            .iter()
            .map(|source| source.source_id.as_str())
            .collect::<BTreeSet<_>>();
        let returned = payload
            .memories
            .iter()
            .flat_map(|memory| memory.source_ids.iter().map(String::as_str))
            .collect::<BTreeSet<_>>();
        if expected != actual || roots != returned {
            return Err("current-memory payload does not match its complete actual source set");
        }
        let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
        let envelope_id = native_ids::stable(
            Kind::Lineage,
            &("current-memory-result-v1", &projection_id, &digest, &usage),
        );
        usage.source_envelope_id = envelope_id.clone();
        let source_envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id,
            content: ContentRef::ImmutableBlob {
                blob_id: format!("current-memory-{projection_id}"),
                sha256: digest.clone(),
                size_bytes: content.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "assistant-data-source".into(),
                source_tool_name: "current_memories".into(),
                source_object_id: Some(projection_id),
                source_envelope_ids: usage
                    .history_dependencies
                    .iter()
                    .map(|dependency| {
                        native_ids::stable(
                            Kind::Lineage,
                            &("retained-source-version-v1", &session.actor_id, dependency),
                        )
                    })
                    .collect(),
            },
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: usage.expires_at_unix_ms,
                delete_with_run: false,
            },
        };
        let mut unused_until = authority_deadline.min(
            now.checked_add(data.retrieval_budget.limits.unused_projection_ttl_ms)
                .ok_or("current-memory deadline overflow")?,
        );
        for deadline in [scope.expires_at_unix_ms, usage.expires_at_unix_ms]
            .into_iter()
            .flatten()
        {
            unused_until = unused_until.min(deadline);
        }
        if unused_until <= now {
            return Err("current-memory preparation expired");
        }
        let proof = Self {
            kind: ReadProjectionKind::CurrentMemories,
            actor_id: session.actor_id.clone(),
            root_conversation_id: session.conversation_id.clone(),
            source_device_ref: data.source_device_ref.clone(),
            input_revision: session.input_revision,
            actor_input_seq: data.current_actor_input_seq,
            task_id: data.retrieval_task_id.clone(),
            producer_message_id: None,
            producer_output_envelope_id: None,
            tool_call_id: None,
            tool_name: "current_memories".into(),
            logical_call_id: logical_id,
            request_sha256: request_hash,
            source_envelope,
            snapshot_policy: None,
            usage,
            created_at_unix_ms: now,
            unused_until_unix_ms: unused_until,
        };
        Ok(proof)
    }

    pub fn validate_current_replay(
        &self,
        context: CurrentMemoryPreparation<'_>,
        content: &str,
        consumed_at: Option<u64>,
    ) -> Result<(), &'static str> {
        if self.kind != ReadProjectionKind::CurrentMemories
            || !self.preparation_available(context.now, consumed_at)
        {
            return Err("original current-memory preparation is unavailable");
        }
        let candidate =
            Self::for_current_memories(context, content, std::slice::from_ref(&self.usage))?;
        if !self.same_logical_input(&candidate)
            || self.usage != candidate.usage
            || self.source_envelope != candidate.source_envelope
            || self.unused_until_unix_ms > candidate.unused_until_unix_ms
        {
            return Err("original current-memory preparation changed");
        }
        Ok(())
    }
}
