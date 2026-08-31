//! Frozen action-result provenance and immutable, generation-bound receipts.
//! These records describe data; they never authorize an effect or an export.

use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{ComputerActionCompleted, ComputerActionOutput},
    data_lineage::{ContentRef, DataEnvelope, DataProvenance, RetentionBoundary, Sensitivity},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    action_turn_fence::AssistantTurnFence,
    chat::{ChatRole, ToolCall},
    provider_registry::ProviderRegistry,
    seam::ToolRunOutput,
    session::{ActionIdentity, PersistedAgentSession},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResultOrigin {
    pub schema_version: u16,
    pub turn_fence: AssistantTurnFence,
    pub tool_call_id: String,
    pub provider_id: String,
    pub tool_name: String,
    pub source_object_id: String,
    pub source_envelope_ids: Vec<String>,
    pub sensitivity: Sensitivity,
    pub retention: RetentionBoundary,
    pub ephemeral: bool,
}

impl ActionResultOrigin {
    /// Freeze the registry and server-resolved input lineage before approval or
    /// dispatch. A later completion never consults the current registry.
    pub fn capture(
        registry: &ProviderRegistry,
        session: &PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<Self, AgentError> {
        let capability = registry
            .capability_for_tool(&call.name)
            .ok_or_else(invalid)?;
        if !capability.wire.effect.is_side_effecting() {
            return Err(invalid());
        }
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(invalid)?;
        let parent = session
            .conversation
            .iter()
            .rev()
            .find(|message| {
                message.role == ChatRole::Assistant
                    && message.tool_calls.iter().any(|candidate| {
                        candidate.id == call.id
                            && candidate.name == call.name
                            && candidate.arguments_json == call.arguments_json
                    })
            })
            .and_then(|message| message.data_envelope.as_ref())
            .ok_or_else(invalid)?;
        let mut source = crate::model_message_labels::internal_tool_result_envelope(
            Some(parent),
            &call.id,
            "action result origin",
            "freeze_action_result_origin",
        )?;
        crate::agent_loop::bind_tool_input_envelopes(session, call, &mut source)?;
        let source = source.ok_or_else(invalid)?;
        let mut retention = source.retention;
        let mut sensitivity = source.sensitivity.max(Sensitivity::Sensitive);
        for id in &source.provenance.source_envelope_ids {
            let envelope = session
                .conversation
                .iter()
                .filter_map(|message| message.data_envelope.as_ref())
                .chain(
                    session
                        .context_attachments
                        .iter()
                        .map(|attachment| &attachment.envelope),
                )
                .find(|envelope| &envelope.envelope_id == id)
                .ok_or_else(invalid)?;
            envelope.validate().map_err(|_| invalid())?;
            sensitivity = sensitivity.max(envelope.sensitivity);
            retention = retention.most_restrictive(envelope.retention);
            if let ContentRef::EphemeralObservation {
                expires_at_unix_ms, ..
            } = envelope.content
            {
                retention = retention.most_restrictive(RetentionBoundary {
                    expires_at_unix_ms: Some(expires_at_unix_ms),
                    delete_with_run: true,
                });
            }
        }
        let origin = Self {
            schema_version: 1,
            turn_fence: AssistantTurnFence::from_session(session)?.ok_or_else(invalid)?,
            tool_call_id: call.id.clone(),
            provider_id: provider.wire.provider_id.clone(),
            tool_name: call.name.clone(),
            source_object_id: format!("{}:{}", session.device_id, call.id),
            source_envelope_ids: source.provenance.source_envelope_ids,
            sensitivity,
            retention,
            ephemeral: call.name.starts_with("browser_"),
        };
        origin.validate()?;
        Ok(origin)
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        self.turn_fence.validate()?;
        self.retention.validate().map_err(|_| invalid())?;
        if self.schema_version != 1
            || self.sensitivity < Sensitivity::Sensitive
            || self.source_envelope_ids.is_empty()
            || [
                &self.tool_call_id,
                &self.provider_id,
                &self.tool_name,
                &self.source_object_id,
            ]
            .iter()
            .any(|value| {
                value.is_empty() || value.len() > 512 || value.chars().any(char::is_control)
            })
            || self
                .source_envelope_ids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid());
        }
        DataProvenance {
            source_provider_id: self.provider_id.clone(),
            source_tool_name: self.tool_name.clone(),
            source_object_id: Some(self.source_object_id.clone()),
            source_envelope_ids: self.source_envelope_ids.clone(),
        }
        .validate()
        .map_err(|_| invalid())
    }

    pub fn receipt(
        &self,
        action: ActionIdentity,
        attempt: u32,
        received_at_unix_ms: u64,
        output: &ToolRunOutput,
    ) -> Result<ActionResultReceipt, AgentError> {
        self.validate()?;
        if action.work_id <= 0
            || attempt == 0
            || received_at_unix_ms == 0
            || [&action.action_request_id, &action.execution_id]
                .iter()
                .any(|id| {
                    id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control)
                })
        {
            return Err(invalid());
        }
        let bytes = crate::model_egress::message_payload_bytes(
            &output.content,
            output.image_data_url.as_deref(),
        )
        .map_err(|_| invalid())?;
        if bytes.is_empty() {
            return Err(invalid());
        }
        let digest = hash(&bytes);
        let key = hash(&serde_json::to_vec(&(&action, attempt, &digest)).map_err(|_| invalid())?);
        let artifact = serde_json::from_str::<ComputerActionCompleted>(&output.content)
            .ok()
            .and_then(|completion| match completion.output {
                Some(ComputerActionOutput::FileArtifact(artifact))
                    if artifact.validate().is_ok() =>
                {
                    Some(artifact)
                }
                _ => None,
            });
        let expiry = received_at_unix_ms
            .checked_add(5 * 60 * 1000)
            .ok_or_else(invalid)?;
        let retention = self.retention.most_restrictive(RetentionBoundary {
            expires_at_unix_ms: self.ephemeral.then_some(expiry),
            delete_with_run: artifact.is_none(),
        });
        let envelope = DataEnvelope {
            schema_version: desk_agent_protocol::data_lineage::DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("action-result-{key}"),
            content: if self.ephemeral {
                ContentRef::EphemeralObservation {
                    observation_id: format!("action-observation-{key}"),
                    size_bytes: bytes.len() as u64,
                    expires_at_unix_ms: retention.expires_at_unix_ms.unwrap_or(expiry),
                }
            } else {
                ContentRef::ImmutableBlob {
                    blob_id: format!("action-content-{key}"),
                    sha256: digest.clone(),
                    size_bytes: bytes.len() as u64,
                    media_type: "text/plain;charset=utf-8".into(),
                }
            },
            provenance: DataProvenance {
                source_provider_id: self.provider_id.clone(),
                source_tool_name: self.tool_name.clone(),
                source_object_id: artifact
                    .map(|artifact| format!("artifact:{}", artifact.file.token))
                    .or_else(|| Some(self.source_object_id.clone())),
                source_envelope_ids: self.source_envelope_ids.clone(),
            },
            digest_sha256: digest,
            sensitivity: self.sensitivity,
            allowed_destinations: vec![],
            retention,
        };
        envelope.validate().map_err(|_| invalid())?;
        Ok(ActionResultReceipt {
            schema_version: 1,
            origin_digest_sha256: hash(&serde_json::to_vec(self).map_err(|_| invalid())?),
            action,
            attempt,
            received_at_unix_ms,
            envelope,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResultReceipt {
    pub schema_version: u16,
    pub origin_digest_sha256: String,
    pub action: ActionIdentity,
    pub attempt: u32,
    pub received_at_unix_ms: u64,
    pub envelope: DataEnvelope,
}

impl ActionResultReceipt {
    pub fn validate_for(
        &self,
        origin: &ActionResultOrigin,
        action: ActionIdentity,
        attempt: u32,
        output: &ToolRunOutput,
    ) -> Result<(), AgentError> {
        if self != &origin.receipt(action, attempt, self.received_at_unix_ms, output)? {
            return Err(invalid());
        }
        Ok(())
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "invalid frozen action result provenance".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ToolCallRef};
    use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::DestinationIdentity};

    fn prepared() -> (PersistedAgentSession, ToolCall) {
        let mut session = PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "now",
        );
        session.surface = crate::session::AgentSessionSurface::DeviceAssistant;
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session
            .begin_turn("turn", None, None, 1, session.scope_snapshot.clone(), "now")
            .unwrap();
        let mut user = crate::model_message_labels::model_bound_user_message(
            "user".into(),
            "PRIVATE REQUIREMENT".into(),
            DestinationIdentity::Model {
                connection_id: "test-model".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        user.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Secret;
        let call = ToolCall {
            id: "call".into(),
            name: "browser_open_page".into(),
            arguments_json: "{}".into(),
        };
        let mut proposal = ChatMessage::assistant_tool_calls(
            "proposal",
            "proposal",
            vec![ToolCallRef {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            }],
        );
        let mut parent = crate::model_message_labels::internal_tool_result_envelope(
            user.data_envelope.as_ref(),
            &call.id,
            &proposal.text,
            "test_model_output",
        )
        .unwrap()
        .unwrap();
        parent.content = ContentRef::EphemeralObservation {
            observation_id: "original-observation".into(),
            size_bytes: proposal.text.len() as u64,
            expires_at_unix_ms: 7000,
        };
        proposal.data_envelope = Some(parent);
        proposal.turn_id = Some("turn".into());
        session.conversation.extend([user, proposal]);
        (session, call)
    }

    fn action() -> ActionIdentity {
        ActionIdentity::new(
            9,
            "request",
            "generation",
            crate::session::WorkKind::CapabilityProvider,
        )
    }

    #[test]
    fn origin_freezes_registered_provider_and_original_input_retention_without_authority() {
        let (mut session, call) = prepared();
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let origin = ActionResultOrigin::capture(&registry, &session, &call).unwrap();
        assert_eq!(origin.tool_name, "browser_open_page");
        assert!(origin.ephemeral);
        assert_eq!(origin.sensitivity, Sensitivity::Secret);
        assert_eq!(origin.retention.expires_at_unix_ms, Some(7000));
        assert!(origin.retention.delete_with_run);
        assert_eq!(origin.source_envelope_ids.len(), 2);
        let encoded = serde_json::to_string(&origin).unwrap();
        assert!(!encoded.contains("PRIVATE REQUIREMENT"));
        session.input_revision += 1;
        assert_eq!(origin.turn_fence.input_revision, 1);
        assert_ne!(
            origin,
            ActionResultOrigin::capture(&registry, &session, &call).unwrap()
        );
        let mut missing = session.clone();
        missing.conversation.last_mut().unwrap().data_envelope = None;
        assert!(ActionResultOrigin::capture(&registry, &missing, &call).is_err());
        let mut changed = call;
        changed.arguments_json = "{\"changed\":true}".into();
        assert!(ActionResultOrigin::capture(&registry, &session, &changed).is_err());
    }

    #[test]
    fn receipt_is_exact_generation_and_bytes_bound_and_replay_cannot_extend_expiry() {
        let (session, call) = prepared();
        let origin = ActionResultOrigin::capture(
            &crate::device_assistant::device_assistant_provider_registry(),
            &session,
            &call,
        )
        .unwrap();
        let output = ToolRunOutput {
            content: "DEVICE RESULT".into(),
            image_data_url: None,
        };
        let receipt = origin.receipt(action(), 1, 1000, &output).unwrap();
        assert!(receipt.envelope.allowed_destinations.is_empty());
        assert_eq!(receipt.envelope.retention.expires_at_unix_ms, Some(7000));
        assert_eq!(receipt.envelope.sensitivity, Sensitivity::Secret);
        let replay: ActionResultReceipt =
            serde_json::from_str(&serde_json::to_string(&receipt).unwrap()).unwrap();
        replay.validate_for(&origin, action(), 1, &output).unwrap();
        assert_eq!(receipt, replay);
        let mut other = action();
        other.execution_id = "other-generation".into();
        assert!(replay.validate_for(&origin, other, 1, &output).is_err());
        assert!(replay.validate_for(&origin, action(), 2, &output).is_err());
        assert!(
            replay
                .validate_for(
                    &origin,
                    action(),
                    1,
                    &ToolRunOutput {
                        content: "CHANGED RESULT".into(),
                        image_data_url: None
                    }
                )
                .is_err()
        );
        for index in 0..4 {
            let mut corrupt = receipt.clone();
            match index {
                0 => corrupt.envelope.retention.expires_at_unix_ms = None,
                1 => corrupt.envelope.provenance.source_envelope_ids.clear(),
                2 => {
                    corrupt.envelope.allowed_destinations = session.conversation[0]
                        .data_envelope
                        .as_ref()
                        .unwrap()
                        .allowed_destinations
                        .clone()
                }
                _ => corrupt.origin_digest_sha256 = "0".repeat(64),
            }
            assert!(corrupt.validate_for(&origin, action(), 1, &output).is_err());
        }
    }

    #[test]
    fn expired_origin_is_not_renewed_and_unknown_contract_fields_fail_closed() {
        let (session, call) = prepared();
        let origin = ActionResultOrigin::capture(
            &crate::device_assistant::device_assistant_provider_registry(),
            &session,
            &call,
        )
        .unwrap();
        let output = ToolRunOutput {
            content: "late result".into(),
            image_data_url: None,
        };
        let receipt = origin.receipt(action(), 1, 9000, &output).unwrap();
        assert_eq!(receipt.envelope.retention.expires_at_unix_ms, Some(7000));
        assert!(origin.receipt(action(), 0, 1000, &output).is_err());
        let mut unknown = serde_json::to_value(&origin).unwrap();
        unknown["export_grant"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ActionResultOrigin>(unknown).is_err());
        let mut unknown = serde_json::to_value(receipt).unwrap();
        unknown["export_grant"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ActionResultReceipt>(unknown).is_err());
    }
}
