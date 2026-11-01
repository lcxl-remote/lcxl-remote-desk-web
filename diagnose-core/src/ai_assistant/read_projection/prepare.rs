//! Purpose-bound read preparation. The host owns fresh native actor/task/source
//! validation; this constructor cannot replace any of those database gates.

use super::{ReadProjectionKind, ReadProjectionProof, request_sha256};
use crate::ai_assistant::{
    data_access::{AssistantDataToolAccess, scope_sha256},
    data_control::CurrentMemoryUse,
    history::serialized_tool_response_bytes,
    seam::{AssistantDataCall, AssistantDataReadUse},
};
use crate::native_ids::{self, Kind};
use desk_agent_protocol::{
    ai_assistant::source::{HistoryDependency, SourceReadPurpose},
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
        Sensitivity,
    },
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

impl ReadProjectionProof {
    pub fn validate_retrieval_replay(
        &self,
        context: &AssistantDataCall<'_>,
        access: &AssistantDataToolAccess,
        content: &str,
    ) -> Result<(), &'static str> {
        if !self.preparation_available(context.now_unix_ms, None) {
            return Err("original retrieval preparation expired");
        }
        {
            let candidate = Self::for_retrieval(
                context,
                access,
                content,
                self.usage.purpose,
                std::slice::from_ref(&self.usage),
                context.now_unix_ms,
            )?;
            if !self.same_logical_input(&candidate)
                || self.usage != candidate.usage
                || self.source_envelope != candidate.source_envelope
                || self.unused_until_unix_ms > candidate.unused_until_unix_ms
            {
                return Err("original retrieval preparation changed");
            }
        }
        super::claims::validate(
            context.arguments,
            content,
            &self.usage,
            &context.tool_call.id,
        )
    }
    /// Only the selected source usages belong here. Scanned-but-omitted sources
    /// must not acquire an output root or be marked delivered by the query.
    pub fn for_retrieval(
        context: &AssistantDataCall<'_>,
        access: &AssistantDataToolAccess,
        content: &str,
        purpose: SourceReadPurpose,
        selected: &[AssistantDataReadUse],
        now: u64,
    ) -> Result<Self, &'static str> {
        let original =
            crate::ai_assistant::assistant_data_admission::OriginalAssistantDataCall::retrieval(
                context, now,
            )?;
        let data = context
            .session
            .assistant_data
            .as_ref()
            .ok_or("missing assistant read source session")?;
        let scope_expiry = original.scope_deadline(context, access, now)?;
        let request_hash =
            request_sha256(&context.tool_call.name, &context.tool_call.arguments_json)?;
        if serialized_tool_response_bytes(&context.tool_call.id, content, None)?
            > original.maximum_response_bytes()
        {
            return Err("read preparation exceeded its original response limit");
        }
        let scope_hash = scope_sha256(context.session)?;
        let projection_id = native_ids::stable(
            Kind::AssistantReadProjection,
            &(
                "assistant-read-v1",
                &context.session.actor_id,
                &context.session.conversation_id,
                &data.retrieval_budget.episode_id,
                context.logical_call_id,
                &request_hash,
                purpose,
            ),
        );
        let mut usage = AssistantDataReadUse {
            decision_roots: Vec::new(),
            projection_id: projection_id.clone(),
            source_envelope_id: "pending-read-label".into(),
            purpose,
            retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
            scope_sha256: scope_hash,
            policy_revision: access.policy_revision,
            memory_state_generation: access.memory_state_generation,
            control_generation: access.control_generation,
            roots: Vec::new(),
            history_dependencies: Vec::new(),
            memories: Vec::new(),
            expires_at_unix_ms: None,
        };
        aggregate(&mut usage, selected, now)?;
        let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
        let envelope_id = native_ids::stable(
            Kind::Lineage,
            &("assistant-read-result-v1", &projection_id, &digest, &usage),
        );
        usage.source_envelope_id = envelope_id.clone();
        let source_envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id,
            content: ContentRef::ImmutableBlob {
                blob_id: format!("assistant-read-{projection_id}"),
                sha256: digest.clone(),
                size_bytes: content.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "assistant-data-source".into(),
                source_tool_name: context.tool_call.name.clone(),
                source_object_id: Some(projection_id),
                source_envelope_ids: usage
                    .history_dependencies
                    .iter()
                    .map(|dependency| {
                        native_ids::stable(
                            Kind::Lineage,
                            &(
                                "retained-source-version-v1",
                                &context.session.actor_id,
                                dependency,
                            ),
                        )
                    })
                    .collect(),
            },
            digest_sha256: digest,
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![context.destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: usage.expires_at_unix_ms,
                delete_with_run: false,
            },
        };
        let mut until = original
            .unused_read_deadline()
            .ok_or("missing original read deadline")?;
        for bound in [scope_expiry, usage.expires_at_unix_ms]
            .into_iter()
            .flatten()
        {
            until = until.min(bound);
        }
        if until <= now {
            return Err("original read preparation expired");
        }
        Ok(Self {
            kind: ReadProjectionKind::Tool,
            actor_id: context.session.actor_id.clone(),
            root_conversation_id: context.session.conversation_id.clone(),
            source_device_ref: data.source_device_ref.clone(),
            input_revision: context.session.input_revision,
            actor_input_seq: data.current_actor_input_seq,
            task_id: data.retrieval_task_id.clone(),
            producer_message_id: Some(context.producer_message.message_id.clone()),
            producer_output_envelope_id: Some(context.producer_output_envelope.envelope_id.clone()),
            tool_call_id: Some(context.tool_call.id.clone()),
            tool_name: context.tool_call.name.clone(),
            logical_call_id: context.logical_call_id.into(),
            request_sha256: request_hash,
            source_envelope,
            snapshot_policy: None,
            usage,
            created_at_unix_ms: now,
            unused_until_unix_ms: until,
        })
    }
}

/// Shared aggregation preserves exact per-source purpose, generation and full
/// dependency constraints. Call-specific result claims remain separate.
pub(super) fn aggregate(
    usage: &mut AssistantDataReadUse,
    selected: &[AssistantDataReadUse],
    now: u64,
) -> Result<(), &'static str> {
    let mut dependencies = BTreeMap::<(String, u64), HistoryDependency>::new();
    let mut roots = BTreeMap::new();
    let mut decision_roots = BTreeMap::new();
    let mut memories = BTreeMap::<(String, u64, u8), CurrentMemoryUse>::new();
    for source in selected {
        if source.purpose != usage.purpose
            || source.retrieval_episode_id != usage.retrieval_episode_id
            || source.scope_sha256 != usage.scope_sha256
            || source.policy_revision != usage.policy_revision
            || source.control_generation != usage.control_generation
            || source.memory_state_generation != usage.memory_state_generation
            || source.expires_at_unix_ms.is_some_and(|until| until <= now)
        {
            return Err("selected read source changed its original purpose or generation");
        }
        usage.expires_at_unix_ms = earlier(usage.expires_at_unix_ms, source.expires_at_unix_ms);
        for dependency in &source.history_dependencies {
            let key = (dependency.source_id.clone(), dependency.source_revision);
            if let Some(old) = dependencies.insert(key, dependency.clone())
                && old != *dependency
            {
                return Err("selected read sources contain conflicting dependency constraints");
            }
        }
        for root in &source.roots {
            roots.insert((root.source_id.clone(), root.source_revision), root.clone());
        }
        for root in &source.decision_roots {
            decision_roots.insert((root.source_id.clone(), root.source_revision), root.clone());
        }
        for memory in &source.memories {
            memories.insert(
                (
                    memory.memory_id.clone(),
                    memory.revision,
                    memory.purpose as u8,
                ),
                memory.clone(),
            );
        }
        if decision_roots.len() > 1 {
            return Err("selected read source metadata exceeds its budget");
        }
    }
    usage.history_dependencies = dependencies.into_values().collect();
    usage.roots = roots.into_values().collect();
    usage.decision_roots = decision_roots.into_values().collect();
    usage.memories = memories.into_values().collect();
    usage.validate()
}
fn earlier(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests;
