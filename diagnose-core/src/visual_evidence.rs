//! Bounded visual evidence projection for owner-visible Computer Use activity.

use std::collections::BTreeSet;

use desk_agent_protocol::{
    data_lineage::ContentRef,
    visual_evidence::{
        VISUAL_EVIDENCE_SCHEMA_VERSION, VisualEvidenceFrame, VisualEvidencePhase,
        VisualEvidenceStatus,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{chat::ChatRole, image_input::ImageDataUrlInfo, session::PersistedAgentSession};

pub const MAX_VISUAL_EVIDENCE_ITEMS: usize = 64;
pub const MAX_VISUAL_EVIDENCE_SNAPSHOT_ITEMS: usize = 32;

/// A screenshot may inform a target proposal, but it is never target authority.
/// This durable fence withdraws Computer Use targeting tools until a later model
/// step obtains a fresh semantic UI tree or a second screen observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualVerificationFence {
    pub focus_input_revision: u64,
    pub source_tool_call_id: String,
    pub source_assistant_message_id: String,
}

pub fn blocks_targeting(session: &PersistedAgentSession, tool_name: &str) -> bool {
    session.pending_visual_verification.is_some()
        && matches!(
            tool_name,
            "preview_computer_action"
                | "execute_confirmed_ui_action"
                | "execute_confirmed_raw_input"
        )
}

/// Advance the visual verification fence after an accepted, successful read.
/// Calls in the same assistant batch cannot verify one another: the model has
/// not yet observed either result. A later assistant step may verify the first
/// screenshot with bounded semantic UI or with one fresh screen frame.
pub fn note_successful_observation(
    session: &mut PersistedAgentSession,
    call_id: &str,
    tool_name: &str,
) -> Result<(), &'static str> {
    if session.surface != crate::session::AgentSessionSurface::DeviceAssistant
        || !matches!(tool_name, "read_current_screen" | "inspect_desktop_ui")
    {
        return Ok(());
    }
    let assistant_message_id = session
        .conversation
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.tool_calls.iter().any(|call| call.id == call_id)
        })
        .map(|message| message.message_id.clone())
        .ok_or("visual observation has no assistant tool-call message")?;

    if let Some(pending) = &session.pending_visual_verification {
        if pending.focus_input_revision != session.input_revision {
            return Err("visual verification fence is bound to another input revision");
        }
        if pending.source_assistant_message_id != assistant_message_id {
            session.pending_visual_verification = None;
        }
        return Ok(());
    }
    if tool_name == "read_current_screen" {
        session.pending_visual_verification = Some(VisualVerificationFence {
            focus_input_revision: session.input_revision,
            source_tool_call_id: call_id.to_string(),
            source_assistant_message_id: assistant_message_id,
        });
    }
    Ok(())
}

pub fn record_live_observation(
    session: &mut PersistedAgentSession,
    call_id: &str,
    image_data_url: &str,
    image: &ImageDataUrlInfo,
) -> Result<VisualEvidenceFrame, &'static str> {
    let message = session
        .conversation
        .iter()
        .rev()
        .find(|message| message.tool_call_id.as_deref() == Some(call_id))
        .ok_or("visual evidence has no tool result")?;
    let envelope = message
        .data_envelope
        .as_ref()
        .ok_or("visual evidence has no data envelope")?;
    envelope
        .validate()
        .map_err(|_| "visual evidence has an invalid data envelope")?;
    let (frame_id, expires_at_unix_ms, captured_at_unix_ms) = match &envelope.content {
        ContentRef::EphemeralObservation {
            observation_id,
            expires_at_unix_ms,
            ..
        } => (
            observation_id.clone(),
            Some(*expires_at_unix_ms),
            expires_at_unix_ms.saturating_sub(5 * 60 * 1000),
        ),
        ContentRef::ImmutableBlob { blob_id, .. } => (blob_id.clone(), None, 0),
        ContentRef::Artifact { artifact_id, .. } => (artifact_id.clone(), None, 0),
    };
    let turn_id = session
        .current_turn_id
        .clone()
        .ok_or("visual evidence has no current turn")?;
    let evidence_id = format!(
        "visual-{:x}",
        Sha256::digest(
            format!(
                "{}:{}:{turn_id}:{call_id}:{frame_id}",
                session.conversation_id, session.input_revision
            )
            .as_bytes()
        )
    );
    let window_summary = session
        .context_attachments
        .iter()
        .find(|attachment| {
            attachment.kind == crate::context_attachment::ContextAttachmentKind::WindowSelection
                && matches!(
                    attachment.state,
                    crate::context_attachment::AttachmentState::Active
                )
        })
        .map(|attachment| attachment.display_summary.clone());
    let frame = VisualEvidenceFrame {
        schema_version: VISUAL_EVIDENCE_SCHEMA_VERSION,
        evidence_id,
        conversation_id: session.conversation_id.clone(),
        focus_input_revision: session.input_revision,
        turn_id,
        tool_call_id: call_id.to_string(),
        frame_id,
        phase: VisualEvidencePhase::Observation,
        status: VisualEvidenceStatus::Available,
        captured_at_unix_ms,
        expires_at_unix_ms,
        device_id: session.device_id.clone(),
        display_summary: None,
        application_summary: window_summary,
        content: Some(envelope.content.clone()),
        digest_sha256: Some(envelope.digest_sha256.clone()),
        size_bytes: image.decoded_bytes as u64,
        media_type: Some(image.media_type.clone()),
        preview_data_url: Some(image_data_url.to_string()),
    };
    if let Some(existing) = session
        .visual_evidence
        .iter_mut()
        .find(|existing| existing.evidence_id == frame.evidence_id)
    {
        *existing = frame.clone();
    } else {
        session.visual_evidence.push(frame.clone());
        if session.visual_evidence.len() > MAX_VISUAL_EVIDENCE_ITEMS {
            let excess = session.visual_evidence.len() - MAX_VISUAL_EVIDENCE_ITEMS;
            session.visual_evidence.drain(..excess);
        }
    }
    Ok(frame)
}

pub fn durable_projection(
    frames: &[VisualEvidenceFrame],
    now_unix_ms: u64,
) -> Vec<VisualEvidenceFrame> {
    let start = frames
        .len()
        .saturating_sub(MAX_VISUAL_EVIDENCE_SNAPSHOT_ITEMS);
    frames[start..]
        .iter()
        .cloned()
        .map(|mut frame| {
            frame.preview_data_url = None;
            frame.status = if frame
                .expires_at_unix_ms
                .is_some_and(|expires| expires <= now_unix_ms)
            {
                VisualEvidenceStatus::Expired
            } else {
                VisualEvidenceStatus::NotRetained
            };
            frame
        })
        .collect()
}

pub fn strip_previews(frames: &mut [VisualEvidenceFrame]) {
    for frame in frames {
        frame.preview_data_url = None;
        if frame.status == VisualEvidenceStatus::Available {
            frame.status = VisualEvidenceStatus::NotRetained;
        }
    }
}

pub fn validate_set(frames: &[VisualEvidenceFrame]) -> Result<(), &'static str> {
    if frames.len() > MAX_VISUAL_EVIDENCE_ITEMS {
        return Err("too many visual evidence records");
    }
    let mut ids = BTreeSet::new();
    for frame in frames {
        if frame.schema_version != VISUAL_EVIDENCE_SCHEMA_VERSION
            || frame.evidence_id.is_empty()
            || frame.evidence_id.len() > 256
            || frame.conversation_id.is_empty()
            || frame.turn_id.is_empty()
            || frame.tool_call_id.is_empty()
            || frame.frame_id.is_empty()
            || frame.device_id.is_empty()
            || frame.size_bytes == 0
            || frame
                .digest_sha256
                .as_ref()
                .is_none_or(|digest| digest.len() != 64)
            || frame.content.is_none()
            || !ids.insert(frame.evidence_id.as_str())
        {
            return Err("invalid visual evidence record");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{ChatMessage, ToolCallRef},
        image_input::validate_image_data_url,
        session::{AgentSessionSurface, PersistedAgentSession},
    };
    use desk_agent_protocol::{
        AgentScope, ExecutionMode,
        data_lineage::{
            DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
            Sensitivity,
        },
    };

    fn session_with_observation() -> PersistedAgentSession {
        let scope = AgentScope {
            granted: Vec::new(),
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session = PersistedAgentSession::new(
            "conversation-1",
            "actor-1",
            "device-1",
            1,
            scope.clone(),
            "now",
        );
        session.adopt_client_metadata(Some("client-1"), AgentSessionSurface::DeviceAssistant);
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session.begin_focus_epoch(1, Vec::new()).unwrap();
        session
            .begin_turn("turn-1", Some("request-1".into()), None, 1, scope, "now")
            .unwrap();
        let mut result = ChatMessage::tool_result("result-1", "call-1", "screen captured");
        result.data_envelope = Some(DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "envelope-1".into(),
            content: ContentRef::EphemeralObservation {
                observation_id: "frame-1".into(),
                size_bytes: 3,
                expires_at_unix_ms: 600_000,
            },
            provenance: DataProvenance {
                source_provider_id: "computer_use".into(),
                source_tool_name: "read_current_screen".into(),
                source_object_id: Some("display-1".into()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: "a".repeat(64),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(600_000),
                delete_with_run: true,
            },
        });
        session.conversation.push(result);
        session
    }

    #[test]
    fn live_preview_is_deduplicated_bounded_and_never_persisted() {
        let mut session = session_with_observation();
        let data_url = "data:image/png;base64,AQID";
        let info = validate_image_data_url(data_url).unwrap();
        let first = record_live_observation(&mut session, "call-1", data_url, &info).unwrap();
        let second = record_live_observation(&mut session, "call-1", data_url, &info).unwrap();
        assert_eq!(first.evidence_id, second.evidence_id);
        assert_eq!(session.visual_evidence.len(), 1);
        assert_eq!(
            session.visual_evidence[0].preview_data_url.as_deref(),
            Some(data_url)
        );

        let stored = session.encode_json_for_storage().unwrap();
        assert!(!stored.contains(data_url));
        let recovered = PersistedAgentSession::decode_json(&stored).unwrap();
        assert_eq!(recovered.visual_evidence.len(), 1);
        assert!(recovered.visual_evidence[0].preview_data_url.is_none());
        assert_eq!(
            recovered.visual_evidence[0].status,
            VisualEvidenceStatus::NotRetained
        );
    }

    #[test]
    fn durable_projection_expires_without_pixels_and_keeps_only_the_tail() {
        let mut session = session_with_observation();
        let data_url = "data:image/png;base64,AQID";
        let info = validate_image_data_url(data_url).unwrap();
        let template = record_live_observation(&mut session, "call-1", data_url, &info).unwrap();
        let frames = (0..MAX_VISUAL_EVIDENCE_ITEMS)
            .map(|index| {
                let mut frame = template.clone();
                frame.evidence_id = format!("evidence-{index}");
                frame.frame_id = format!("frame-{index}");
                frame
            })
            .collect::<Vec<_>>();
        let projected = durable_projection(&frames, 600_000);
        assert_eq!(projected.len(), MAX_VISUAL_EVIDENCE_SNAPSHOT_ITEMS);
        assert_eq!(projected[0].evidence_id, "evidence-32");
        assert!(projected.iter().all(|frame| {
            frame.preview_data_url.is_none() && frame.status == VisualEvidenceStatus::Expired
        }));
    }

    #[test]
    fn targeting_requires_a_later_assistant_steps_fresh_observation() {
        let mut session = session_with_observation();
        session.conversation.push(ChatMessage::assistant_tool_calls(
            "assistant-1",
            "",
            vec![
                ToolCallRef {
                    id: "screen-1".into(),
                    name: "read_current_screen".into(),
                    arguments_json: "{}".into(),
                },
                ToolCallRef {
                    id: "ui-same-batch".into(),
                    name: "inspect_desktop_ui".into(),
                    arguments_json: "{}".into(),
                },
            ],
        ));
        note_successful_observation(&mut session, "screen-1", "read_current_screen").unwrap();
        assert!(blocks_targeting(&session, "preview_computer_action"));
        assert!(blocks_targeting(&session, "execute_confirmed_raw_input"));

        note_successful_observation(&mut session, "ui-same-batch", "inspect_desktop_ui").unwrap();
        assert!(session.pending_visual_verification.is_some());

        session.conversation.push(ChatMessage::assistant_tool_calls(
            "assistant-2",
            "",
            vec![ToolCallRef {
                id: "screen-2".into(),
                name: "read_current_screen".into(),
                arguments_json: "{}".into(),
            }],
        ));
        note_successful_observation(&mut session, "screen-2", "read_current_screen").unwrap();
        assert!(session.pending_visual_verification.is_none());
        assert!(!blocks_targeting(&session, "execute_confirmed_raw_input"));
    }
}
