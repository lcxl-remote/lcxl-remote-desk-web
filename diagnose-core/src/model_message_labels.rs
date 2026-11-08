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

pub const STATUS_RESULT_PROJECTION: &str = "status-result";
pub const SOURCE_RESULT_PROJECTION: &str = "source-result";

pub fn model_bound_user_message(
    message_id: String,
    text: String,
    destination: DestinationIdentity,
) -> Result<ChatMessage, AgentError> {
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("user-message-{message_id}"),
        content: ContentRef::Message {
            message_id: message_id.clone(),
            size_bytes: text.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "ai-assistant-user".into(),
            source_tool_name: "send-message".into(),
            source_object_id: Some(message_id.clone()),
            source_envelope_ids: Vec::new(),
        },
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
        message: format!("failed to label AI Assistant input: {error}"),
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
    let digest_sha256 = format!("{:x}", Sha256::digest(&payload));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: label.envelope_id,
        content: ContentRef::ImmutableBlob {
            blob_id: format!("native-read-snapshot-{}", label.observation_id),
            sha256: digest_sha256.clone(),
            size_bytes: payload.len() as u64,
            media_type: "application/octet-stream".into(),
        },
        provenance: DataProvenance {
            source_provider_id: provider.wire.provider_id.clone(),
            source_tool_name: call.name.clone(),
            source_object_id: label.source_object_id,
            source_envelope_ids: Vec::new(),
        },
        sensitivity: match capability.wire.capability_id.as_str() {
            crate::ai_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID
            | crate::ai_assistant::WEB_RESEARCH_SEARCH_CAPABILITY_ID => Sensitivity::Public,
            crate::ai_assistant::DESKTOP_SESSION_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_INFO_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_NETWORK_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_SERVICE_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_CONTAINER_CAPABILITY_ID => Sensitivity::UserContent,
            _ => Sensitivity::Sensitive,
        },
        // Reading never implicitly grants export to any model or other sink.
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
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
/// Label a persisted runtime status result. One call has one result under a
/// given parent label and tool, and in-place rewrites derive from the current
/// label, so the id needs no body digest.
pub fn internal_tool_result_envelope(
    parent: Option<&DataEnvelope>,
    call_id: &str,
    content: &str,
    source_tool_name: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    status_envelope(parent, call_id, content, source_tool_name, false)
}

/// Label a runtime projection that exists only in one model request. Its body
/// is regenerated per request under a fixed call id, so the id names the
/// body revision by digest.
pub fn request_projection_envelope(
    parent: Option<&DataEnvelope>,
    call_id: &str,
    content: &str,
    source_tool_name: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    status_envelope(parent, call_id, content, source_tool_name, true)
}

fn status_envelope(
    parent: Option<&DataEnvelope>,
    call_id: &str,
    content: &str,
    source_tool_name: &str,
    content_addressed: bool,
) -> Result<Option<DataEnvelope>, AgentError> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    parent.validate().map_err(|_| invalid_label())?;
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
    let (identity, content) = if content_addressed {
        let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
        (
            format!("{}:{call_id}:{digest}", parent.envelope_id),
            ContentRef::ImmutableBlob {
                blob_id: format!("status-content-{:x}", Sha256::digest(digest.as_bytes())),
                sha256: digest,
                size_bytes: content.len() as u64,
                media_type: "text/plain".into(),
            },
        )
    } else {
        (
            serde_json::to_string(&(&parent.envelope_id, call_id, source_tool_name))
                .map_err(|_| invalid_label())?,
            ContentRef::Projection {
                projection: STATUS_RESULT_PROJECTION.into(),
                size_bytes: content.len() as u64,
                media_type: "text/plain".into(),
            },
        )
    };
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("status-result-{:x}", Sha256::digest(identity.as_bytes())),
        content,
        provenance: DataProvenance {
            source_provider_id: crate::dynamic_run::RUN_CONTROL_PROVIDER_ID.into(),
            source_tool_name: source_tool_name.into(),
            source_object_id: None,
            source_envelope_ids: vec![parent.envelope_id.clone()],
        },
        sensitivity: parent.sensitivity,
        allowed_destinations: parent.allowed_destinations.clone(),
        retention,
    };
    envelope.validate().map_err(|_| invalid_label())?;
    Ok(Some(envelope))
}

/// Replace a persisted message body in place. The new label derives from the
/// current label and the rewrite kind, so one envelope id never names two
/// bodies, including when rewrites return to an earlier body. On error the
/// message is left unchanged.
pub(crate) fn rewrite_message_content(
    message: &mut ChatMessage,
    call_id: &str,
    text: &str,
    rewrite_kind: &str,
) -> Result<(), AgentError> {
    let envelope =
        internal_tool_result_envelope(message.data_envelope.as_ref(), call_id, text, rewrite_kind)?;
    message.text = text.into();
    message.data_envelope = envelope;
    Ok(())
}

/// A history page is a deterministic projection of several older message
/// envelopes plus the model's current tool call. Its authority is the strict
/// intersection of every input destination, its sensitivity is the maximum,
/// and operation-reference deadlines remain in its lineage metadata. A model
/// switch still requires matching disclosure authority; elapsed time does not
/// invalidate the historical page for model recollection.
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
    source_result_envelope(
        &inputs,
        call_id,
        content,
        crate::dynamic_run::RUN_CONTROL_PROVIDER_ID,
        crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME,
        None,
    )
}

pub fn assistant_data_result_envelope(
    parent: Option<&DataEnvelope>,
    sources: &[DataEnvelope],
    call_id: &str,
    content: &str,
    tool_name: &str,
    episode_id: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    if let [snapshot] = sources
        && snapshot.provenance.source_provider_id == "assistant-conversation-snapshot"
    {
        return Ok(Some(snapshot.clone()));
    }
    let mut inputs = Vec::with_capacity(sources.len() + 1);
    if let Some(parent) = parent {
        inputs.push(parent);
    }
    inputs.extend(sources);
    source_result_envelope(
        &inputs,
        call_id,
        content,
        "assistant-data-result",
        tool_name,
        Some(episode_id),
    )
}

fn source_result_envelope(
    inputs: &[&DataEnvelope],
    call_id: &str,
    content: &str,
    provider: &str,
    tool_name: &str,
    object: Option<&str>,
) -> Result<Option<DataEnvelope>, AgentError> {
    source_result_identity(
        inputs,
        call_id,
        content.len() as u64,
        provider,
        tool_name,
        object,
    )
}

/// Reconstruct an original tool-result label from immutable projection metadata,
/// without loading or retaining the old result body. This grants no source use.
pub(crate) fn retained_assistant_data_result(
    parent: &DataEnvelope,
    source: &DataEnvelope,
    call_id: &str,
    tool_name: &str,
    episode_id: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    source.validate().map_err(|_| invalid_label())?;
    source_result_identity(
        &[parent, source],
        call_id,
        source.content.size_bytes(),
        "assistant-data-result",
        tool_name,
        Some(episode_id),
    )
}

fn source_result_identity(
    inputs: &[&DataEnvelope],
    call_id: &str,
    size_bytes: u64,
    provider: &str,
    tool_name: &str,
    object: Option<&str>,
) -> Result<Option<DataEnvelope>, AgentError> {
    if inputs.is_empty() {
        return Ok(None);
    }
    for input in inputs {
        input.validate().map_err(|_| invalid_label())?;
    }
    let mut by_id = std::collections::BTreeMap::new();
    for input in inputs {
        if let Some(previous) = by_id.insert(input.envelope_id.as_str(), *input)
            && previous != *input
        {
            return Err(invalid_label());
        }
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
    let effective_retention = |input: &DataEnvelope| {
        if let ContentRef::EphemeralObservation {
            expires_at_unix_ms, ..
        } = input.content
        {
            input.retention.most_restrictive(RetentionBoundary {
                expires_at_unix_ms: Some(expires_at_unix_ms),
                delete_with_run: true,
            })
        } else {
            input.retention
        }
    };
    let retention = inputs
        .iter()
        .skip(1)
        .fold(effective_retention(inputs[0]), |current, input| {
            current.most_restrictive(effective_retention(input))
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
    let identity = serde_json::to_vec(&(
        provider,
        tool_name,
        object,
        call_id,
        by_id.values().collect::<Vec<_>>(),
    ))
    .map_err(|_| invalid_label())?;
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("source-result-{:x}", Sha256::digest(identity)),
        content: ContentRef::Projection {
            projection: SOURCE_RESULT_PROJECTION.into(),
            size_bytes,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: provider.into(),
            source_tool_name: tool_name.into(),
            source_object_id: object.map(str::to_owned),
            source_envelope_ids,
        },
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

    fn source(id: &str) -> DataEnvelope {
        model_bound_user_message(
            id.into(),
            "source text".into(),
            DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
        .data_envelope
        .unwrap()
    }

    #[test]
    fn rewrites_never_reuse_an_envelope_id_for_another_body() {
        let mut message = ChatMessage::tool_result("result", "call", "original");
        message.data_envelope = Some(source("owner"));
        let mut seen = std::collections::BTreeSet::new();
        seen.insert(message.data_envelope.clone().unwrap().envelope_id);
        // A -> B -> A still yields a fresh label at every step.
        for (text, kind) in [
            ("unknown", "execution_outcome_unknown"),
            ("disposed", "manual_outcome_disposition"),
            ("unknown", "execution_outcome_unknown"),
        ] {
            rewrite_message_content(&mut message, "call", text, kind).unwrap();
            assert_eq!(message.text, text);
            assert!(seen.insert(message.data_envelope.clone().unwrap().envelope_id));
        }
        let mut invalid = message.clone();
        invalid
            .data_envelope
            .as_mut()
            .unwrap()
            .allowed_destinations
            .clear();
        invalid.data_envelope.as_mut().unwrap().envelope_id.clear();
        let before = invalid.clone();
        assert!(rewrite_message_content(&mut invalid, "call", "other", "kind").is_err());
        assert_eq!(
            invalid, before,
            "a failed rewrite leaves the message unchanged"
        );
    }

    #[test]
    fn source_result_identity_binds_purpose_episode_and_complete_parent_proofs() {
        let a = source("a");
        let b = source("b");
        let result = assistant_data_result_envelope(
            None,
            &[a.clone(), b.clone()],
            "call",
            "{}",
            "search_memories",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        let reordered = assistant_data_result_envelope(
            None,
            &[b.clone(), a.clone(), a.clone()],
            "call",
            "{}",
            "search_memories",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(result, reordered);
        let other_tool = assistant_data_result_envelope(
            None,
            &[a.clone(), b.clone()],
            "call",
            "{}",
            "search_conversations",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        let other_episode = assistant_data_result_envelope(
            None,
            &[a.clone(), b.clone()],
            "call",
            "{}",
            "search_memories",
            "episode-2",
        )
        .unwrap()
        .unwrap();
        assert_ne!(result.envelope_id, other_tool.envelope_id);
        assert_ne!(result.envelope_id, other_episode.envelope_id);
        let mut restricted = a.clone();
        restricted.allowed_destinations.clear();
        let changed = assistant_data_result_envelope(
            None,
            &[restricted.clone(), b],
            "call",
            "{}",
            "search_memories",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        assert_ne!(result.envelope_id, changed.envelope_id);
        assert!(changed.allowed_destinations.is_empty());
        assert!(
            assistant_data_result_envelope(
                None,
                &[a, restricted],
                "call",
                "{}",
                "search_memories",
                "episode-1"
            )
            .is_err()
        );
    }

    #[test]
    fn original_assistant_result_identity_survives_body_cleanup_and_binds_all_original_metadata() {
        let parent = source("producer");
        let read = source("projection");
        let original = assistant_data_result_envelope(
            Some(&parent),
            std::slice::from_ref(&read),
            "read-call",
            "source text",
            "search_memories",
            "original-episode",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            retained_assistant_data_result(
                &parent,
                &read,
                "read-call",
                "search_memories",
                "original-episode"
            )
            .unwrap(),
            Some(original.clone())
        );
        for case in 0..7 {
            let mut producer = parent.clone();
            let mut projection = read.clone();
            let mut call = "read-call";
            let mut tool = "search_memories";
            let mut episode = "original-episode";
            match case {
                0 => producer.allowed_destinations.clear(),
                1 => producer.retention.expires_at_unix_ms = Some(5000),
                2 => projection.provenance.source_object_id = Some("other-object".into()),
                3 => call = "other-call",
                4 => tool = "search_conversations",
                5 => episode = "other-episode",
                _ => {
                    if let ContentRef::ImmutableBlob { size_bytes, .. }
                    | ContentRef::Projection { size_bytes, .. }
                    | ContentRef::Message { size_bytes, .. } = &mut projection.content
                    {
                        *size_bytes += 1;
                    }
                }
            }
            assert_ne!(
                retained_assistant_data_result(&producer, &projection, call, tool, episode)
                    .unwrap(),
                Some(original.clone()),
                "case {case}"
            );
        }
    }

    #[test]
    fn history_and_memory_results_preserve_ephemeral_content_deadlines() {
        let mut input = source("ephemeral");
        input.content = ContentRef::EphemeralObservation {
            observation_id: "obs".into(),
            size_bytes: 11,
            expires_at_unix_ms: 100,
        };
        input.retention = RetentionBoundary {
            expires_at_unix_ms: Some(500),
            delete_with_run: false,
        };
        let result = assistant_data_result_envelope(
            None,
            std::slice::from_ref(&input),
            "call",
            "{}",
            "search_memories",
            "episode",
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.retention.expires_at_unix_ms, Some(100));
        assert!(result.retention.delete_with_run);
        let mut message = ChatMessage::text("message", ChatRole::User, "source text");
        message.data_envelope = Some(input);
        let history = conversation_history_result_envelope(None, &[message], "call", "{}")
            .unwrap()
            .unwrap();
        assert_eq!(history.retention, result.retention);
        assert_ne!(history.envelope_id, result.envelope_id);
    }

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
        parent.envelope_id.clear();
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
            format: crate::seam::ToolOutputFormat::Text,
            content: "desktop metadata".into(),
            image_data_url: None,
            document_preview: None,
        };
        let envelope = read_result_envelope(
            &crate::ai_assistant::ai_assistant_provider_registry(),
            &call("inspect_desktop_session"),
            &output,
            label(),
        )
        .unwrap();
        assert!(envelope.allowed_destinations.is_empty());
        assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
        assert_eq!(envelope.retention.expires_at_unix_ms, None);
        assert!(envelope.retention.delete_with_run);
        assert_eq!(
            envelope.content_sha256(),
            Some(format!("{:x}", Sha256::digest(output.content.as_bytes())).as_str())
        );
    }

    #[test]
    fn unknown_tool_and_empty_result_do_not_get_labels() {
        let registry = crate::ai_assistant::ai_assistant_provider_registry();
        assert!(
            read_result_envelope(
                &registry,
                &call("invented-tool"),
                &ToolRunOutput {
                    format: crate::seam::ToolOutputFormat::Text,
                    content: "data".into(),
                    image_data_url: None,
                    document_preview: None,
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
