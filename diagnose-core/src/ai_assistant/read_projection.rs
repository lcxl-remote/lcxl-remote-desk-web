//! Durable read preparation proofs, separate from native physical-call grants.

use super::seam::AssistantDataReadUse;
use desk_agent_protocol::{
    ai_assistant::{
        memory::MemoryUsePurpose,
        source::{
            SourceReadPurpose, SourceUsePolicy, validate_optional_device_ref, validate_source_id,
        },
    },
    data_lineage::{ContentRef, DataEnvelope},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod claims;
mod current;
mod prepare;
pub use current::CurrentMemoryPreparation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadProjectionKind {
    Tool,
    CurrentMemories,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadProjectionProof {
    pub kind: ReadProjectionKind,
    pub actor_id: String,
    pub root_conversation_id: String,
    pub source_device_ref: String,
    pub input_revision: u64,
    pub actor_input_seq: u64,
    pub task_id: Option<String>,
    pub producer_message_id: Option<String>,
    pub producer_output_envelope_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub logical_call_id: String,
    pub request_sha256: String,
    pub source_envelope: DataEnvelope,
    pub snapshot_policy: Option<SourceUsePolicy>,
    pub usage: AssistantDataReadUse,
    pub created_at_unix_ms: u64,
    pub unused_until_unix_ms: u64,
}

impl ReadProjectionProof {
    #[inline(never)]
    pub fn snapshot_envelope(&self) -> Result<DataEnvelope, &'static str> {
        let policy = self
            .snapshot_policy
            .as_ref()
            .ok_or("read cannot be retained in the conversation")?;
        super::source::validate_policy(policy)?;
        if !policy.history_retention_allowed
            || policy.no_copy
            || policy.no_export
            || !policy.fixed_destinations.is_empty()
        {
            return Err("read cannot be retained in the conversation");
        }
        let mut envelope = self.source_envelope.clone();
        envelope.envelope_id = crate::native_ids::stable(
            crate::native_ids::Kind::Lineage,
            &("conversation-snapshot", self.fingerprint()?),
        );
        envelope.provenance.source_provider_id = "assistant-conversation-snapshot".into();
        envelope.provenance.source_envelope_ids.clear();
        envelope.retention.expires_at_unix_ms = policy.content_hard_expires_at_unix_ms;
        Ok(envelope)
    }

    /// Decode one bounded immutable read snapshot. This checks stored content
    /// integrity only; current owner, source revocation and send permission
    /// remain the responsibility of the caller's original transaction.
    pub fn decode_snapshot(
        proof_json: &str,
        content: Option<&str>,
    ) -> Result<Box<Self>, &'static str> {
        let proof: Box<Self> =
            serde_json::from_str(proof_json).map_err(|_| "invalid stored assistant read proof")?;
        match content {
            Some(body) => proof.validate_cached_result(body)?,
            None => proof.validate()?,
        }
        Ok(proof)
    }

    /// Hash the immutable proof after its boundary validation.
    pub fn fingerprint(&self) -> Result<String, &'static str> {
        let bytes =
            serde_json::to_vec(self).map_err(|_| "invalid assistant read proof encoding")?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    /// A logical retry may use an earlier frozen result. Creation time and the
    /// result's source set belong to that first accepted preparation, not to a
    /// later query attempt; every original caller binding must still match.
    pub fn same_logical_input(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.actor_id == other.actor_id
            && self.root_conversation_id == other.root_conversation_id
            && self.source_device_ref == other.source_device_ref
            && self.input_revision == other.input_revision
            && self.actor_input_seq == other.actor_input_seq
            && self.task_id == other.task_id
            && self.producer_message_id == other.producer_message_id
            && self.producer_output_envelope_id == other.producer_output_envelope_id
            && self.tool_call_id == other.tool_call_id
            && self.tool_name == other.tool_name
            && self.logical_call_id == other.logical_call_id
            && self.request_sha256 == other.request_sha256
            && self.usage.purpose == other.usage.purpose
            && self.usage.retrieval_episode_id == other.usage.retrieval_episode_id
            && self.usage.scope_sha256 == other.usage.scope_sha256
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.usage.validate()?;
        if let Some(policy) = &self.snapshot_policy {
            super::source::validate_policy(policy)?;
        }
        self.source_envelope
            .validate()
            .map_err(|_| "invalid assistant read projection label")?;
        for id in [
            &self.actor_id,
            &self.root_conversation_id,
            &self.logical_call_id,
            &self.tool_name,
        ] {
            validate_source_id(id)?;
        }
        validate_optional_device_ref(&self.source_device_ref)?;
        for id in [
            &self.task_id,
            &self.producer_message_id,
            &self.producer_output_envelope_id,
            &self.tool_call_id,
        ]
        .into_iter()
        .flatten()
        {
            validate_source_id(id)?;
        }
        let content_bytes = match &self.source_envelope.content {
            ContentRef::ImmutableBlob { size_bytes, .. }
            | ContentRef::Projection { size_bytes, .. }
            | ContentRef::Message { size_bytes, .. } => *size_bytes,
            _ => return Err("assistant read projection must bind an immutable result"),
        };
        if self.input_revision == 0
            || self.actor_input_seq == 0
            || self.created_at_unix_ms == 0
            || self.unused_until_unix_ms <= self.created_at_unix_ms
            || self.unused_until_unix_ms > i64::MAX as u64
            || self.request_sha256.len() != 64
            || !self
                .request_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.usage.source_envelope_id != self.source_envelope.envelope_id
            || self.usage.expires_at_unix_ms != self.source_envelope.retention.expires_at_unix_ms
            || self
                .usage
                .expires_at_unix_ms
                .is_some_and(|until| until < self.unused_until_unix_ms)
            || content_bytes == 0
        {
            return Err("invalid assistant read preparation binding");
        }
        match self.kind {
            ReadProjectionKind::Tool => {
                if self.producer_message_id.is_none()
                    || self.producer_output_envelope_id.is_none()
                    || self.tool_call_id.is_none()
                    || !matches!(
                        self.tool_name.as_str(),
                        super::data_tools::SEARCH_MEMORIES
                            | super::data_tools::READ_MEMORY_HISTORY
                            | super::data_tools::MANAGE_MEMORIES
                            | super::data_tools::READ_MEMORY_SOURCE
                            | super::data_tools::SEARCH_CONVERSATIONS
                            | super::data_tools::READ_CONVERSATION
                            | crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME
                            | crate::conversation_attachment::READ_ATTACHMENT_TOOL
                    )
                {
                    return Err("assistant read proof has no original tool producer");
                }
                if (self.tool_name == super::data_tools::READ_CONVERSATION
                    && self.usage.decision_roots.len() != 1)
                    || (self.tool_name != super::data_tools::READ_CONVERSATION
                        && !self.usage.decision_roots.is_empty())
                {
                    return Err("assistant read decision anchor has the wrong tool binding");
                }
            }
            ReadProjectionKind::CurrentMemories => {
                if self.tool_name != "current_memories"
                    || self.producer_message_id.is_some()
                    || self.producer_output_envelope_id.is_some()
                    || self.tool_call_id.is_some()
                    || self.usage.purpose != SourceReadPurpose::ModelMemory
                    || self
                        .usage
                        .memories
                        .iter()
                        .any(|memory| memory.purpose != MemoryUsePurpose::CurrentFact)
                {
                    return Err("current memory preparation cannot impersonate a tool result");
                }
            }
        }
        Ok(())
    }

    pub fn validate_cached_result(&self, content: &str) -> Result<(), &'static str> {
        self.validate()?;
        if self.source_envelope.content.size_bytes() != content.len() as u64 {
            return Err("cached assistant read result size changed");
        }
        serde_json::from_str::<serde::de::IgnoredAny>(content)
            .map_err(|_| "cached assistant read result is not JSON")?;
        Ok(())
    }

    /// A direct read must occur in the exact selected payload and in the actual
    /// sink audit. Merely attaching a prepared proof to a request is insufficient.
    /// Historical model output uses its native successful-output evidence instead.
    pub fn validate_direct_selection(
        &self,
        session: &crate::session::PersistedAgentSession,
        request: &crate::seam::ModelRequest,
        inputs: &[crate::model_egress::ModelInputLineage],
    ) -> Result<(), &'static str> {
        let expected = self.validate_request_selection(session, request)?;
        if !inputs.iter().any(|input| {
            input.envelope_id == expected.envelope_id
                || input.source_envelope_ids.contains(&expected.envelope_id)
        }) {
            return Err("assistant read is absent from the actual model projection");
        }
        Ok(())
    }

    /// Exact prepared payload selection only. Native input admission and live
    /// source authorization are separate checks in the physical-call transaction.
    pub fn validate_request_selection(
        &self,
        session: &crate::session::PersistedAgentSession,
        request: &crate::seam::ModelRequest,
    ) -> Result<DataEnvelope, &'static str> {
        self.validate()?;
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing assistant read session")?;
        if !data.read_uses.contains(&self.usage.reference()) {
            return Err("assistant read is not retained by its source session");
        }
        let expected = match self.kind {
            ReadProjectionKind::CurrentMemories => {
                if self
                    .usage
                    .memories
                    .iter()
                    .any(|memory| !data.current_memories.contains(memory))
                {
                    return Err("current memory block does not match its selected facts");
                }
                self.source_envelope.clone()
            }
            ReadProjectionKind::Tool => {
                let mut producers = session.conversation.iter().filter(|message| {
                    Some(message.message_id.as_str()) == self.producer_message_id.as_deref()
                });
                let producer = producers
                    .next()
                    .ok_or("assistant read producer is missing")?;
                if producers.next().is_some() {
                    return Err("assistant read producer is ambiguous");
                }
                let output = crate::model_egress::model_output_message_envelope(producer)
                    .map_err(|_| "assistant read producer payload changed")?;
                if Some(output.envelope_id.as_str()) != self.producer_output_envelope_id.as_deref()
                {
                    return Err("assistant read producer identity changed");
                }
                let mut calls = producer
                    .tool_calls
                    .iter()
                    .filter(|call| Some(call.id.as_str()) == self.tool_call_id.as_deref());
                let call = calls.next().ok_or("assistant read tool call is missing")?;
                if calls.next().is_some()
                    || call.name != self.tool_name
                    || request_sha256(&call.name, &call.arguments_json)? != self.request_sha256
                {
                    return Err("assistant read tool arguments changed");
                }
                let result = request
                    .messages
                    .iter()
                    .find(|message| {
                        message.role == crate::chat::ChatRole::Tool
                            && message.tool_call_id.as_deref() == self.tool_call_id.as_deref()
                    })
                    .ok_or("assistant read tool result is missing")?;
                self.validate_cached_result(&result.text)?;
                crate::model_message_labels::assistant_data_result_envelope(
                    Some(output),
                    std::slice::from_ref(&self.source_envelope),
                    &call.id,
                    &result.text,
                    &call.name,
                    &self.usage.retrieval_episode_id,
                )
                .map_err(|_| "assistant read aggregate label is invalid")?
                .ok_or("assistant read aggregate label is missing")?
            }
        };
        let mut selected = request
            .messages
            .iter()
            .filter(|message| message.data_envelope.as_ref() == Some(&expected));
        let message = selected
            .next()
            .ok_or("assistant read is absent from the selected model context")?;
        if selected.next().is_some()
            || message.role
                != match self.kind {
                    ReadProjectionKind::Tool => crate::chat::ChatRole::Tool,
                    ReadProjectionKind::CurrentMemories => crate::chat::ChatRole::User,
                }
        {
            return Err("assistant read selected payload changed");
        }
        self.validate_cached_result(&message.text)?;
        Ok(expected)
    }

    /// A consumed proof keeps its original hard source deadline. This merely
    /// checks preparation lifetime; the native send gate still revalidates all
    /// current source, task, actor and session conditions for its physical call.
    pub fn preparation_available(&self, now: u64, consumed_at: Option<u64>) -> bool {
        self.validate().is_ok()
            && now >= self.created_at_unix_ms
            && self
                .usage
                .expires_at_unix_ms
                .is_none_or(|until| until > now)
            && match consumed_at {
                Some(at) => {
                    at >= self.created_at_unix_ms && at < self.unused_until_unix_ms && at <= now
                }
                None => now < self.unused_until_unix_ms,
            }
    }
}

pub fn request_sha256(tool_name: &str, arguments_json: &str) -> Result<String, &'static str> {
    let bytes = serde_json::to_vec(&(tool_name, arguments_json))
        .map_err(|_| "invalid assistant read input")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ai_assistant::{data_access::scope_sha256, test_support::personal_session},
        model_message_labels::model_bound_user_message,
        native_ids::{self, Kind},
    };
    use desk_agent_protocol::data_lineage::DestinationIdentity;

    fn proof() -> ReadProjectionProof {
        let session = personal_session("owner", "root", "device");
        let envelope = model_bound_user_message(
            "read-body".into(),
            "{\"memories\":[]}".into(),
            DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
        .data_envelope
        .unwrap();
        ReadProjectionProof {
            kind: ReadProjectionKind::Tool,
            actor_id: "owner".into(),
            root_conversation_id: "root".into(),
            source_device_ref: "device".into(),
            input_revision: 1,
            actor_input_seq: 1,
            task_id: None,
            producer_message_id: Some("producer".into()),
            producer_output_envelope_id: Some("output".into()),
            tool_call_id: Some("call".into()),
            tool_name: super::super::data_tools::SEARCH_MEMORIES.into(),
            logical_call_id: native_ids::stable(Kind::AssistantReadAuthorization, &"call"),
            request_sha256: request_sha256("search_memories", "{\"queries\":[\"语言\"]}").unwrap(),
            usage: AssistantDataReadUse {
                decision_roots: Vec::new(),
                projection_id: native_ids::stable(Kind::AssistantReadProjection, &"call"),
                source_envelope_id: envelope.envelope_id.clone(),
                purpose: SourceReadPurpose::ModelMemory,
                retrieval_episode_id: "episode-1".into(),
                scope_sha256: scope_sha256(&session).unwrap(),
                policy_revision: 1,
                control_generation: 1,
                memory_state_generation: 1,
                roots: Vec::new(),
                history_dependencies: Vec::new(),
                memories: Vec::new(),
                expires_at_unix_ms: None,
            },
            source_envelope: envelope,
            snapshot_policy: None,
            created_at_unix_ms: 1000,
            unused_until_unix_ms: 1000
                + desk_agent_protocol::ai_assistant::limits::DEFAULT_UNUSED_PROJECTION_TTL_MS,
        }
    }

    #[test]
    fn direct_tool_reads_bind_the_original_arguments_complete_result_and_aggregate_label() {
        use crate::{
            chat::{ChatMessage, ModelTurn, StopReason, ToolCall},
            model_egress::{ModelEgressPolicy, ModelInputLineage},
        };
        let mut proof = proof();
        let destination = proof.source_envelope.allowed_destinations[0].clone();
        let policy = ModelEgressPolicy {
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "original-tool-producer".into(),
            now_unix_ms: 1000,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        };
        let turn = ModelTurn {
            tool_calls: vec![ToolCall {
                id: "call".into(),
                name: proof.tool_name.clone(),
                arguments_json: "{\"queries\":[\"语言\"]}".into(),
            }],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        };
        let input =
            model_bound_user_message("input".into(), "remembered preference".into(), destination)
                .unwrap()
                .data_envelope
                .unwrap();
        let output = policy
            .derive_model_output_envelope(&turn, &[input])
            .unwrap();
        let mut producer = ChatMessage::assistant_tool_calls(
            "producer",
            "",
            turn.tool_calls.iter().map(ToolCall::to_ref).collect(),
        );
        producer.data_envelope = Some(output.clone());
        proof.producer_output_envelope_id = Some(output.envelope_id.clone());
        let mut result = ChatMessage::tool_result("result", "call", "{\"memories\":[]}");
        result.data_envelope = crate::model_message_labels::assistant_data_result_envelope(
            Some(&output),
            std::slice::from_ref(&proof.source_envelope),
            "call",
            &result.text,
            &proof.tool_name,
            &proof.usage.retrieval_episode_id,
        )
        .unwrap();
        let label = result.data_envelope.as_ref().unwrap();
        let actual = ModelInputLineage {
            envelope_id: label.envelope_id.clone(),
            size_bytes: label.content.size_bytes(),
            source_provider_id: label.provenance.source_provider_id.clone(),
            source_tool_name: label.provenance.source_tool_name.clone(),
            source_envelope_ids: label.provenance.source_envelope_ids.clone(),
            public_system_prompt: false,
        };
        let mut session = personal_session("owner", "root", "device");
        session.conversation.push(producer);
        session
            .assistant_data
            .as_mut()
            .unwrap()
            .read_uses
            .push(proof.usage.reference());
        let request = crate::seam::ModelRequest::text_only(
            vec![result],
            crate::prompt::ResponseFormatSpec::None,
        );
        assert!(
            crate::ai_assistant::seam::AssistantModelDataManifest::validate_absence(&request)
                .is_err()
        );
        proof
            .validate_direct_selection(&session, &request, std::slice::from_ref(&actual))
            .unwrap();
        for case in 0..5 {
            let mut changed_session = session.clone();
            let mut changed_request = request.clone();
            match case {
                0 => changed_session.conversation[0].tool_calls[0].arguments_json = "{}".into(),
                1 => changed_request.messages[0].tool_call_id = Some("other-call".into()),
                2 => changed_request.messages[0].text = "{\"memories\":[\"invented\"]}".into(),
                3 => {
                    changed_request.messages[0]
                        .data_envelope
                        .as_mut()
                        .unwrap()
                        .retention
                        .delete_with_run = false
                }
                _ => changed_session
                    .conversation
                    .push(changed_session.conversation[0].clone()),
            }
            assert!(
                proof
                    .validate_direct_selection(
                        &changed_session,
                        &changed_request,
                        std::slice::from_ref(&actual)
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn a_prepared_read_requires_the_exact_selected_body_session_reference_and_actual_sink_lineage()
    {
        let mut proof = proof();
        proof.kind = ReadProjectionKind::CurrentMemories;
        proof.tool_name = "current_memories".into();
        proof.producer_message_id = None;
        proof.producer_output_envelope_id = None;
        proof.tool_call_id = None;
        let mut session = personal_session("owner", "root", "device");
        session
            .assistant_data
            .as_mut()
            .unwrap()
            .read_uses
            .push(proof.usage.reference());
        let mut message = crate::chat::ChatMessage::text(
            "memory-block",
            crate::chat::ChatRole::User,
            "{\"memories\":[]}",
        );
        message.data_envelope = Some(proof.source_envelope.clone());
        let request = crate::seam::ModelRequest::text_only(
            vec![message],
            crate::prompt::ResponseFormatSpec::None,
        );
        crate::ai_assistant::seam::AssistantModelDataManifest::validate_absence(&request).unwrap();
        let mut unmanifested_memory = request.clone();
        unmanifested_memory.messages[0]
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = "assistant-data-source".into();
        assert!(
            crate::ai_assistant::seam::AssistantModelDataManifest::validate_absence(
                &unmanifested_memory
            )
            .is_err()
        );
        let input = crate::model_egress::ModelInputLineage {
            envelope_id: proof.source_envelope.envelope_id.clone(),
            size_bytes: proof.source_envelope.content.size_bytes(),
            source_provider_id: proof.source_envelope.provenance.source_provider_id.clone(),
            source_tool_name: proof.source_envelope.provenance.source_tool_name.clone(),
            source_envelope_ids: Vec::new(),
            public_system_prompt: false,
        };
        proof
            .validate_direct_selection(&session, &request, std::slice::from_ref(&input))
            .unwrap();
        for case in 0..5 {
            let mut changed_request = request.clone();
            let mut changed_session = session.clone();
            let mut changed_input = input.clone();
            match case {
                0 => changed_request.messages[0].text.push(' '),
                1 => {
                    changed_request.messages[0]
                        .data_envelope
                        .as_mut()
                        .unwrap()
                        .retention
                        .delete_with_run = true
                }
                2 => changed_session
                    .assistant_data
                    .as_mut()
                    .unwrap()
                    .read_uses
                    .clear(),
                3 => changed_input.envelope_id = "different-sink-input".into(),
                _ => changed_request
                    .messages
                    .push(changed_request.messages[0].clone()),
            }
            assert!(
                proof
                    .validate_direct_selection(&changed_session, &changed_request, &[changed_input])
                    .is_err()
            );
        }
        let projected = crate::model_egress::ModelInputLineage {
            envelope_id: "actual-projection".into(),
            source_envelope_ids: vec![proof.source_envelope.envelope_id.clone()],
            ..input
        };
        proof
            .validate_direct_selection(&session, &request, &[projected])
            .unwrap();
    }

    #[test]
    fn cached_result_is_bound_to_the_exact_full_payload_not_just_its_source_ids() {
        let proof = proof();
        proof.validate_cached_result("{\"memories\":[]}").unwrap();
        assert!(
            proof
                .validate_cached_result("{\"memories\":[\"invented\"]}")
                .is_err()
        );
        assert!(
            proof
                .validate_cached_result("{ \"memories\": [] }")
                .is_err()
        );
        let mut changed = proof;
        changed.usage.source_envelope_id = "different-source".into();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn stored_snapshot_integrity_does_not_grant_use_after_its_deadline() {
        let original = proof();
        let json = serde_json::to_string(&original).unwrap();
        let snapshot =
            ReadProjectionProof::decode_snapshot(&json, Some(r#"{"memories":[]}"#)).unwrap();
        assert_eq!(*snapshot, original);
        assert!(!snapshot.preparation_available(snapshot.unused_until_unix_ms, None));
        assert_eq!(
            *ReadProjectionProof::decode_snapshot(&json, None).unwrap(),
            original
        );
        assert!(
            ReadProjectionProof::decode_snapshot(&json, Some(r#"{"memories":["invented"]}"#))
                .is_err()
        );
        assert!(ReadProjectionProof::decode_snapshot("invalid-json", None).is_err());
        let mut changed = original;
        changed.usage.source_envelope_id = "different-source".into();
        assert!(
            ReadProjectionProof::decode_snapshot(&serde_json::to_string(&changed).unwrap(), None)
                .is_err()
        );
    }

    #[test]
    fn an_actual_consumption_keeps_necessary_proof_beyond_unused_preparation_ttl() {
        let mut proof = proof();
        assert!(proof.preparation_available(1001, None));
        let later = proof.unused_until_unix_ms + 1;
        assert!(!proof.preparation_available(later, None));
        assert!(proof.preparation_available(later, Some(1001)));
        assert!(!proof.preparation_available(later, Some(proof.unused_until_unix_ms)));
        proof.usage.expires_at_unix_ms = Some(later + 10);
        proof.source_envelope.retention.expires_at_unix_ms = Some(later + 10);
        assert!(proof.preparation_available(later, Some(1001)));
        assert!(!proof.preparation_available(later + 10, Some(1001)));
    }

    #[test]
    fn owner_history_and_fabricated_tool_producers_cannot_form_model_read_proofs() {
        let mut proof = proof();
        proof.usage.purpose = SourceReadPurpose::OwnerHistory;
        assert!(proof.validate().is_err());
        proof.usage.purpose = SourceReadPurpose::ModelMemory;
        proof.producer_message_id = None;
        assert!(proof.validate().is_err());
        proof.kind = ReadProjectionKind::CurrentMemories;
        proof.tool_name = "current_memories".into();
        assert!(proof.validate().is_err());
        proof.producer_output_envelope_id = None;
        proof.tool_call_id = None;
        assert!(proof.validate().is_ok());
    }

    #[test]
    fn session_reference_keeps_identity_and_generations_without_copying_proof_arrays() {
        let proof = proof();
        let reference = proof.usage.reference();
        reference.validate().unwrap();
        assert_eq!(reference.projection_id, proof.usage.projection_id);
        assert_eq!(reference.scope_sha256, proof.usage.scope_sha256);
        let encoded = serde_json::to_value(reference).unwrap();
        assert!(encoded.get("history_dependencies").is_none());
        assert!(encoded.get("memories").is_none());
        assert!(encoded.get("roots").is_none());
    }

    #[test]
    fn returned_roots_are_closed_and_source_hard_deadlines_cannot_be_broadened() {
        use super::super::seam::AssistantReadRoot;
        let mut proof = proof();
        let source_id = native_ids::stable(Kind::AssistantSource, &"read-root");
        proof.usage.history_dependencies.push(
            desk_agent_protocol::ai_assistant::source::HistoryDependency {
                source_id: source_id.clone(),
                source_revision: 1,
                origin_id: native_ids::stable(Kind::AssistantOrigin, &"root-origin"),
                origin_deletion_generation: 1,
                policy_revision: 1,
                expires_at_unix_ms: None,
            },
        );
        assert!(proof.validate().is_err());
        proof.usage.roots.push(AssistantReadRoot {
            source_id,
            source_revision: 1,
        });
        proof.validate().unwrap();
        let mut changed = proof.clone();
        changed.usage.roots.push(changed.usage.roots[0].clone());
        assert!(changed.validate().is_err());
        let mut changed = proof.clone();
        changed.usage.roots[0].source_revision = 2;
        assert!(changed.validate().is_err());
        let expiry = proof.unused_until_unix_ms + 100;
        proof.usage.history_dependencies[0].expires_at_unix_ms = Some(expiry);
        assert!(proof.validate().is_err());
        proof.usage.expires_at_unix_ms = Some(expiry);
        proof.source_envelope.retention.expires_at_unix_ms = Some(expiry);
        proof.validate().unwrap();
    }
}
