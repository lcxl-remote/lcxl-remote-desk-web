//! Shared provenance labels for user messages and selected Provider read results.

use crate::{
    chat::{ChatMessage, ChatRole, ToolCall},
    provider_registry::ProviderRegistry,
    seam::ToolRunOutput,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
};
use sha2::{Digest, Sha256};

pub fn model_bound_user_message(
    message_id: String,
    text: String,
    destination: DestinationIdentity,
) -> Result<ChatMessage, AgentError> {
    let bytes = text.as_bytes();
    let digest_sha256 = format!("{:x}", Sha256::digest(bytes));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("user-message-{message_id}"),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("session-message-{message_id}"),
            sha256: digest_sha256.clone(),
            size_bytes: bytes.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "device-assistant-user".into(),
            source_tool_name: "send-message".into(),
            source_object_id: Some(message_id.clone()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256,
        sensitivity: Sensitivity::UserContent,
        // Pressing Send explicitly targets the currently resolved model. A
        // different gateway/model revision produces a different identity and
        // cannot reuse this allowance.
        allowed_destinations: vec![destination],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        },
    };
    envelope.validate().map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to label Device Assistant input: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let mut message = ChatMessage::text(message_id, ChatRole::User, text);
    message.data_envelope = Some(envelope);
    Ok(message)
}

pub struct ReadResultLabel {
    pub envelope_id: String,
    pub observation_id: String,
    pub source_object_id: Option<String>,
    pub observed_at_unix_ms: u64,
}

pub fn read_result_envelope(
    registry: &ProviderRegistry,
    call: &ToolCall,
    output: &ToolRunOutput,
    label: ReadResultLabel,
) -> Result<DataEnvelope, AgentError> {
    let capability = registry
        .capability_for_tool(&call.name)
        .ok_or_else(invalid_label)?;
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .ok_or_else(invalid_label)?;
    let payload = crate::model_egress::message_payload_bytes(
        &output.content,
        output.image_data_url.as_deref(),
    )
    .map_err(|_| invalid_label())?;
    if payload.is_empty() || label.observed_at_unix_ms == 0 {
        return Err(invalid_label());
    }
    let expires_at_unix_ms = label.observed_at_unix_ms.saturating_add(5 * 60 * 1000);
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: label.envelope_id,
        content: ContentRef::EphemeralObservation {
            observation_id: label.observation_id,
            size_bytes: payload.len() as u64,
            expires_at_unix_ms,
        },
        provenance: DataProvenance {
            source_provider_id: provider.wire.provider_id.clone(),
            source_tool_name: call.name.clone(),
            source_object_id: label.source_object_id,
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: format!("{:x}", Sha256::digest(&payload)),
        sensitivity: match capability.wire.capability_id.as_str() {
            crate::device_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID => Sensitivity::Public,
            crate::device_assistant::DESKTOP_SESSION_CAPABILITY_ID
            | crate::device_assistant::SYSTEM_INFO_CAPABILITY_ID
            | crate::device_assistant::SYSTEM_NETWORK_CAPABILITY_ID
            | crate::device_assistant::SYSTEM_SERVICE_CAPABILITY_ID
            | crate::device_assistant::SYSTEM_CONTAINER_CAPABILITY_ID => Sensitivity::UserContent,
            _ => Sensitivity::Sensitive,
        },
        // Reading never implicitly grants export to any model or other sink.
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(expires_at_unix_ms),
            delete_with_run: true,
        },
    };
    envelope.validate().map_err(|_| invalid_label())?;
    Ok(envelope)
}

fn invalid_label() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "Provider result cannot be labeled safely".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments_json: "{}".into(),
        }
    }
    fn label() -> ReadResultLabel {
        ReadResultLabel {
            envelope_id: "result-1".into(),
            observation_id: "observation-1".into(),
            source_object_id: Some("device-1:call-1".into()),
            observed_at_unix_ms: 100,
        }
    }

    #[test]
    fn read_label_is_bounded_and_never_grants_export() {
        let output = ToolRunOutput {
            content: "desktop metadata".into(),
            image_data_url: None,
        };
        let envelope = read_result_envelope(
            &crate::device_assistant::device_assistant_provider_registry(),
            &call("inspect_desktop_session"),
            &output,
            label(),
        )
        .unwrap();
        assert!(envelope.allowed_destinations.is_empty());
        assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
        assert_eq!(envelope.retention.expires_at_unix_ms, Some(300_100));
        assert!(envelope.retention.delete_with_run);
        assert_eq!(
            envelope.digest_sha256,
            format!("{:x}", Sha256::digest(output.content.as_bytes()))
        );
    }

    #[test]
    fn unknown_tool_and_empty_result_do_not_get_labels() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        assert!(
            read_result_envelope(
                &registry,
                &call("invented-tool"),
                &ToolRunOutput {
                    content: "data".into(),
                    image_data_url: None
                },
                label()
            )
            .is_err()
        );
        assert!(
            read_result_envelope(
                &registry,
                &call("inspect_desktop_session"),
                &ToolRunOutput::default(),
                label()
            )
            .is_err()
        );
    }

    #[test]
    fn user_label_targets_only_the_resolved_model() {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 2,
        };
        let user = model_bound_user_message(
            "user-1".into(),
            "private question".into(),
            destination.clone(),
        )
        .unwrap();
        let envelope = user.data_envelope.unwrap();
        assert_eq!(envelope.allowed_destinations, vec![destination]);
        assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
        assert_eq!(envelope.provenance.source_tool_name, "send-message");
    }
}
