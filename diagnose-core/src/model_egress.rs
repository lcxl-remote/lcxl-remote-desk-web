//! Fail-closed projection of Device Assistant messages into one exact model sink.
//!
//! Provider dialects are deliberately downstream of this module. A caller must
//! first authorize every dynamic message payload, then hand the returned request
//! to the HTTP model seam. The `data_envelope` field itself is server metadata and
//! is never serialized by a provider adapter.

use std::collections::{BTreeSet, HashSet};

use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::chat::{ChatMessage, ChatRole, ModelTurn, ToolCall, ToolCallRef};
use crate::seam::ModelRequest;
use crate::sink_authorizer::{
    DefaultSinkAuthorizer, ExportDataAuthorization, MAX_SINK_BYTES, SinkAuthorizationError,
    SinkAuthorizer, SinkInput, SinkProjectionAudit, authorize_export,
};

const MODEL_OUTPUT_TTL_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct ModelEgressPolicy {
    pub destination: DestinationIdentity,
    /// Server-authoritative read tool names selected by the user for this turn.
    pub selected_source_tools: BTreeSet<String>,
    /// Stable id for the user's explicit context-selection + send action.
    pub export_authorization_id: String,
    pub now_unix_ms: u64,
    pub byte_cap: usize,
}

#[derive(Debug, Clone)]
pub struct AuthorizedModelRequest {
    pub request: ModelRequest,
    pub audit: SinkProjectionAudit,
    /// Exact projected inputs used to conservatively label the model output.
    pub input_envelopes: Vec<DataEnvelope>,
}

impl ModelEgressPolicy {
    pub fn authorize_request(
        &self,
        mut request: ModelRequest,
    ) -> Result<AuthorizedModelRequest, ModelEgressError> {
        self.destination
            .validate()
            .map_err(|error| ModelEgressError::InvalidPolicy(error.to_string()))?;
        if self.export_authorization_id.trim().is_empty()
            || self.byte_cap == 0
            || self.byte_cap > MAX_SINK_BYTES
        {
            return Err(ModelEgressError::InvalidPolicy(
                "invalid export authorization id or byte cap".into(),
            ));
        }

        // The browser-visible transcript remains intact, but a removed context
        // authorization must also remove prior turns derived from that context
        // from the next provider request. Dropping the complete turn keeps tool
        // call/result grouping valid and prevents a prior model answer from
        // becoming an indirect replay path for deselected device data.
        let tool_call_turns = request
            .messages
            .iter()
            .filter_map(|message| message.turn_id.as_ref().map(|turn_id| (message, turn_id)))
            .flat_map(|(message, turn_id)| {
                message
                    .tool_calls
                    .iter()
                    .map(move |call| (call.id.clone(), turn_id.clone()))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let deselected_turns = request
            .messages
            .iter()
            .filter(|message| message.role == ChatRole::Tool)
            .filter_map(|message| {
                let envelope = message.data_envelope.as_ref()?;
                // Server-owned run-control results (for example the advisory
                // task-status projection) contain no newly read device data.
                // They inherit an already-authorized model envelope and must not
                // be mistaken for a deselected Provider source: doing so would
                // prune the *current* follow-up turn after its first internal
                // tool call and resurrect an older user request.
                (envelope.provenance.source_provider_id
                    != crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
                    && !self
                        .selected_source_tools
                        .contains(&envelope.provenance.source_tool_name))
                .then_some(message.turn_id.as_ref().or_else(|| {
                    message
                        .tool_call_id
                        .as_ref()
                        .and_then(|call_id| tool_call_turns.get(call_id))
                }))
                .flatten()
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        // Ephemeral observations and answers derived from them must not be
        // replayed after their retention boundary.  Keep the browser-visible
        // transcript intact, but omit the complete *historical* turn from this
        // provider request so tool call/result grouping remains valid.  The
        // current turn is deliberately never omitted here: if its selected
        // observation expires while the loop is running, the sink authorizer
        // below still fails closed instead of silently answering without it.
        let current_turn_id = request
            .messages
            .iter()
            .rev()
            .find_map(|message| message.turn_id.clone());
        let expired_historical_turns = request
            .messages
            .iter()
            .filter_map(|message| {
                let envelope = message.data_envelope.as_ref()?;
                let turn_id = message.turn_id.as_ref().or_else(|| {
                    message
                        .tool_call_id
                        .as_ref()
                        .and_then(|call_id| tool_call_turns.get(call_id))
                })?;
                (current_turn_id.as_ref() != Some(turn_id)
                    && envelope_is_expired(envelope, self.now_unix_ms))
                .then_some(turn_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let omitted_turns = deselected_turns
            .union(&expired_historical_turns)
            .cloned()
            .collect::<BTreeSet<_>>();
        if !omitted_turns.is_empty() {
            request.messages.retain(|message| {
                let turn_id = message.turn_id.as_ref().or_else(|| {
                    message
                        .tool_call_id
                        .as_ref()
                        .and_then(|call_id| tool_call_turns.get(call_id))
                });
                turn_id.is_none_or(|turn_id| !omitted_turns.contains(turn_id))
            });
        }

        let mut projected_envelopes = Vec::with_capacity(request.messages.len());
        let mut payloads = Vec::with_capacity(request.messages.len());
        for message in &mut request.messages {
            let bytes = message_content_bytes(message)?;
            let source = match message.data_envelope.clone() {
                Some(envelope) => envelope,
                None if message.role == ChatRole::System => {
                    self.public_system_envelope(&message.message_id, &bytes)?
                }
                None => {
                    return Err(ModelEgressError::MissingEnvelope {
                        message_id: message.message_id.clone(),
                        role: message.role,
                    });
                }
            };

            if message.image_data_url.is_some()
                && (message.role != ChatRole::Tool
                    || source.provenance.source_tool_name != "read_current_screen"
                    || !self.selected_source_tools.contains("read_current_screen")
                    || source.sensitivity != Sensitivity::Sensitive)
            {
                return Err(ModelEgressError::ImageNotSupported);
            }

            let projected = if source
                .allowed_destinations
                .iter()
                .any(|destination| destination == &self.destination)
            {
                source
            } else {
                if message.role != ChatRole::Tool
                    || !self
                        .selected_source_tools
                        .contains(&source.provenance.source_tool_name)
                {
                    return Err(ModelEgressError::ExportNotSelected {
                        message_id: message.message_id.clone(),
                        source_tool_name: source.provenance.source_tool_name,
                    });
                }
                let authorization = ExportDataAuthorization {
                    authorization_id: self.export_authorization_id.clone(),
                    source_envelope_ids: vec![source.envelope_id.clone()],
                    destination: self.destination.clone(),
                    max_sensitivity: Sensitivity::Sensitive,
                    expires_at_unix_ms: self.now_unix_ms.saturating_add(MODEL_OUTPUT_TTL_MS),
                    max_bytes: u64::try_from(self.byte_cap)
                        .map_err(|_| ModelEgressError::InvalidPolicy("byte cap overflow".into()))?,
                };
                let exported_id = format!(
                    "model-export-{}",
                    short_digest(
                        format!(
                            "{}:{}:{}",
                            authorization.authorization_id, source.envelope_id, message.message_id
                        )
                        .as_bytes()
                    )
                );
                authorize_export(&source, &exported_id, &authorization, self.now_unix_ms)
                    .map_err(ModelEgressError::Sink)?
                    .0
            };
            message.data_envelope = Some(projected.clone());
            projected_envelopes.push(projected);
            payloads.push(bytes);
        }

        let inputs = projected_envelopes
            .iter()
            .zip(payloads.iter())
            .map(|(envelope, bytes)| SinkInput {
                envelope,
                bytes: bytes.as_slice(),
            })
            .collect::<Vec<_>>();
        let projection = DefaultSinkAuthorizer
            .authorize(&self.destination, &inputs, self.now_unix_ms, self.byte_cap)
            .map_err(ModelEgressError::Sink)?;

        Ok(AuthorizedModelRequest {
            request,
            audit: projection.audit,
            input_envelopes: projected_envelopes,
        })
    }

    pub fn derive_model_output_envelope(
        &self,
        turn: &ModelTurn,
        inputs: &[DataEnvelope],
    ) -> Result<DataEnvelope, ModelEgressError> {
        if inputs.is_empty() {
            return Err(ModelEgressError::EmptyInputs);
        }
        let bytes = model_turn_content_bytes(turn)?;
        if bytes.is_empty() {
            return Err(ModelEgressError::EmptyModelOutput);
        }
        let sensitivity = inputs
            .iter()
            .map(|input| input.sensitivity)
            .max()
            .ok_or(ModelEgressError::EmptyInputs)?;

        let mut allowed: BTreeSet<DestinationIdentity> =
            inputs[0].allowed_destinations.iter().cloned().collect();
        for input in &inputs[1..] {
            let next: BTreeSet<_> = input.allowed_destinations.iter().cloned().collect();
            allowed = allowed.intersection(&next).cloned().collect();
        }
        if !allowed.contains(&self.destination) {
            return Err(ModelEgressError::DerivedDestinationLost);
        }

        let mut source_ids = Vec::with_capacity(inputs.len());
        let mut seen = HashSet::new();
        let mut retention = RetentionBoundary {
            expires_at_unix_ms: Some(self.now_unix_ms.saturating_add(MODEL_OUTPUT_TTL_MS)),
            delete_with_run: true,
        };
        for input in inputs {
            if seen.insert(input.envelope_id.as_str()) {
                source_ids.push(input.envelope_id.clone());
            }
            retention = retention.most_restrictive(input.retention);
        }
        let digest_sha256 = hex_digest(&bytes);
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!(
                "model-output-{}",
                short_digest(
                    format!(
                        "{}:{}:{}",
                        self.export_authorization_id, digest_sha256, self.now_unix_ms
                    )
                    .as_bytes()
                )
            ),
            content: ContentRef::EphemeralObservation {
                observation_id: format!(
                    "model-response-{}",
                    short_digest(digest_sha256.as_bytes())
                ),
                size_bytes: bytes.len() as u64,
                expires_at_unix_ms: retention
                    .expires_at_unix_ms
                    .unwrap_or_else(|| self.now_unix_ms.saturating_add(MODEL_OUTPUT_TTL_MS)),
            },
            provenance: DataProvenance {
                source_provider_id: "external-model".into(),
                source_tool_name: "model-response".into(),
                source_object_id: Some(short_digest(format!("{:?}", self.destination).as_bytes())),
                source_envelope_ids: source_ids,
            },
            digest_sha256,
            sensitivity,
            allowed_destinations: allowed.into_iter().collect(),
            retention,
        };
        envelope
            .validate()
            .map_err(|error| ModelEgressError::InvalidDerivedEnvelope(error.to_string()))?;
        Ok(envelope)
    }

    fn public_system_envelope(
        &self,
        message_id: &str,
        bytes: &[u8],
    ) -> Result<DataEnvelope, ModelEgressError> {
        if bytes.is_empty() {
            return Err(ModelEgressError::EmptySystemPrompt);
        }
        let digest_sha256 = hex_digest(bytes);
        let envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("system-prompt-{}", short_digest(message_id.as_bytes())),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("system-prompt-content-{}", short_digest(bytes)),
                sha256: digest_sha256.clone(),
                size_bytes: bytes.len() as u64,
                media_type: "text/plain;charset=utf-8".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "device-assistant-runtime".into(),
                source_tool_name: "system-prompt-projector".into(),
                source_object_id: Some(message_id.to_string()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256,
            sensitivity: Sensitivity::Public,
            allowed_destinations: vec![self.destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        };
        envelope
            .validate()
            .map_err(|error| ModelEgressError::InvalidDerivedEnvelope(error.to_string()))?;
        Ok(envelope)
    }
}

fn envelope_is_expired(envelope: &DataEnvelope, now_unix_ms: u64) -> bool {
    envelope
        .retention
        .expires_at_unix_ms
        .is_some_and(|expiry| expiry <= now_unix_ms)
        || matches!(
            &envelope.content,
            ContentRef::EphemeralObservation {
                expires_at_unix_ms,
                ..
            } if *expires_at_unix_ms <= now_unix_ms
        )
}

#[derive(Serialize)]
struct AssistantPayload<'a, T> {
    text: &'a str,
    tool_calls: &'a [T],
}

pub fn message_content_bytes(message: &ChatMessage) -> Result<Vec<u8>, ModelEgressError> {
    if message.image_data_url.is_some() && !message.tool_calls.is_empty() {
        return Err(ModelEgressError::ImageNotSupported);
    }
    if message.tool_calls.is_empty() {
        message_payload_bytes(&message.text, message.image_data_url.as_deref())
    } else {
        serde_json::to_vec(&AssistantPayload::<ToolCallRef> {
            text: &message.text,
            tool_calls: &message.tool_calls,
        })
        .map_err(|error| ModelEgressError::Encode(error.to_string()))
    }
}

/// Return the exact bytes bound by a message's `DataEnvelope`.
///
/// Text-only messages intentionally retain their historical raw-text encoding.
/// An image-bearing message uses a tagged, length-delimited encoding so neither
/// the text/image boundary nor a future payload kind can change the digest's
/// meaning. The data URL is validated before it can influence an authorization.
pub fn message_payload_bytes(
    text: &str,
    image_data_url: Option<&str>,
) -> Result<Vec<u8>, ModelEgressError> {
    let Some(image_data_url) = image_data_url else {
        return Ok(text.as_bytes().to_vec());
    };
    crate::image_input::validate_image_data_url(image_data_url)
        .map_err(|error| ModelEgressError::Encode(error.to_string()))?;

    const IMAGE_PAYLOAD_TAG: &[u8] = b"lrd-model-image-v1\0";
    let text_len = u64::try_from(text.len())
        .map_err(|_| ModelEgressError::Encode("message text is too large".into()))?;
    let image_len = u64::try_from(image_data_url.len())
        .map_err(|_| ModelEgressError::Encode("message image is too large".into()))?;
    let capacity = IMAGE_PAYLOAD_TAG
        .len()
        .checked_add(std::mem::size_of::<u64>())
        .and_then(|value| value.checked_add(text.len()))
        .and_then(|value| value.checked_add(std::mem::size_of::<u64>()))
        .and_then(|value| value.checked_add(image_data_url.len()))
        .ok_or_else(|| ModelEgressError::Encode("message payload is too large".into()))?;
    let mut payload = Vec::with_capacity(capacity);
    payload.extend_from_slice(IMAGE_PAYLOAD_TAG);
    payload.extend_from_slice(&text_len.to_le_bytes());
    payload.extend_from_slice(text.as_bytes());
    payload.extend_from_slice(&image_len.to_le_bytes());
    payload.extend_from_slice(image_data_url.as_bytes());
    Ok(payload)
}

pub fn model_turn_content_bytes(turn: &ModelTurn) -> Result<Vec<u8>, ModelEgressError> {
    if turn.tool_calls.is_empty() {
        Ok(turn.text.as_bytes().to_vec())
    } else {
        serde_json::to_vec(&AssistantPayload::<ToolCall> {
            text: &turn.text,
            tool_calls: &turn.tool_calls,
        })
        .map_err(|error| ModelEgressError::Encode(error.to_string()))
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn short_digest(bytes: &[u8]) -> String {
    hex_digest(bytes)[..32].to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEgressError {
    InvalidPolicy(String),
    MissingEnvelope {
        message_id: String,
        role: ChatRole,
    },
    ExportNotSelected {
        message_id: String,
        source_tool_name: String,
    },
    ImageNotSupported,
    EmptySystemPrompt,
    EmptyInputs,
    EmptyModelOutput,
    DerivedDestinationLost,
    Encode(String),
    InvalidDerivedEnvelope(String),
    Sink(SinkAuthorizationError),
}

impl std::fmt::Display for ModelEgressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPolicy(error) => write!(formatter, "invalid model egress policy: {error}"),
            Self::MissingEnvelope { message_id, role } => write!(
                formatter,
                "message {message_id} with role {role:?} has no DataEnvelope"
            ),
            Self::ExportNotSelected {
                message_id,
                source_tool_name,
            } => write!(
                formatter,
                "message {message_id} from {source_tool_name} is not selected for model export"
            ),
            Self::ImageNotSupported => {
                formatter.write_str("image egress is not enabled for Device Assistant Stage 1")
            }
            Self::EmptySystemPrompt => formatter.write_str("system prompt is empty"),
            Self::EmptyInputs => formatter.write_str("model output has no source envelopes"),
            Self::EmptyModelOutput => formatter.write_str("model output is empty"),
            Self::DerivedDestinationLost => {
                formatter.write_str("derived output lost the current model destination")
            }
            Self::Encode(error) => write!(formatter, "encode model content: {error}"),
            Self::InvalidDerivedEnvelope(error) => {
                write!(formatter, "invalid derived DataEnvelope: {error}")
            }
            Self::Sink(error) => write!(formatter, "model sink denied: {error}"),
        }
    }
}

impl std::error::Error for ModelEgressError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{StopReason, TokenUsage};
    use crate::prompt::ResponseFormatSpec;

    fn destination() -> DestinationIdentity {
        DestinationIdentity::Model {
            connection_id: "oss-ai-gateway:1".into(),
            connection_revision: 7,
            model_id: "fake-model".into(),
            profile_revision: 9,
        }
    }

    fn envelope(
        id: &str,
        tool: &str,
        bytes: &[u8],
        sensitivity: Sensitivity,
        destinations: Vec<DestinationIdentity>,
    ) -> DataEnvelope {
        let digest_sha256 = hex_digest(bytes);
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("blob-{id}"),
                sha256: digest_sha256.clone(),
                size_bytes: bytes.len() as u64,
                media_type: "text/plain".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "test-provider".into(),
                source_tool_name: tool.into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256,
            sensitivity,
            allowed_destinations: destinations,
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        }
    }

    fn message(
        id: &str,
        role: ChatRole,
        text: &str,
        envelope: Option<DataEnvelope>,
    ) -> ChatMessage {
        let mut message = ChatMessage::text(id, role, text);
        message.data_envelope = envelope;
        message
    }

    fn policy(selected: &[&str]) -> ModelEgressPolicy {
        ModelEgressPolicy {
            destination: destination(),
            selected_source_tools: selected.iter().map(|value| (*value).into()).collect(),
            export_authorization_id: "explicit-user-context-selection".into(),
            now_unix_ms: 100,
            byte_cap: MAX_SINK_BYTES,
        }
    }

    #[test]
    fn selected_tool_result_is_exported_exactly_and_output_inherits_labels() {
        let destination = destination();
        let question = b"What is selected?";
        let observation = b"A1 = customer total";
        let request = ModelRequest::text_only(
            vec![
                ChatMessage::text("system", ChatRole::System, "trusted prompt"),
                message(
                    "user",
                    ChatRole::User,
                    std::str::from_utf8(question).unwrap(),
                    Some(envelope(
                        "user-envelope",
                        "send-message",
                        question,
                        Sensitivity::UserContent,
                        vec![destination.clone()],
                    )),
                ),
                message(
                    "tool",
                    ChatRole::Tool,
                    std::str::from_utf8(observation).unwrap(),
                    Some(envelope(
                        "read-envelope",
                        "inspect_office_selection",
                        observation,
                        Sensitivity::Sensitive,
                        Vec::new(),
                    )),
                ),
            ],
            ResponseFormatSpec::None,
        );
        let policy = policy(&["inspect_office_selection"]);
        let authorized = policy.authorize_request(request).unwrap();
        assert_eq!(authorized.audit.envelope_ids.len(), 3);
        assert!(matches!(
            authorized.input_envelopes[0].content,
            ContentRef::ImmutableBlob { .. }
        ));
        assert_eq!(
            authorized.input_envelopes[0].retention.expires_at_unix_ms,
            None
        );
        assert_eq!(
            authorized.request.messages[2]
                .data_envelope
                .as_ref()
                .unwrap()
                .allowed_destinations,
            vec![destination.clone()]
        );

        let turn = ModelTurn {
            text: "The selected cell contains a customer total.".into(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            ..Default::default()
        };
        let derived = policy
            .derive_model_output_envelope(&turn, &authorized.input_envelopes)
            .unwrap();
        assert_eq!(derived.sensitivity, Sensitivity::Sensitive);
        assert_eq!(derived.allowed_destinations, vec![destination]);
        assert!(
            derived
                .provenance
                .source_envelope_ids
                .contains(&"read-envelope".to_string())
                || derived
                    .provenance
                    .source_envelope_ids
                    .iter()
                    .any(|id| id.starts_with("model-export-"))
        );
    }

    #[test]
    fn expired_historical_turn_is_omitted_without_changing_current_turn() {
        let destination = destination();
        let mut expired_answer = envelope(
            "expired-answer-envelope",
            "model-response",
            b"old answer derived from an Office observation",
            Sensitivity::Sensitive,
            vec![destination.clone()],
        );
        expired_answer.retention.expires_at_unix_ms = Some(99);

        let request = ModelRequest::text_only(
            vec![
                ChatMessage::text("system", ChatRole::System, "trusted prompt"),
                message(
                    "prior-user",
                    ChatRole::User,
                    "inspect the old selection",
                    Some(envelope(
                        "prior-user-envelope",
                        "send-message",
                        b"inspect the old selection",
                        Sensitivity::UserContent,
                        vec![destination.clone()],
                    )),
                )
                .with_turn_id("prior-turn"),
                message(
                    "prior-answer",
                    ChatRole::Assistant,
                    "old answer derived from an Office observation",
                    Some(expired_answer),
                )
                .with_turn_id("prior-turn"),
                message(
                    "current-user",
                    ChatRole::User,
                    "inspect the current selection",
                    Some(envelope(
                        "current-user-envelope",
                        "send-message",
                        b"inspect the current selection",
                        Sensitivity::UserContent,
                        vec![destination],
                    )),
                )
                .with_turn_id("current-turn"),
            ],
            ResponseFormatSpec::None,
        );

        let authorized = policy(&["inspect_office_selection"])
            .authorize_request(request)
            .unwrap();
        let message_ids = authorized
            .request
            .messages
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(message_ids, vec!["system", "current-user"]);
    }

    #[test]
    fn expired_current_turn_still_fails_closed() {
        let destination = destination();
        let mut expired_observation = envelope(
            "expired-current-envelope",
            "inspect_office_selection",
            b"current Office observation",
            Sensitivity::Sensitive,
            vec![destination.clone()],
        );
        expired_observation.retention.expires_at_unix_ms = Some(99);
        let request = ModelRequest::text_only(
            vec![
                ChatMessage::text("system", ChatRole::System, "trusted prompt"),
                message(
                    "current-user",
                    ChatRole::User,
                    "inspect the current selection",
                    Some(envelope(
                        "current-user-envelope",
                        "send-message",
                        b"inspect the current selection",
                        Sensitivity::UserContent,
                        vec![destination],
                    )),
                )
                .with_turn_id("current-turn"),
                message(
                    "current-tool",
                    ChatRole::Tool,
                    "current Office observation",
                    Some(expired_observation),
                )
                .with_turn_id("current-turn"),
            ],
            ResponseFormatSpec::None,
        );

        assert!(matches!(
            policy(&["inspect_office_selection"]).authorize_request(request),
            Err(ModelEgressError::Sink(
                SinkAuthorizationError::ExpiredEnvelope
            ))
        ));
    }

    #[test]
    fn missing_unselected_and_secret_content_fail_before_provider_dial() {
        let missing = ModelRequest::text_only(
            vec![ChatMessage::text("user", ChatRole::User, "bare")],
            ResponseFormatSpec::None,
        );
        assert!(matches!(
            policy(&[]).authorize_request(missing),
            Err(ModelEgressError::MissingEnvelope { .. })
        ));

        let bytes = b"bounded observation";
        let unselected = ModelRequest::text_only(
            vec![message(
                "tool",
                ChatRole::Tool,
                std::str::from_utf8(bytes).unwrap(),
                Some(envelope(
                    "unselected",
                    "inspect_desktop_ui",
                    bytes,
                    Sensitivity::Sensitive,
                    Vec::new(),
                )),
            )],
            ResponseFormatSpec::None,
        );
        assert!(matches!(
            policy(&[]).authorize_request(unselected),
            Err(ModelEgressError::ExportNotSelected { .. })
        ));

        let secret = ModelRequest::text_only(
            vec![message(
                "tool",
                ChatRole::Tool,
                "credential",
                Some(envelope(
                    "secret",
                    "inspect_desktop_ui",
                    b"credential",
                    Sensitivity::Secret,
                    Vec::new(),
                )),
            )],
            ResponseFormatSpec::None,
        );
        assert!(matches!(
            policy(&["inspect_desktop_ui"]).authorize_request(secret),
            Err(ModelEgressError::Sink(
                SinkAuthorizationError::ExportSensitivityExceeded
            ))
        ));
    }

    #[test]
    fn deselected_context_prunes_its_complete_prior_turn_but_not_visible_history() {
        let destination = destination();
        let mut prior_call = message(
            "prior-call",
            ChatRole::Assistant,
            "",
            Some(envelope(
                "prior-call-envelope",
                "model-response",
                b"tool call",
                Sensitivity::UserContent,
                vec![destination.clone()],
            )),
        )
        .with_turn_id("prior-turn");
        prior_call.tool_calls = vec![ToolCallRef {
            id: "prior-call-id".into(),
            name: "inspect_desktop_ui".into(),
            arguments_json: "{}".into(),
        }];
        let mut prior_tool = message(
            "prior-tool",
            ChatRole::Tool,
            "sensitive old UI",
            Some(envelope(
                "prior-tool-envelope",
                "inspect_desktop_ui",
                b"sensitive old UI",
                Sensitivity::Sensitive,
                Vec::new(),
            )),
        );
        prior_tool.tool_call_id = Some("prior-call-id".into());
        let prior_answer = message(
            "prior-answer",
            ChatRole::Assistant,
            "derived sensitive answer",
            Some(envelope(
                "prior-answer-envelope",
                "model-response",
                b"derived sensitive answer",
                Sensitivity::Sensitive,
                vec![destination.clone()],
            )),
        )
        .with_turn_id("prior-turn");
        let current = message(
            "current-user",
            ChatRole::User,
            "do not read device context",
            Some(envelope(
                "current-user-envelope",
                "send-message",
                b"do not read device context",
                Sensitivity::UserContent,
                vec![destination],
            )),
        )
        .with_turn_id("current-turn");
        let visible_history = vec![prior_call, prior_tool, prior_answer, current];
        let request = ModelRequest::text_only(visible_history.clone(), ResponseFormatSpec::None);

        let authorized = policy(&[]).authorize_request(request).unwrap();
        assert_eq!(authorized.request.messages.len(), 1);
        assert_eq!(authorized.request.messages[0].message_id, "current-user");
        assert_eq!(
            visible_history.len(),
            4,
            "projection must not mutate stored history"
        );
        assert!(
            authorized
                .request
                .messages
                .iter()
                .all(|message| !message.text.contains("sensitive"))
        );
    }

    #[test]
    fn internal_run_control_result_never_prunes_the_current_followup_turn() {
        let destination = destination();
        let user = message(
            "current-user",
            ChatRole::User,
            "latest correction",
            Some(envelope(
                "current-user-envelope",
                "send-message",
                b"latest correction",
                Sensitivity::UserContent,
                vec![destination.clone()],
            )),
        )
        .with_turn_id("current-turn");
        let mut call =
            ChatMessage::text("status-call", ChatRole::Assistant, "").with_turn_id("current-turn");
        call.tool_calls = vec![ToolCallRef {
            id: "status-call-id".into(),
            name: "update_task_status".into(),
            arguments_json: "{}".into(),
        }];
        let call_bytes = message_content_bytes(&call).unwrap();
        call.data_envelope = Some(envelope(
            "status-call-envelope",
            "model-response",
            &call_bytes,
            Sensitivity::UserContent,
            vec![destination.clone()],
        ));
        let mut result_envelope = envelope(
            "status-result-envelope",
            "update_task_status",
            b"status updated",
            Sensitivity::UserContent,
            vec![destination],
        );
        result_envelope.provenance.source_provider_id =
            crate::dynamic_run::RUN_CONTROL_PROVIDER_ID.into();
        let mut result = message(
            "status-result",
            ChatRole::Tool,
            "status updated",
            Some(result_envelope),
        );
        result.tool_call_id = Some("status-call-id".into());

        let authorized = policy(&[])
            .authorize_request(ModelRequest::text_only(
                vec![user, call, result],
                ResponseFormatSpec::None,
            ))
            .unwrap();

        assert_eq!(authorized.request.messages.len(), 3);
        assert_eq!(authorized.request.messages[0].message_id, "current-user");
        assert_eq!(authorized.request.messages[2].message_id, "status-result");
    }

    #[test]
    fn image_is_rejected_without_exact_current_screen_export_contract() {
        let mut image = ChatMessage::text("image", ChatRole::User, "screen")
            .with_image("data:image/png;base64,AA==");
        image.data_envelope = Some(envelope(
            "image-envelope",
            "capture-screen",
            b"screen",
            Sensitivity::Sensitive,
            vec![destination()],
        ));
        let request = ModelRequest::text_only(vec![image], ResponseFormatSpec::None);
        assert_eq!(
            policy(&[]).authorize_request(request).unwrap_err(),
            ModelEgressError::ImageNotSupported
        );
    }

    #[test]
    fn explicitly_selected_current_screen_exports_one_sensitive_image() {
        let mut image = ChatMessage::text("screen-result", ChatRole::Tool, "screen metadata")
            .with_image("data:image/png;base64,AA==");
        let bytes = message_content_bytes(&image).unwrap();
        image.data_envelope = Some(envelope(
            "screen-envelope",
            "read_current_screen",
            &bytes,
            Sensitivity::Sensitive,
            Vec::new(),
        ));
        let request = ModelRequest::text_only(vec![image], ResponseFormatSpec::None);

        let authorized = policy(&["read_current_screen"])
            .authorize_request(request)
            .unwrap();
        assert_eq!(authorized.request.messages.len(), 1);
        assert!(authorized.request.messages[0].image_data_url.is_some());
        assert_eq!(authorized.input_envelopes.len(), 1);
        assert_eq!(
            authorized.input_envelopes[0].sensitivity,
            Sensitivity::Sensitive
        );
        assert_eq!(
            authorized.input_envelopes[0].allowed_destinations,
            vec![destination()]
        );
    }

    #[test]
    fn derived_sensitive_draft_keeps_provenance_and_cannot_enter_other_sinks() {
        let destination = destination();
        let source_bytes = b"customer account observation";
        let request = ModelRequest::text_only(
            vec![message(
                "tool",
                ChatRole::Tool,
                std::str::from_utf8(source_bytes).unwrap(),
                Some(envelope(
                    "sensitive-source",
                    "inspect_desktop_ui",
                    source_bytes,
                    Sensitivity::Sensitive,
                    Vec::new(),
                )),
            )],
            ResponseFormatSpec::None,
        );
        let policy = policy(&["inspect_desktop_ui"]);
        let authorized = policy.authorize_request(request).unwrap();
        let turn = ModelTurn {
            text: "Draft a lookup query for the observed account".into(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            ..Default::default()
        };
        let derived = policy
            .derive_model_output_envelope(&turn, &authorized.input_envelopes)
            .unwrap();
        assert_eq!(derived.sensitivity, Sensitivity::Sensitive);
        assert!(
            derived
                .provenance
                .source_envelope_ids
                .iter()
                .any(|id| id == "sensitive-source" || id.starts_with("model-export-"))
        );
        assert_eq!(derived.allowed_destinations, vec![destination]);

        let output = model_turn_content_bytes(&turn).unwrap();
        for sink in [
            DestinationIdentity::WebResearch {
                connector_id: "web".into(),
            },
            DestinationIdentity::EmailAccount {
                account_id: "mail".into(),
            },
            DestinationIdentity::ChatAccount {
                account_id: "chat".into(),
            },
        ] {
            assert!(matches!(
                DefaultSinkAuthorizer.authorize(
                    &sink,
                    &[SinkInput {
                        envelope: &derived,
                        bytes: &output,
                    }],
                    policy.now_unix_ms,
                    MAX_SINK_BYTES,
                ),
                Err(SinkAuthorizationError::DestinationNotAllowed)
            ));
        }
    }
}
