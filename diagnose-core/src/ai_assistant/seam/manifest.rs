//! Deterministic direct-read selection. The host loads original read proofs;
//! this metadata does not replace source checks or permit a physical send.

use super::*;
use crate::ai_assistant::{
    data_access::{AssistantDataToolAccess, retained_context_eligible, scope_sha256},
    read_projection::ReadProjectionProof,
};
use crate::{chat::ChatRole, seam::ModelRequest};
use desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose;
use std::collections::{BTreeMap, BTreeSet};

impl AssistantModelDataManifest {
    /// Select only direct payloads that actually survived context planning.
    /// Stored session references are candidate identities, never read grants.
    pub fn selected_read_ids(
        session: &PersistedAgentSession,
        request: &ModelRequest,
    ) -> Result<Vec<String>, &'static str> {
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing assistant manifest session")?;
        let mut selected = BTreeSet::new();
        for message in &request.messages {
            let Some(envelope) = &message.data_envelope else {
                continue;
            };
            let direct = message.role == ChatRole::Tool
                && envelope.provenance.source_provider_id == "assistant-data-result";
            let current = message.role == ChatRole::User
                && envelope.provenance.source_provider_id == "assistant-data-source";
            let mut linked = false;
            for reference in &data.read_uses {
                if (current && envelope.envelope_id == reference.source_envelope_id)
                    || (direct
                        && envelope
                            .provenance
                            .source_envelope_ids
                            .contains(&reference.source_envelope_id))
                {
                    selected.insert(reference.projection_id.clone());
                    linked = true;
                }
            }
            if (envelope.provenance.source_provider_id == "assistant-data-source" && !linked)
                || (direct && envelope.provenance.source_envelope_ids.len() > 1 && !linked)
            {
                return Err("selected assistant payload has no retained projection identity");
            }
        }
        Ok(selected.into_iter().collect())
    }

    pub fn for_direct_request(
        session: &PersistedAgentSession,
        request: &ModelRequest,
        access: &AssistantDataToolAccess,
        proofs: &[&ReadProjectionProof],
    ) -> Result<Self, &'static str> {
        access.validate()?;
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing assistant manifest session")?;
        if !retained_context_eligible(session)
            || request.context_conversation_id.as_deref() != Some(session.conversation_id.as_str())
            || access.control_generation != data.control_generation
            || access.memory_state_generation != data.memory_state_generation
        {
            return Err("assistant manifest source context changed");
        }
        let selected_ids = Self::selected_read_ids(session, request)?;
        if proofs.len() != selected_ids.len() {
            return Err("assistant manifest read proofs are incomplete");
        }
        let scope = scope_sha256(session)?;
        let mut seen = BTreeSet::new();
        let mut memories = BTreeMap::new();
        for proof in proofs {
            if !selected_ids.contains(&proof.usage.projection_id)
                || !seen.insert(&proof.usage.projection_id)
                || proof.actor_id != session.actor_id
                || proof.root_conversation_id != session.conversation_id
                || proof.source_device_ref != data.source_device_ref
                || proof.actor_input_seq != data.current_actor_input_seq
                || proof.input_revision != session.input_revision
                || proof.task_id != data.retrieval_task_id
                || proof.usage.retrieval_episode_id != data.retrieval_budget.episode_id
                || proof.usage.scope_sha256 != scope
                || proof.usage.policy_revision != access.policy_revision
                || proof.usage.control_generation != access.control_generation
                || proof.usage.memory_state_generation != access.memory_state_generation
            {
                return Err("assistant manifest original read binding changed");
            }
            proof.validate_request_selection(session, request)?;
            for memory in proof
                .usage
                .memories
                .iter()
                .filter(|memory| memory.purpose == MemoryUsePurpose::CurrentFact)
            {
                if !data.current_memories.contains(memory)
                    || memories
                        .insert((&memory.memory_id, memory.revision), memory.clone())
                        .is_some_and(|old| old != *memory)
                {
                    return Err("assistant manifest current fact changed");
                }
            }
        }
        let mut selected = BTreeMap::new();
        for message in &request.messages {
            if let Some(envelope) = &message.data_envelope {
                envelope
                    .validate()
                    .map_err(|_| "invalid assistant manifest input label")?;
                if selected
                    .insert(&envelope.envelope_id, envelope)
                    .is_some_and(|old| old != envelope)
                {
                    return Err("assistant manifest input label changed under the same identity");
                }
            }
        }
        crate::model_egress::sink_metadata_sha256(&selected)?;
        let manifest = Self {
            actor_id: session.actor_id.clone(),
            conversation_id: session.conversation_id.clone(),
            source_device_ref: data.source_device_ref.clone(),
            request_memory_device_refs: Self::selected_memory_devices(data, &selected_ids).to_vec(),
            session_version: session.version,
            lease_token: session.lease_token,
            input_revision: session.input_revision,
            actor_input_seq: data.current_actor_input_seq,
            retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
            task_id: data.retrieval_task_id.clone(),
            scope_sha256: scope,
            policy_revision: access.policy_revision,

            memory_state_generation: access.memory_state_generation,
            control_generation: access.control_generation,
            selected_envelopes: selected
                .values()
                .map(|envelope| SelectedAssistantEnvelope {
                    envelope_id: envelope.envelope_id.clone(),
                    digest_sha256: envelope.digest_sha256.clone(),
                })
                .collect(),
            read_projection_ids: selected_ids,
            current_memories: memories.into_values().collect(),
        };
        manifest.validate_for(session, request)?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests;
