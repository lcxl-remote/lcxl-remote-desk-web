//! Published task input keeps its occurrence identity instead of impersonating a new owner message.

use super::definition::ScheduledTaskDefinition;
use crate::ai_assistant::{
    source::{owner_text_policy, validate_history_dependencies, validate_policy},
    source_registry::NativeDerivedInputPolicy,
};
use crate::chat::{ChatMessage, ChatRole};
use desk_agent_protocol::ai_assistant::source::{HistoryDependency, SourceUsePolicy};
use desk_agent_protocol::{
    AgentError,
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PUBLISHED_INPUT_PROVIDER: &str = "assistant-published-task";
pub const PUBLISHED_INPUT_TOOL: &str = "scheduled-occurrence-input";

/// Frozen by the native task producer, never supplied by a client or model.
/// The original prompt is already in the task; this stores metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPromptRetention {
    pub actor_id: String,
    pub target_device_id: String,
    pub policy: SourceUsePolicy,
    pub dependencies: Vec<HistoryDependency>,
    pub model_source: Option<NativeDerivedInputPolicy>,
}

impl TaskPromptRetention {
    /// The retention record is written in the same row update as the prompt
    /// it describes, so it carries no prompt digest.
    pub fn owner(actor: &str, device: &str, revision: u64) -> Self {
        let mut policy = owner_text_policy(revision, false);
        // A published requirement is not a new assertion of a stable fact.
        policy.independent_fact_allowed = false;
        Self {
            actor_id: actor.into(),
            target_device_id: device.into(),
            policy,
            dependencies: Vec::new(),
            model_source: None,
        }
    }

    pub fn validate(&self, actor: &str, device: &str) -> Result<(), &'static str> {
        if self.actor_id != actor
            || self.target_device_id != device
            || actor.is_empty()
            || device.is_empty()
            || self.policy.independent_fact_allowed
            || self.dependencies.len() > 128
        {
            return Err("invalid original task prompt retention binding");
        }
        validate_policy(&self.policy)?;
        validate_history_dependencies(&self.dependencies)?;
        if let Some(parent) = &self.model_source {
            parent.validate()?;
            if parent.actor_id != actor {
                return Err("task prompt belongs to another actor");
            }
        } else if !self.dependencies.is_empty()
            || self.policy != Self::owner(actor, device, self.policy.policy_revision).policy
        {
            return Err("owner task prompt changed its original policy");
        }
        if crate::model_egress::sink_metadata_size(self)? > 64 * 1024 {
            return Err("task prompt retention exceeds its metadata bound");
        }
        Ok(())
    }
}

/// Shape registration is not native authority. The host must reconstruct the
/// full label from the original owner-bound occurrence and its task definition.
pub fn is_published_input_label(source: &DataEnvelope) -> bool {
    let Some(message_id) = source.provenance.source_object_id.as_deref() else {
        return false;
    };
    let Some(run_id) = message_id.strip_suffix(":input") else {
        return false;
    };
    let Some(key) = source.envelope_id.strip_prefix("published-task-input-") else {
        return false;
    };
    run_id.starts_with("schedule-run-")
        && crate::subagent::valid_id(message_id)
        && key.len() == 64
        && key.bytes().all(|byte| byte.is_ascii_hexdigit())
        && source.provenance.source_provider_id == PUBLISHED_INPUT_PROVIDER
        && source.provenance.source_tool_name == PUBLISHED_INPUT_TOOL
        && source.provenance.source_envelope_ids.is_empty()
        && source.sensitivity == Sensitivity::UserContent
        && source.retention
            == (RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            })
        && source.allowed_destinations.len() == 1
        && matches!(
            source.allowed_destinations[0],
            DestinationIdentity::Model { .. }
        )
        && matches!(&source.content, ContentRef::Message { message_id: content_id, size_bytes, media_type }
            if content_id == message_id && *size_bytes > 0
                && media_type == "text/plain;charset=utf-8")
}

/// The caller verifies current occurrence authority. This label
/// records provenance and the selected model; it grants no tool permission.
pub fn model_bound_published_input(
    run_id: &str,
    text: &str,
    definition: &ScheduledTaskDefinition,
    destination: DestinationIdentity,
) -> Result<ChatMessage, AgentError> {
    let denied = || crate::subagent::invalid("invalid published task input binding");
    let message_id = format!("{run_id}:input");
    if !run_id.starts_with("schedule-run-")
        || !crate::subagent::valid_id(&message_id)
        || text.trim().is_empty()
        || text.chars().count() > super::MAX_SCHEDULE_PROMPT_CHARS
        || !matches!(&destination, DestinationIdentity::Model { .. })
    {
        return Err(denied());
    }
    destination.validate().map_err(|_| denied())?;
    let identity = serde_json::to_vec(&(
        run_id,
        &definition.schedule_id,
        definition.task_revision,
        definition.policy_revision,
    ))
    .map_err(|_| denied())?;
    let key = format!("{:x}", Sha256::digest(identity));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("published-task-input-{key}"),
        content: ContentRef::Message {
            message_id: message_id.clone(),
            size_bytes: text.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: PUBLISHED_INPUT_PROVIDER.into(),
            source_tool_name: PUBLISHED_INPUT_TOOL.into(),
            source_object_id: Some(message_id.clone()),
            source_envelope_ids: Vec::new(),
        },
        sensitivity: Sensitivity::UserContent,
        allowed_destinations: vec![destination],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    };
    envelope.validate().map_err(|_| denied())?;
    let mut message =
        ChatMessage::text(message_id, ChatRole::User, text).with_turn_id(format!("{run_id}-turn"));
    message.data_envelope = Some(envelope);
    Ok(message)
}

/// Exact reconstruction prevents a regular owner message, a resume bridge or
/// another occurrence's label from being promoted to published task evidence.
pub fn validate_published_input(
    message: &ChatMessage,
    run_id: &str,
    definition: &ScheduledTaskDefinition,
    destination: &DestinationIdentity,
) -> Result<(), AgentError> {
    let expected =
        model_bound_published_input(run_id, &message.text, definition, destination.clone())?;
    if message != &expected {
        return Err(crate::subagent::invalid(
            "published task input provenance changed",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_requirement_is_retained_without_becoming_a_stable_fact() {
        let original = TaskPromptRetention::owner("1", "device", 2);
        original.validate("1", "device").unwrap();
        assert!(original.policy.history_retention_allowed);
        assert!(!original.policy.independent_fact_allowed);
        for changed in ["2", "device"] {
            assert!(original.validate(changed, "device").is_err());
        }
        assert!(original.validate("1", "another-device").is_err());
        let mut forged = original.clone();
        forged.policy.independent_fact_allowed = true;
        assert!(forged.validate("1", "device").is_err());
        let mut forged = original.clone();
        forged.policy.content_hard_expires_at_unix_ms = Some(10);
        assert!(forged.validate("1", "device").is_err());
    }

    #[test]
    fn task_input_shape_does_not_authorize_arbitrary_provider_or_owner_labels() {
        let key = "a".repeat(64);
        let source = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("published-task-input-{key}"),
            content: ContentRef::Message {
                message_id: "schedule-run-shape:input".into(),
                size_bytes: 4,
                media_type: "text/plain;charset=utf-8".into(),
            },
            provenance: DataProvenance {
                source_provider_id: PUBLISHED_INPUT_PROVIDER.into(),
                source_tool_name: PUBLISHED_INPUT_TOOL.into(),
                source_object_id: Some("schedule-run-shape:input".into()),
                source_envelope_ids: Vec::new(),
            },
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: vec![DestinationIdentity::Model {
                connection_id: "p".into(),
                connection_revision: 1,
                model_id: "m".into(),
                profile_revision: 1,
            }],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        };
        assert!(is_published_input_label(&source));
        let mut forged = source.clone();
        forged.provenance.source_provider_id = "ai-assistant-user".into();
        assert!(!is_published_input_label(&forged));
        let mut forged = source.clone();
        forged.provenance.source_object_id = Some("ordinary-chat:input".into());
        assert!(!is_published_input_label(&forged));
        let mut forged = source.clone();
        if let ContentRef::Message { message_id, .. } = &mut forged.content {
            *message_id = "schedule-run-other:input".into();
        }
        assert!(!is_published_input_label(&forged));
        let mut forged = source.clone();
        forged.retention.delete_with_run = false;
        assert!(!is_published_input_label(&forged));
        let mut forged = source.clone();
        forged
            .allowed_destinations
            .push(source.allowed_destinations[0].clone());
        assert!(!is_published_input_label(&forged));
        let mut forged = source.clone();
        forged
            .provenance
            .source_envelope_ids
            .push("hidden-parent".into());
        assert!(!is_published_input_label(&forged));
        let registered = crate::ai_assistant::source_registry::registered_leaf(
            &source,
            2,
            crate::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            registered.contract,
            crate::ai_assistant::source_registry::RegisteredSourceContract::PublishedTaskInput
        );
        assert!(!registered.policy.history_retention_allowed);
        assert!(!registered.policy.independent_fact_allowed);
    }
}
