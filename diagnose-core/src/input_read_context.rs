//! Immutable owner-selected read context, shared by both central backends.

pub mod live_read;
pub mod object_read;

use crate::{
    context_attachment::{
        AttachmentState, ContextAttachment, ContextAttachmentKind, validate_attachment_set,
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadContextSelection {
    pub tool_names: Vec<String>,
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub object_attachments: Vec<ContextAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_targets: Vec<live_read::LiveReadTarget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_issues: Vec<crate::external_tools::IssueAccess>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_issue_sources: Vec<crate::issue_authority::IssueSourceRef>,
}

impl ReadContextSelection {
    /// Account-scoped inputs may include device resources without granting a
    /// device capability to the conversation itself.
    pub fn is_selectable_input(&self) -> bool {
        self.tool_names.iter().all(|name| {
            crate::web_research::is_web_tool(name) || self.is_selected_object_tool(name)
        }) && self.live_targets.iter().all(|target| {
            self.object_attachments.iter().any(|attachment| {
                selectable_live_metadata(attachment)
                    && attachment.device_id == target.device_id
                    && attachment.envelope.provenance.source_tool_name == target.tool_name
            })
        })
    }

    pub fn is_selected_object_tool(&self, name: &str) -> bool {
        (object_read::implicit_object_tool(name, &self.object_attachments)
            || self
                .live_targets
                .iter()
                .any(|target| target.tool_name == name))
            && crate::ai_assistant::ai_assistant_provider_registry_ref()
                .capability_for_tool(name)
                .is_some_and(crate::request_devices::selected_read_descriptor)
    }

    pub fn include_selectable_object_tools(&mut self) {
        if self.object_attachments.is_empty() {
            return;
        }
        for descriptor in crate::ai_assistant::ai_assistant_provider_registry_ref()
            .providers()
            .flat_map(|provider| &provider.capabilities)
            .filter(|descriptor| crate::request_devices::selected_read_descriptor(descriptor))
        {
            let name = descriptor.wire.tool_name.as_str();
            if object_read::implicit_object_tool(name, &self.object_attachments)
                && let Err(index) = self
                    .tool_names
                    .binary_search_by(|tool| tool.as_str().cmp(name))
            {
                self.tool_names.insert(index, name.into());
            }
        }
    }

    pub(crate) fn has_device_objects_for_tool(&self, name: &str, device_id: &str) -> bool {
        self.live_targets
            .iter()
            .any(|target| target.device_id == device_id && target.tool_name == name)
            || self.object_attachments.iter().any(|object| {
                object.device_id == device_id
                    && object_read::supports_implicit_object(name, object.kind)
            })
    }

    pub fn validate_object_devices(
        &self,
        targets: &[crate::dynamic_run::AssistantInputTarget],
    ) -> Result<(), AgentError> {
        if self.object_attachments.iter().any(|attachment| {
            !targets
                .iter()
                .any(|target| target.device_id == attachment.device_id)
        }) || self.live_targets.iter().any(|live| {
            !targets
                .iter()
                .any(|target| target.device_id == live.device_id)
        }) {
            return Err(invalid(
                "selected resource is outside the original device targets",
            ));
        }
        Ok(())
    }

    /// Capture the final model-compatible tools actually exposed to this turn.
    pub fn capture(tools: &[RegisteredTool], scope: &AgentScope) -> Result<Self, AgentError> {
        let selection = Self {
            tool_names: tools
                .iter()
                .filter(|tool| {
                    tool.effect == ToolEffect::ReadOnly
                        && tool.has_device_capability(&scope.granted)
                })
                .map(|tool| tool.name().to_string())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
            expires_at: scope.expires_at.clone(),
            object_attachments: Vec::new(),
            live_targets: Vec::new(),
            external_issues: Vec::new(),
            external_issue_sources: Vec::new(),
        };
        selection.validate()?;
        Ok(selection)
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        crate::external_tools::validate_accesses(&self.external_issues)?;
        let mut refs = std::collections::BTreeSet::new();
        if self.external_issue_sources.len() > self.external_issues.len()
            || self.external_issue_sources.iter().any(|source| {
                !crate::native_ids::is(
                    crate::native_ids::Kind::IssueAdmission,
                    &source.authorization_id,
                ) || source.subscription_revision == 0
                    || source.subscription_revision > i64::MAX as u64
                    || !refs.insert(source.tool_ref.as_str())
                    || !self
                        .external_issues
                        .iter()
                        .any(|issue| issue.tool_ref() == source.tool_ref)
            })
        {
            return Err(invalid("invalid original issue source references"));
        }
        validate_objects(&self.object_attachments)?;
        live_read::validate_targets(self)?;
        let providers = crate::ai_assistant::ai_assistant_provider_registry_ref();
        if self.tool_names.len() > MAX_READ_TOOLS
            || self.tool_names.windows(2).any(|pair| pair[0] >= pair[1])
            || self.tool_names.iter().any(|name| {
                providers
                    .capability_for_tool(name)
                    .is_none_or(|capability| capability.wire.effect.is_side_effecting())
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

fn selectable_live_metadata(object: &ContextAttachment) -> bool {
    let provenance = &object.envelope.provenance;
    object.kind == ContextAttachmentKind::InteractiveSession
        && live_read::target_kind(&provenance.source_tool_name).is_some()
        && crate::ai_assistant::ai_assistant_provider_registry_ref()
            .capability_for_tool(&provenance.source_tool_name)
            .is_some_and(|descriptor| {
                descriptor.wire.capability_id == object.object_ref.source_capability_id
                    && provenance.source_provider_id == object.object_ref.source_provider_id
                    && provenance.source_object_id.as_deref()
                        == Some(&object.object_ref.opaque_token)
            })
        && matches!(
            object.envelope.allowed_destinations.as_slice(),
            [DestinationIdentity::Model { .. }]
        )
}

/// Resolve explicit object selections, including live read metadata.
pub fn selected_object_attachment<'a>(
    objects: &'a [ContextAttachment],
    id: &str,
    now: u64,
) -> Result<&'a ContextAttachment, AgentError> {
    let object = objects.iter().find(|object| object.attachment_id == id)
        .ok_or_else(|| invalid(&format!("Selected context attachment {id} was removed or is unavailable. Refresh the conversation and select the intended object again. The message was not accepted.")))?;
    if object.kind == ContextAttachmentKind::InteractiveSession && !selectable_live_metadata(object)
    {
        return Err(invalid(
            "Desktop capability selections must use selected_capability_ids, not selected_attachment_ids. The message was not accepted.",
        ));
    }
    if !object.is_active_at(now) {
        return Err(invalid(&format!(
            "Selected context attachment {id} ({}) expired, changed or was withdrawn. Re-select this object or explicitly remove it before sending again. The message was not accepted.",
            object.display_summary
        )));
    }
    Ok(object)
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
        if selectable_live_metadata(object) && matches!(object.state, AttachmentState::Active) {
            continue;
        }
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
                | (ContextAttachmentKind::WindowSelection, ObjectKind::Window)
        );
        let expiry = if reference.object_kind.is_lifecycle_bound() {
            Some(u64::MAX)
        } else {
            DateTime::parse_from_rfc3339(&reference.expires_at)
                .ok()
                .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
        };
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
    validate_current_objects_on(session, objects, destination, now, None)
}

pub fn validate_current_device_objects(
    session: &PersistedAgentSession,
    objects: &[ContextAttachment],
    destination: &DestinationIdentity,
    now: u64,
    device_id: &str,
) -> Result<(), AgentError> {
    validate_current_objects_on(session, objects, destination, now, Some(device_id))
}

fn validate_current_objects_on(
    session: &PersistedAgentSession,
    objects: &[ContextAttachment],
    destination: &DestinationIdentity,
    now: u64,
    device_id: Option<&str>,
) -> Result<(), AgentError> {
    validate_objects(objects)?;
    for object in objects
        .iter()
        .filter(|object| device_id.is_none_or(|id| id == object.device_id))
    {
        session
            .validate_context_subject(object)
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
