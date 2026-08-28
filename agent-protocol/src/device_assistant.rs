//! Device Assistant orchestration wire types (control end to central brain).
//!
//! The assistant owns a distinct signaling surface and persisted session from
//! Diagnose. Its event stream deliberately reuses the neutral agent-loop event
//! shape: status, tool activity, final answer, and structured error. Computer
//! Use observation still travels over the read-only remote-tool RPC; no action
//! plan or approval can be carried by this request.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::computer_use::{ObjectKind, ObjectRef};

/// Browser to central brain: one owner-authenticated Device Assistant turn.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DeviceAssistantAsk {
    /// The owner's natural-language request.
    pub question: String,
    /// Stable idempotency identity for this user message. It is distinct from
    /// the transport request id so reconnect/retry cannot duplicate input.
    pub client_message_id: String,
    /// Stable client intent for multi-turn continuity. The server validates and
    /// subject-namespaces it before using it as a persistence key.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// BCP-47 response locale (for example `zh-CN`).
    #[serde(default)]
    pub locale: Option<String>,
    /// Exact user-selected read contexts for this turn. Empty means the model
    /// receives the question but no device/Office read tool. The server matches
    /// these ids against the current Provider Registry and live readiness.
    #[serde(default)]
    pub selected_capability_ids: Vec<String>,
    /// Exact durable object attachments frozen for this turn. The server
    /// resolves these ids from the subject-scoped session and never accepts an
    /// ObjectRef directly from model arguments.
    #[serde(default)]
    pub selected_attachment_ids: Vec<String>,
}

/// Browser to central brain: independently reconcile the durable context
/// selection for one Device Assistant conversation. The outer signaling
/// request id provides transport correlation; `client_request_id` provides
/// persistence idempotency across reconnect/retry.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DeviceAssistantContextUpdate {
    pub conversation_id: String,
    pub client_request_id: String,
    #[serde(default)]
    pub selected_capability_ids: Vec<String>,
}

impl DeviceAssistantContextUpdate {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.conversation_id.trim().is_empty() || self.conversation_id.len() > 256 {
            return Err("invalid Device Assistant conversation id");
        }
        if self.client_request_id.trim().is_empty() || self.client_request_id.len() > 256 {
            return Err("invalid Device Assistant context client request id");
        }
        validate_selected_capability_ids(&self.selected_capability_ids)
    }
}

/// Central brain acknowledgement for a durable context reconciliation. The
/// browser reads attachment metadata from the normal session snapshot, so this
/// acknowledgement cannot expose opaque refs or egress metadata.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DeviceAssistantContextUpdated {
    pub conversation_id: String,
    pub client_request_id: String,
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Explicit mutation of one object-level attachment. The browser may only pass
/// an edge-issued reference obtained from the corresponding first-party
/// surface; native paths, terminal identifiers, and raw content are
/// intentionally absent from this contract.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DeviceAssistantObjectContextUpdate {
    pub conversation_id: String,
    pub client_request_id: String,
    pub operation: DeviceAssistantObjectContextOperation,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceAssistantObjectContextOperation {
    AttachFile {
        object_ref: ObjectRef,
        display_summary: String,
    },
    AttachTerminalOutput {
        object_ref: ObjectRef,
        display_summary: String,
    },
    Detach {
        attachment_id: String,
    },
    RefreshFile {
        stale_attachment_id: String,
        object_ref: ObjectRef,
        display_summary: String,
    },
}

impl DeviceAssistantObjectContextUpdate {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_wire_id(
            &self.conversation_id,
            "invalid Device Assistant conversation id",
        )?;
        validate_wire_id(
            &self.client_request_id,
            "invalid Device Assistant object context client request id",
        )?;
        match &self.operation {
            DeviceAssistantObjectContextOperation::AttachFile {
                object_ref,
                display_summary,
            } => validate_file_selection(object_ref, display_summary),
            DeviceAssistantObjectContextOperation::AttachTerminalOutput {
                object_ref,
                display_summary,
            } => validate_terminal_selection(object_ref, display_summary),
            DeviceAssistantObjectContextOperation::Detach { attachment_id } => {
                validate_wire_id(attachment_id, "invalid Device Assistant attachment id")
            }
            DeviceAssistantObjectContextOperation::RefreshFile {
                stale_attachment_id,
                object_ref,
                display_summary,
            } => {
                validate_wire_id(
                    stale_attachment_id,
                    "invalid stale Device Assistant attachment id",
                )?;
                validate_file_selection(object_ref, display_summary)
            }
        }
    }
}

fn validate_terminal_selection(
    object_ref: &ObjectRef,
    display_summary: &str,
) -> Result<(), &'static str> {
    if object_ref.object_kind != ObjectKind::TerminalOutput {
        return Err("Device Assistant terminal selection requires a terminal output reference");
    }
    validate_wire_id(
        &object_ref.token,
        "invalid Device Assistant terminal reference token",
    )?;
    validate_wire_id(
        &object_ref.snapshot_id,
        "invalid Device Assistant terminal snapshot id",
    )?;
    validate_wire_id(
        &object_ref.expires_at,
        "invalid Device Assistant terminal reference expiry",
    )?;
    if display_summary.trim().is_empty() || display_summary.len() > 512 {
        return Err("invalid Device Assistant terminal display summary");
    }
    Ok(())
}

fn validate_file_selection(
    object_ref: &ObjectRef,
    display_summary: &str,
) -> Result<(), &'static str> {
    if !matches!(
        object_ref.object_kind,
        ObjectKind::File | ObjectKind::Directory
    ) {
        return Err("Device Assistant file selection requires a file or directory reference");
    }
    validate_wire_id(
        &object_ref.token,
        "invalid Device Assistant file reference token",
    )?;
    validate_wire_id(
        &object_ref.snapshot_id,
        "invalid Device Assistant file snapshot id",
    )?;
    validate_wire_id(
        &object_ref.expires_at,
        "invalid Device Assistant file reference expiry",
    )?;
    if display_summary.trim().is_empty() || display_summary.len() > 512 {
        return Err("invalid Device Assistant file display summary");
    }
    Ok(())
}

fn validate_wire_id(value: &str, error: &'static str) -> Result<(), &'static str> {
    if value.trim().is_empty() || value.len() > 512 {
        Err(error)
    } else {
        Ok(())
    }
}

pub type DeviceAssistantObjectContextUpdated = DeviceAssistantContextUpdated;

impl DeviceAssistantAsk {
    pub fn validate(&self) -> Result<(), &'static str> {
        let question = self.question.trim();
        if question.is_empty() || question.len() > 16 * 1024 {
            return Err("Device Assistant question must be 1..=16384 bytes");
        }
        validate_wire_id(
            &self.client_message_id,
            "invalid Device Assistant client message id",
        )?;
        validate_selected_capability_ids(&self.selected_capability_ids)
            .and_then(|_| validate_attachment_ids(&self.selected_attachment_ids))
    }
}

fn validate_selected_capability_ids(
    selected_capability_ids: &[String],
) -> Result<(), &'static str> {
    if selected_capability_ids.len() > 16 {
        return Err("too many selected Device Assistant capabilities");
    }
    let mut unique = std::collections::BTreeSet::new();
    for capability_id in selected_capability_ids {
        if capability_id.trim().is_empty()
            || capability_id.len() > 256
            || !unique.insert(capability_id.as_str())
        {
            return Err("invalid or duplicate selected Device Assistant capability");
        }
    }
    Ok(())
}

fn validate_attachment_ids(attachment_ids: &[String]) -> Result<(), &'static str> {
    if attachment_ids.len() > 32 {
        return Err("too many selected Device Assistant attachments");
    }
    let mut unique = std::collections::BTreeSet::new();
    for attachment_id in attachment_ids {
        if attachment_id.trim().is_empty()
            || attachment_id.len() > 512
            || !unique.insert(attachment_id.as_str())
        {
            return Err("invalid or duplicate selected Device Assistant attachment");
        }
    }
    Ok(())
}

/// Device Assistant streams the shared agent-loop event contract over its own
/// signaling discriminant.
pub type DeviceAssistantEvent = crate::agent_event::AgentEvent;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_round_trips_without_any_mutation_payload() {
        let ask = DeviceAssistantAsk {
            question: "Inspect the active application and suggest a safe change.".into(),
            client_message_id: "message-1".into(),
            conversation_id: Some("assistant-1".into()),
            locale: Some("en-US".into()),
            selected_capability_ids: vec!["desktop.session.inspect".into()],
            selected_attachment_ids: Vec::new(),
        };
        let value = serde_json::to_value(&ask).unwrap();
        assert!(value.get("action").is_none());
        assert!(value.get("approval").is_none());
        let back: DeviceAssistantAsk = serde_json::from_value(value).unwrap();
        assert_eq!(back, ask);
    }

    #[test]
    fn context_update_is_bounded_and_ack_contains_no_attachment_secret() {
        let update = DeviceAssistantContextUpdate {
            conversation_id: "assistant-1".into(),
            client_request_id: "context-change-1".into(),
            selected_capability_ids: vec!["desktop.session.inspect".into()],
        };
        update.validate().unwrap();
        let back: DeviceAssistantContextUpdate =
            serde_json::from_value(serde_json::to_value(&update).unwrap()).unwrap();
        assert_eq!(back, update);

        let ack = serde_json::to_value(DeviceAssistantContextUpdated {
            conversation_id: "assistant-1".into(),
            client_request_id: "context-change-1".into(),
            changed: true,
            error: None,
        })
        .unwrap();
        assert!(ack.get("opaque_token").is_none());
        assert!(ack.get("envelope").is_none());
    }

    #[test]
    fn file_attachment_update_is_typed_and_model_selection_is_bounded() {
        let object_ref = ObjectRef {
            token: "edge-file-token".into(),
            snapshot_id: "worker-1:7".into(),
            object_kind: ObjectKind::File,
            expires_at: "2026-08-25T20:00:00Z".into(),
        };
        let update = DeviceAssistantObjectContextUpdate {
            conversation_id: "assistant-1".into(),
            client_request_id: "file-change-1".into(),
            operation: DeviceAssistantObjectContextOperation::AttachFile {
                object_ref: object_ref.clone(),
                display_summary: "selected.txt".into(),
            },
        };
        update.validate().unwrap();
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(value["operation"]["kind"], "attach_file");
        assert!(value["operation"].get("path").is_none());

        let ask = DeviceAssistantAsk {
            question: "Read the selected file metadata.".into(),
            client_message_id: "message-2".into(),
            selected_attachment_ids: vec!["context-file-1".into()],
            ..Default::default()
        };
        ask.validate().unwrap();
        let mut duplicate = ask.clone();
        duplicate
            .selected_attachment_ids
            .push("context-file-1".into());
        assert!(duplicate.validate().is_err());

        let mut invalid_kind = update;
        invalid_kind.operation = DeviceAssistantObjectContextOperation::AttachFile {
            object_ref: ObjectRef {
                object_kind: ObjectKind::UiElement,
                ..object_ref
            },
            display_summary: "not a file".into(),
        };
        assert!(invalid_kind.validate().is_err());
    }

    #[test]
    fn terminal_attachment_update_accepts_only_an_edge_terminal_reference() {
        let object_ref = ObjectRef {
            token: "edge-terminal-token".into(),
            snapshot_id: "worker-1:9".into(),
            object_kind: ObjectKind::TerminalOutput,
            expires_at: "2026-08-25T20:00:00Z".into(),
        };
        let update = DeviceAssistantObjectContextUpdate {
            conversation_id: "assistant-1".into(),
            client_request_id: "terminal-change-1".into(),
            operation: DeviceAssistantObjectContextOperation::AttachTerminalOutput {
                object_ref: object_ref.clone(),
                display_summary: "recent terminal output".into(),
            },
        };
        update.validate().unwrap();
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(value["operation"]["kind"], "attach_terminal_output");
        assert!(value["operation"].get("terminal_id").is_none());
        assert!(value["operation"].get("content").is_none());

        let mut invalid_kind = update;
        invalid_kind.operation = DeviceAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref: ObjectRef {
                object_kind: ObjectKind::File,
                ..object_ref
            },
            display_summary: "not terminal output".into(),
        };
        assert!(invalid_kind.validate().is_err());
    }
}
