//! Immutable owner-selected read context, shared by both central backends.

pub mod live_read;
pub mod object_read;

use crate::{
    context_attachment::{
        AttachmentState, ContextAttachment, ContextAttachmentKind, validate_attachment_set,
        validate_attachment_subject,
    },
    registry::{RegisteredTool, ToolEffect},
    session::PersistedAgentSession,
};
use chrono::DateTime;
use desk_agent_protocol::{
    AgentError, AgentErrorKind, AgentScope,
    computer_use::{ObjectKind, ObjectRef},
    data_lineage::DestinationIdentity,
};
use serde::{Deserialize, Serialize};

const MAX_READ_TOOLS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadContextSelection {
    pub tool_names: Vec<String>,
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub object_attachments: Vec<ContextAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_targets: Vec<live_read::LiveReadTarget>,
}

impl ReadContextSelection {
    /// Capture the final model-compatible tools actually exposed to this turn.
    pub fn capture(tools: &[RegisteredTool], scope: &AgentScope) -> Result<Self, AgentError> {
        let selection = Self {
            tool_names: tools
                .iter()
                .filter(|tool| {
                    tool.effect == ToolEffect::ReadOnly
                        && scope.granted.contains(&tool.required_capability)
                })
                .map(|tool| tool.name().to_string())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
            expires_at: scope.expires_at.clone(),
            object_attachments: Vec::new(),
            live_targets: Vec::new(),
        };
        selection.validate()?;
        Ok(selection)
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        validate_objects(&self.object_attachments)?;
        live_read::validate_targets(self)?;
        let providers = crate::device_assistant::device_assistant_provider_registry();
        let compiled = providers.registered_tools();
        if self.tool_names.len() > MAX_READ_TOOLS
            || self.tool_names.windows(2).any(|pair| pair[0] >= pair[1])
            || self.tool_names.iter().any(|name| {
                !compiled
                    .iter()
                    .any(|tool| tool.name() == name && tool.effect == ToolEffect::ReadOnly)
            })
            || self
                .expires_at
                .as_ref()
                .is_some_and(|value| DateTime::parse_from_rfc3339(value).is_err())
        {
            return Err(invalid("invalid original read context"));
        }
        Ok(())
    }
}

pub fn validate_objects(objects: &[ContextAttachment]) -> Result<(), AgentError> {
    validate_attachment_set(objects).map_err(|_| invalid("invalid original object selection"))?;
    if objects
        .windows(2)
        .any(|pair| pair[0].attachment_id >= pair[1].attachment_id)
    {
        return Err(invalid("object selection is not canonical"));
    }
    for object in objects {
        let reference: ObjectRef = serde_json::from_str(&object.object_ref.opaque_token)
            .map_err(|_| invalid("invalid original object reference"))?;
        let kind_matches = matches!(
            (object.kind, reference.object_kind),
            (ContextAttachmentKind::File, ObjectKind::File)
                | (
                    ContextAttachmentKind::DirectorySelection,
                    ObjectKind::Directory
                )
                | (
                    ContextAttachmentKind::TerminalSessionRef,
                    ObjectKind::TerminalOutput
                )
        );
        let expiry = DateTime::parse_from_rfc3339(&reference.expires_at)
            .ok()
            .and_then(|date| u64::try_from(date.timestamp_millis()).ok());
        if !kind_matches
            || !matches!(object.state, AttachmentState::Active)
            || reference.token.trim().is_empty()
            || reference.snapshot_id.trim().is_empty()
            || expiry != Some(object.expires_at_unix_ms)
            || object.object_ref.object_incarnation
                != format!("{}:{}", reference.snapshot_id, reference.token)
            || !matches!(
                object.envelope.allowed_destinations.as_slice(),
                [DestinationIdentity::Model { .. }]
            )
        {
            return Err(invalid("invalid original object binding"));
        }
    }
    Ok(())
}

pub fn validate_current_objects(
    session: &PersistedAgentSession,
    objects: &[ContextAttachment],
    destination: &DestinationIdentity,
    now: u64,
) -> Result<(), AgentError> {
    validate_objects(objects)?;
    for object in objects {
        validate_attachment_subject(
            object,
            &session.actor_id,
            &session.device_id,
            session.surface,
        )
        .map_err(|_| invalid("object selection subject mismatch"))?;
        if !object.is_active_at(now)
            || object.envelope.allowed_destinations.as_slice() != [destination.clone()]
            || !session
                .context_attachments
                .iter()
                .any(|current| current == object)
        {
            return Err(invalid(
                "original object selection expired, changed or was withdrawn",
            ));
        }
    }
    Ok(())
}

fn invalid(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}
