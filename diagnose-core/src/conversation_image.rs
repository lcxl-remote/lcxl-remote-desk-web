//! Durable screenshot attachment contract. Pixels are stored outside session JSON.
use crate::{chat::ChatMessage, model_egress::ModelEgressPolicy, session::PersistedAgentSession};
use base64::Engine as _;
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    data_lineage::ContentRef,
    visual_evidence::{VisualEvidenceFrame, VisualEvidenceStatus},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const READ_IMAGE_TOOL: &str = "read_conversation_image";
pub const MAX_SESSION_IMAGE_BYTES: i64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub frame: VisualEvidenceFrame,
    /// Exact source text and model authorization; image bytes live separately.
    pub message: ChatMessage,
}

pub fn error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

impl ImageAttachment {
    pub fn prepare(
        session: &PersistedAgentSession,
        frame: &VisualEvidenceFrame,
        policy: Option<&ModelEgressPolicy>,
    ) -> Result<(Self, Vec<u8>), AgentError> {
        let message = session
            .conversation
            .iter()
            .rev()
            .find(|m| m.tool_call_id.as_deref() == Some(frame.tool_call_id.as_str()))
            .ok_or_else(|| error("Screenshot result is missing"))?
            .clone();
        let mut message = if let Some(policy) = policy {
            let request = crate::seam::ModelRequest::text_only(
                vec![message],
                crate::prompt::ResponseFormatSpec::None,
            );
            policy
                .authorize_request_with_history(request, &session.conversation)
                .map_err(|e| e.agent_error())?
                .request
                .messages
                .into_iter()
                .next()
                .ok_or_else(|| error("Screenshot is not authorized for the current model"))?
        } else {
            message
        };
        let url = message
            .image_data_url
            .take()
            .ok_or_else(|| error("Screenshot pixels are missing"))?;
        let info =
            crate::image_input::validate_image_data_url(&url).map_err(|e| error(e.to_string()))?;
        let pixels = base64::engine::general_purpose::STANDARD
            .decode(url.split_once(',').unwrap().1)
            .map_err(|_| error("Invalid screenshot encoding"))?;
        let digest = format!("{:x}", Sha256::digest(&pixels));
        let mut frame = frame.clone();
        frame.preview_data_url = None;
        frame.status = VisualEvidenceStatus::Available;
        frame.expires_at_unix_ms = None;
        frame.digest_sha256 = Some(digest.clone());
        frame.content = Some(ContentRef::Artifact {
            artifact_id: frame.evidence_id.clone(),
            sha256: digest,
            size_bytes: pixels.len() as u64,
            media_type: info.media_type,
        });
        Ok((Self { frame, message }, pixels))
    }

    pub fn restore(&self, pixels: &[u8]) -> Result<ChatMessage, AgentError> {
        let Some(ContentRef::Artifact {
            artifact_id,
            sha256,
            size_bytes,
            media_type,
        }) = &self.frame.content
        else {
            return Err(error("Screenshot attachment metadata is invalid"));
        };
        if self.frame.media_type.as_deref() != Some(media_type.as_str())
            || self.frame.size_bytes != *size_bytes
            || self.frame.digest_sha256.as_deref() != Some(sha256.as_str())
            || self.message.tool_call_id.as_deref() != Some(self.frame.tool_call_id.as_str())
            || artifact_id != &self.frame.evidence_id
            || *size_bytes != pixels.len() as u64
            || *sha256 != format!("{:x}", Sha256::digest(pixels))
        {
            return Err(error("Screenshot attachment integrity check failed"));
        }
        let mut message = self.message.clone();
        message.image_data_url = Some(format!(
            "data:{media_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(pixels)
        ));
        let bytes =
            crate::model_egress::message_content_bytes(&message).map_err(|e| e.agent_error())?;
        let envelope = message
            .data_envelope
            .as_ref()
            .ok_or_else(|| error("Screenshot authorization is missing"))?;
        envelope
            .validate()
            .map_err(|_| error("Screenshot authorization is invalid"))?;
        let declared_size = match &envelope.content {
            ContentRef::ImmutableBlob { size_bytes, .. }
            | ContentRef::Artifact { size_bytes, .. }
            | ContentRef::EphemeralObservation { size_bytes, .. } => *size_bytes,
        };
        if declared_size != bytes.len() as u64
            || envelope.digest_sha256 != format!("{:x}", Sha256::digest(&bytes))
        {
            return Err(error("Screenshot authorization does not match its content"));
        }
        Ok(message)
    }
}

pub fn authorize_read(
    message: &ChatMessage,
    policy: Option<&ModelEgressPolicy>,
) -> Result<(), AgentError> {
    let policy = policy.ok_or_else(|| error("Current model authorization is unavailable"))?;
    let envelope = message
        .data_envelope
        .as_ref()
        .ok_or_else(|| error("Screenshot authorization is missing"))?;
    if !envelope.allowed_destinations.contains(&policy.destination) {
        return Err(error(
            "Stored screenshot is not authorized for the current model. Ask the owner for fresh authorized evidence.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PersistedAgentSession, ImageAttachment, Vec<u8>) {
        use crate::{
            chat::ChatMessage, image_input::validate_image_data_url, session::AgentSessionSurface,
        };
        use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::*};
        use sha2::{Digest, Sha256};
        let scope = AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session =
            PersistedAgentSession::new("conversation", "owner", "device", 1, scope.clone(), "now");
        session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session.begin_focus_epoch(1, vec![]).unwrap();
        session
            .begin_turn("turn", Some("request".into()), None, 1, scope, "now")
            .unwrap();
        let url = "data:image/png;base64,AQID";
        let mut message =
            ChatMessage::tool_result("result", "call", "screen captured").with_image(url);
        let payload = crate::model_egress::message_content_bytes(&message).unwrap();
        message.data_envelope = Some(DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "envelope".into(),
            content: ContentRef::EphemeralObservation {
                observation_id: "observation".into(),
                size_bytes: payload.len() as u64,
                expires_at_unix_ms: 600_000,
            },
            provenance: DataProvenance {
                source_provider_id: "screen.current".into(),
                source_tool_name: "read_current_screen".into(),
                source_object_id: None,
                source_envelope_ids: vec![],
            },
            digest_sha256: format!("{:x}", Sha256::digest(&payload)),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![DestinationIdentity::Model {
                connection_id: "model".into(),
                connection_revision: 1,
                model_id: "visual".into(),
                profile_revision: 1,
            }],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(600_000),
                delete_with_run: true,
            },
        });
        session.conversation.push(message);
        let frame = crate::visual_evidence::record_live_observation(
            &mut session,
            "call",
            url,
            &validate_image_data_url(url).unwrap(),
        )
        .unwrap();
        let (attachment, pixels) = ImageAttachment::prepare(&session, &frame, None).unwrap();
        (session, attachment, pixels)
    }

    #[test]
    fn pixels_survive_history_projection_without_becoming_fresh_authority() {
        let (mut session, attachment, pixels) = fixture();
        session.visual_evidence = vec![attachment.frame.clone()];
        let original = attachment.restore(&pixels).unwrap();
        let encoded = session.encode_json_for_storage().unwrap();
        assert!(!encoded.contains("base64,AQID"));
        assert!(encoded.contains(&attachment.frame.evidence_id));
        let recovered = PersistedAgentSession::decode_json(&encoded).unwrap();
        let frames =
            crate::visual_evidence::durable_projection(&recovered.visual_evidence, 9_999_999);
        assert_eq!(frames[0].status, VisualEvidenceStatus::Available);
        assert!(frames[0].preview_data_url.is_none());
        assert_eq!(attachment.restore(&pixels).unwrap(), original);
        assert_eq!(
            original.data_envelope.unwrap().retention.expires_at_unix_ms,
            Some(600_000)
        );
    }
    #[test]
    fn tampered_pixels_and_source_metadata_are_rejected() {
        let (_, mut attachment, pixels) = fixture();
        assert!(attachment.restore(&[3, 2, 1]).is_err());
        attachment.message.text.push_str("forged");
        assert!(attachment.restore(&pixels).is_err());
    }
    #[test]
    fn stored_images_require_the_original_model_destination() {
        let (_, attachment, pixels) = fixture();
        let message = attachment.restore(&pixels).unwrap();
        let mut policy = ModelEgressPolicy {
            destination: message.data_envelope.as_ref().unwrap().allowed_destinations[0].clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "current".into(),
            now_unix_ms: 9_999_999,
            byte_cap: 1024 * 1024,
            permission_resume: false,
        };
        authorize_read(&message, Some(&policy)).unwrap();
        policy
            .authorize_request(crate::seam::ModelRequest::text_only(
                vec![message.clone()],
                crate::prompt::ResponseFormatSpec::None,
            ))
            .unwrap();
        policy.destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "another".into(),
            connection_revision: 1,
            model_id: "visual".into(),
            profile_revision: 1,
        };
        assert!(authorize_read(&message, Some(&policy)).is_err());
        assert!(authorize_read(&message, None).is_err());
    }
}
