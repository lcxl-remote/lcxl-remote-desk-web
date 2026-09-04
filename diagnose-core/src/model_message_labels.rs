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
use std::collections::BTreeSet;

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
            crate::device_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID
            | crate::device_assistant::WEB_RESEARCH_SEARCH_CAPABILITY_ID => Sensitivity::Public,
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

/// Runtime-authored tool closure inherits its parent authority and retention.
/// A missing label remains missing; this never grants export to a new sink.
pub fn internal_tool_result_envelope(
    parent: Option<&DataEnvelope>,
    call_id: &str,
    content: &str,
    source_tool_name: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    parent.validate().map_err(|_| invalid_label())?;
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    let mut retention = parent.retention;
    if let ContentRef::EphemeralObservation {
        expires_at_unix_ms, ..
    } = parent.content
    {
        retention = retention.most_restrictive(RetentionBoundary {
            expires_at_unix_ms: Some(expires_at_unix_ms),
            delete_with_run: true,
        });
    }
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!(
            "status-result-{:x}",
            Sha256::digest(format!("{}:{call_id}:{digest}", parent.envelope_id).as_bytes())
        ),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("status-content-{:x}", Sha256::digest(digest.as_bytes())),
            sha256: digest.clone(),
            size_bytes: content.len() as u64,
            media_type: "text/plain".into(),
        },
        provenance: DataProvenance {
            source_provider_id: crate::dynamic_run::RUN_CONTROL_PROVIDER_ID.into(),
            source_tool_name: source_tool_name.into(),
            source_object_id: None,
            source_envelope_ids: vec![parent.envelope_id.clone()],
        },
        digest_sha256: digest,
        sensitivity: parent.sensitivity,
        allowed_destinations: parent.allowed_destinations.clone(),
        retention,
    };
    envelope.validate().map_err(|_| invalid_label())?;
    Ok(Some(envelope))
}

/// A history page is a deterministic projection of several older message
/// envelopes plus the model's current tool call. Its authority is the strict
/// intersection of every input destination, its sensitivity is the maximum,
/// and its retention is the most restrictive input boundary. This makes a
/// model/profile switch or expired source fail at the ordinary egress gate.
pub fn conversation_history_result_envelope(
    parent: Option<&DataEnvelope>,
    source_messages: &[ChatMessage],
    call_id: &str,
    content: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    let mut inputs = Vec::new();
    if let Some(parent) = parent {
        inputs.push(parent);
    }
    inputs.extend(
        source_messages
            .iter()
            .filter_map(|message| message.data_envelope.as_ref()),
    );
    if inputs.is_empty() {
        return Ok(None);
    }
    for input in &inputs {
        input.validate().map_err(|_| invalid_label())?;
    }
    let mut source_envelope_ids = inputs
        .iter()
        .map(|input| input.envelope_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if source_envelope_ids.len() > desk_agent_protocol::data_lineage::MAX_LINEAGE_ITEMS {
        return Err(invalid_label());
    }
    source_envelope_ids.sort();
    let sensitivity = inputs
        .iter()
        .map(|input| input.sensitivity)
        .max()
        .unwrap_or(Sensitivity::Sensitive);
    let retention = inputs
        .iter()
        .skip(1)
        .fold(inputs[0].retention, |current, input| {
            current.most_restrictive(input.retention)
        });
    let mut allowed = inputs[0]
        .allowed_destinations
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for input in inputs.iter().skip(1) {
        let destinations = input
            .allowed_destinations
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        allowed = allowed.intersection(&destinations).cloned().collect();
    }
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!(
            "history-result-{:x}",
            Sha256::digest(format!("{call_id}:{digest}").as_bytes())
        ),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("history-content-{:x}", Sha256::digest(digest.as_bytes())),
            sha256: digest.clone(),
            size_bytes: content.len() as u64,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: crate::dynamic_run::RUN_CONTROL_PROVIDER_ID.into(),
            source_tool_name: crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME
                .into(),
            source_object_id: None,
            source_envelope_ids,
        },
        digest_sha256: digest,
        sensitivity,
        allowed_destinations: allowed.into_iter().collect(),
        retention,
    };
    envelope.validate().map_err(|_| invalid_label())?;
    Ok(Some(envelope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_result_never_renews_ephemeral_parent_or_mints_missing_authority() {
        assert!(
            internal_tool_result_envelope(None, "call", "unavailable", "supersede_tool_call")
                .unwrap()
                .is_none()
        );
        let mut parent = model_bound_user_message(
            "source".into(),
            "input".into(),
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
        parent.content = ContentRef::EphemeralObservation {
            observation_id: "observation".into(),
            size_bytes: 5,
            expires_at_unix_ms: 100,
        };
        let result = internal_tool_result_envelope(
            Some(&parent),
            "call",
            "unavailable",
            "supersede_tool_call",
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.retention.expires_at_unix_ms, Some(100));
        assert!(result.retention.delete_with_run);
        assert_eq!(result.allowed_destinations, parent.allowed_destinations);
        assert_eq!(
            result.provenance.source_envelope_ids,
            vec![parent.envelope_id.clone()]
        );
        parent.digest_sha256 = "bad".into();
        assert!(
            internal_tool_result_envelope(
                Some(&parent),
                "call",
                "unavailable",
                "supersede_tool_call"
            )
            .is_err()
        );
    }

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

    #[test]
    fn history_result_intersects_destinations_and_keeps_restrictive_retention() {
        fn destination(model_id: &str) -> DestinationIdentity {
            DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: model_id.into(),
                profile_revision: 1,
            }
        }

        let model_a = destination("model-a");
        let model_b = destination("model-b");
        let mut first =
            model_bound_user_message("first".into(), "first question".into(), model_a.clone())
                .unwrap();
        first.data_envelope.as_mut().unwrap().allowed_destinations = vec![model_a.clone(), model_b];
        first.data_envelope.as_mut().unwrap().retention = RetentionBoundary {
            expires_at_unix_ms: Some(500),
            delete_with_run: false,
        };
        let mut second =
            model_bound_user_message("second".into(), "second question".into(), model_a.clone())
                .unwrap();
        second.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Sensitive;
        second.data_envelope.as_mut().unwrap().retention = RetentionBoundary {
            expires_at_unix_ms: Some(300),
            delete_with_run: true,
        };

        let envelope = conversation_history_result_envelope(
            None,
            &[first, second],
            "history-call",
            r#"{"messages":[]}"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(envelope.allowed_destinations, vec![model_a]);
        assert_eq!(envelope.sensitivity, Sensitivity::Sensitive);
        assert_eq!(envelope.retention.expires_at_unix_ms, Some(300));
        assert!(envelope.retention.delete_with_run);
        assert_eq!(envelope.provenance.source_envelope_ids.len(), 2);
    }
}
