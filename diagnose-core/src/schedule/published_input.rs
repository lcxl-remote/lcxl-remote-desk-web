//! Published task input keeps its occurrence identity instead of impersonating a new owner message.

use super::contract::ValidatedTaskContract;
use crate::chat::{ChatMessage, ChatRole};
use desk_agent_protocol::{
    AgentError,
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
};
use sha2::{Digest, Sha256};

pub const PUBLISHED_INPUT_PROVIDER: &str = "assistant-published-task";
pub const PUBLISHED_INPUT_TOOL: &str = "scheduled-occurrence-input";

/// The caller verifies current publication and occurrence authority. This label
/// records provenance and the selected model; it grants no tool permission.
pub fn model_bound_published_input(
    run_id: &str,
    text: &str,
    contract: &ValidatedTaskContract,
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
    let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    if digest != contract.contract().prompt_sha256 {
        return Err(denied());
    }
    let identity =
        serde_json::to_vec(&(run_id, &contract.contract().schedule_id, contract.digest()))
            .map_err(|_| denied())?;
    let key = format!("{:x}", Sha256::digest(identity));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("published-task-input-{key}"),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("published-task-content-{key}"),
            sha256: digest.clone(),
            size_bytes: text.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: PUBLISHED_INPUT_PROVIDER.into(),
            source_tool_name: PUBLISHED_INPUT_TOOL.into(),
            source_object_id: Some(message_id.clone()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: digest,
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
    contract: &ValidatedTaskContract,
    destination: &DestinationIdentity,
) -> Result<(), AgentError> {
    let expected =
        model_bound_published_input(run_id, &message.text, contract, destination.clone())?;
    if message != &expected {
        return Err(crate::subagent::invalid(
            "published task input provenance changed",
        ));
    }
    Ok(())
}
