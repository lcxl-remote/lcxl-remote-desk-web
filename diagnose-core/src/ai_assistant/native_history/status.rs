//! Content-only projection of the original host-verified native inventory.
//! It carries no execution handles, model-send grant, result body or task text.
use super::*;
use crate::{
    ai_assistant::source::{
        HistoryCapture, HistorySourceDirectory, history_origin_id, history_source_id,
        owner_text_policy, policy_hash,
    },
    native_ids::{self, Kind},
};
use desk_agent_protocol::{
    ai_assistant::source::{SourcePolicyClass, SourceUsePolicy},
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
        Sensitivity,
    },
};
use sha2::{Digest, Sha256};

pub struct NativeStatusHistoryProjection {
    actor: String,
    device: String,
    conversation: String,
    native_id: String,
    content: String,
    metadata: NativeHistoryMetadata,
    policy: SourceUsePolicy,
    envelope: DataEnvelope,
}
impl NativeStatusHistoryProjection {
    pub fn new(
        actor: &str,
        device: &str,
        conversation: &str,
        original_rows_sha256: &str,
        snapshot: NativeHistorySnapshot,
        policy_revision: u64,
    ) -> Result<Self, &'static str> {
        for id in [actor, device, conversation] {
            validate_source_id(id)?;
        }
        if !valid_hash(original_rows_sha256) {
            return Err("invalid original native row digest");
        }
        snapshot.validate(snapshot.observed_at_unix_ms)?;
        let content =
            serde_json::to_string(&snapshot).map_err(|_| "invalid native status projection")?;
        let content_sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
        let native_id = native_ids::stable(
            Kind::Message,
            &(
                "native-status-v1",
                actor,
                device,
                conversation,
                original_rows_sha256,
                &content_sha256,
            ),
        );
        let metadata = NativeHistoryMetadata {
            format_version: 1,
            native_proof_sha256: original_rows_sha256.into(),
            projected_content_sha256: content_sha256.clone(),
            projection_partial: !snapshot.complete,
            time: None,
            status_snapshot: Some(snapshot),
        };
        metadata.validate(
            AssistantSourceKind::NativeStatus,
            &content_sha256,
            metadata
                .status_snapshot
                .as_ref()
                .unwrap()
                .observed_at_unix_ms,
        )?;
        let mut policy = owner_text_policy(policy_revision, false);
        policy.class = SourcePolicyClass::StructuredObservation;
        policy.independent_fact_allowed = false;
        policy.policy_sha256 = policy_hash(&policy)?;
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("native-status-{native_id}"),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("native-status-{native_id}"),
                sha256: content_sha256.clone(),
                size_bytes: content.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "assistant-native-status".into(),
                source_tool_name: "observe-native-status".into(),
                source_object_id: Some(native_id.clone()),
                source_envelope_ids: Vec::new(),
            },
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        };
        envelope
            .validate()
            .map_err(|_| "invalid native status envelope")?;
        Ok(Self {
            actor: actor.into(),
            device: device.into(),
            conversation: conversation.into(),
            native_id,
            content,
            metadata,
            policy,
            envelope,
        })
    }
    pub fn origin_id(&self) -> String {
        history_origin_id(
            &self.actor,
            &self.device,
            &self.conversation,
            &self.native_id,
            AssistantSourceKind::NativeStatus,
        )
    }
    pub fn source_id(&self) -> String {
        history_source_id(&self.origin_id(), 0)
    }
    pub fn capture<'a>(
        &'a self,
        accepted_input_seq: u64,
        directory: Option<HistorySourceDirectory<'a>>,
    ) -> HistoryCapture<'a> {
        HistoryCapture {
            native: Some(&self.metadata),
            actor_id: &self.actor,
            device_ref: &self.device,
            directory,
            conversation_id: &self.conversation,
            root_conversation_id: &self.conversation,
            native_id: &self.native_id,
            accepted_input_seq,
            kind: AssistantSourceKind::NativeStatus,
            role: "system",
            content: &self.content,
            envelope: &self.envelope,
            source_policy: &self.policy,
            dependencies: &[],
            recorded_at_unix_ms: self
                .metadata
                .status_snapshot
                .as_ref()
                .unwrap()
                .observed_at_unix_ms,
            statement_timezone: "UTC",
            revision: 1,
        }
    }
}
