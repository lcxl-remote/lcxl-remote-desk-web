//! Original consumed-read identity for retaining the owner's accepted answer.
//! This view has no body, current fact authority, or new physical send grant.

use super::{
    native_export::AssistantDataExportAuthorization,
    output_archive::ArchivedAssistantOutput,
    read_projection::{ReadProjectionKind, ReadProjectionProof},
    seam::AssistantDataReadUse,
};
use desk_agent_protocol::{
    ai_assistant::source::SourceReadPurpose,
    data_lineage::{DataEnvelope, Sensitivity},
};

/// Hosts authenticate the accepted native output, exact complete ledger and all
/// current copy restrictions under the original actor fence. Model-purpose
/// revocation and current retrieval settings cannot replace these checks.
pub struct OwnerHistoryReadValidation {
    actor_id: String,
    conversation_id: String,
    source_device_ref: String,
    actor_input_seq: u64,
    input_envelope: DataEnvelope,
    usage: AssistantDataReadUse,
    snapshot_policy: Option<desk_agent_protocol::ai_assistant::source::SourceUsePolicy>,
}

impl OwnerHistoryReadValidation {
    pub fn for_prepared_projection(proof: &ReadProjectionProof) -> Result<Self, &'static str> {
        proof.validate()?;
        Ok(Self {
            actor_id: proof.actor_id.clone(),
            conversation_id: proof.root_conversation_id.clone(),
            source_device_ref: proof.source_device_ref.clone(),
            actor_input_seq: proof.actor_input_seq,
            input_envelope: proof.source_envelope.clone(),
            usage: proof.usage.clone(),
            snapshot_policy: proof.snapshot_policy.clone(),
        })
    }

    pub fn snapshot_policy(
        &self,
    ) -> Option<&desk_agent_protocol::ai_assistant::source::SourceUsePolicy> {
        self.snapshot_policy.as_ref()
    }
    pub fn for_consumed_projection(
        archive: &ArchivedAssistantOutput,
        grant: &AssistantDataExportAuthorization,
        proof: &ReadProjectionProof,
        first_consumed_at: u64,
        last_consumed_at: u64,
    ) -> Result<Self, &'static str> {
        archive
            .input_policy_graph
            .validate_native_closure(&archive.sources.input_snapshot)?;
        archive.source_set.validate_member(grant)?;
        grant.validate_projection(proof)?;
        if proof.usage.purpose == SourceReadPurpose::OwnerHistory
            || proof.source_envelope.sensitivity != Sensitivity::Sensitive
            || proof.source_envelope.provenance.source_provider_id != "assistant-data-source"
            || proof.source_envelope.provenance.source_tool_name != proof.tool_name
            || proof.source_envelope.provenance.source_object_id.as_deref()
                != Some(proof.usage.projection_id.as_str())
            || first_consumed_at > grant.authorized_at_unix_ms
            || last_consumed_at < grant.authorized_at_unix_ms
            || !proof.preparation_available(grant.authorized_at_unix_ms, Some(first_consumed_at))
        {
            return Err("owner history requires the original consumed model read");
        }
        let sources = archive.sources.input_snapshot.policy_sources();
        let expected = match proof.kind {
            ReadProjectionKind::CurrentMemories => proof.source_envelope.clone(),
            ReadProjectionKind::Tool => {
                let producer_id = proof
                    .producer_output_envelope_id
                    .as_deref()
                    .ok_or("missing original read producer")?;
                let mut producers = sources
                    .iter()
                    .filter(|source| source.envelope_id == producer_id);
                let producer = producers
                    .next()
                    .ok_or("original read producer is absent from the native input")?;
                if producers.next().is_some()
                    || !super::source_registry::is_native_output_policy_source(producer)
                    || !archive
                        .input_policy_graph
                        .parents
                        .iter()
                        .any(|parent| parent.output_envelope_id == producer_id)
                {
                    return Err("original read producer is not an exact native parent");
                }
                crate::model_message_labels::retained_assistant_data_result(
                    producer,
                    &proof.source_envelope,
                    proof
                        .tool_call_id
                        .as_deref()
                        .ok_or("missing original read tool call")?,
                    &proof.tool_name,
                    &proof.usage.retrieval_episode_id,
                )
                .map_err(|_| "invalid original read result identity")?
                .ok_or("missing original read result identity")?
            }
        };
        if !sources.contains(&&expected) {
            return Err("consumed read changed its original full native input label");
        }
        Ok(Self {
            actor_id: archive.source_set.actor_id.clone(),
            conversation_id: archive.source_set.consumer_conversation_id.clone(),
            source_device_ref: archive.source_set.source_device_ref.clone(),
            actor_input_seq: archive.actor_input_seq,
            input_envelope: expected,
            usage: proof.usage.clone(),
            snapshot_policy: proof.snapshot_policy.clone(),
        })
    }

    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    pub fn source_device_ref(&self) -> &str {
        &self.source_device_ref
    }
    pub fn actor_input_seq(&self) -> u64 {
        self.actor_input_seq
    }
    pub fn input_envelope(&self) -> &DataEnvelope {
        &self.input_envelope
    }
    /// Original declarations only. The host checks current source identities,
    /// manual deletion, hard deadlines and no-copy/derived-text restrictions.
    pub fn usage(&self) -> &AssistantDataReadUse {
        &self.usage
    }
}
