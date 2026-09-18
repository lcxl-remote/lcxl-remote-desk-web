//! AI Assistant orchestration wire types (control end to central brain).
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

/// Device-owned product switch projected to trusted central orchestrators.
///
/// The device is the only authority for this value. Central services may cache
/// the latest observed snapshot for routing and UI, but may never synthesize a
/// newer revision or treat their cache as desired state.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSettings {
    /// Monotonic device-local revision. Revision zero is the initial state.
    pub revision: u64,
    /// The one product-level AI Assistant switch. Defaults fail closed.
    pub enabled: bool,
}

/// Compare-and-set request accepted by the device-local settings endpoint.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSettingsUpdate {
    /// Revision returned by the most recent authoritative device read.
    pub expected_revision: u64,
    /// Exact desired product-switch value.
    pub enabled: bool,
}

/// Browser to central brain: one owner-authenticated AI Assistant turn.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AiAssistantAsk {
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
/// selection for one AI Assistant conversation. The outer signaling
/// request id provides transport correlation; `client_request_id` provides
/// persistence idempotency across reconnect/retry.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AiAssistantContextUpdate {
    pub conversation_id: String,
    pub client_request_id: String,
    #[serde(default)]
    pub selected_capability_ids: Vec<String>,
}

impl AiAssistantContextUpdate {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.conversation_id.trim().is_empty() || self.conversation_id.len() > 256 {
            return Err("invalid AI Assistant conversation id");
        }
        if self.client_request_id.trim().is_empty() || self.client_request_id.len() > 256 {
            return Err("invalid AI Assistant context client request id");
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
pub struct AiAssistantContextUpdated {
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
pub struct AiAssistantObjectContextUpdate {
    pub conversation_id: String,
    pub client_request_id: String,
    pub operation: AiAssistantObjectContextOperation,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AiAssistantObjectContextOperation {
    AttachTerminalOutput {
        object_ref: ObjectRef,
        display_summary: String,
    },
    AttachWindow {
        object_ref: ObjectRef,
        display_summary: String,
    },
    Detach {
        attachment_id: String,
    },
    DecideDirectory {
        directory_request_id: String,
        expected_revision: u64,
        approve: bool,
    },
    SelectDirectory {
        path: String,
        purpose: String,
        expected_revision: u64,
    },
    RevokeDirectory {
        directory_request_id: String,
        expected_revision: u64,
    },
}

impl AiAssistantObjectContextUpdate {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_wire_id(
            &self.conversation_id,
            "invalid AI Assistant conversation id",
        )?;
        validate_wire_id(
            &self.client_request_id,
            "invalid AI Assistant object context client request id",
        )?;
        match &self.operation {
            AiAssistantObjectContextOperation::SelectDirectory { path, purpose, .. } => {
                if path.trim().is_empty()
                    || path.len() > 4096
                    || path.chars().any(char::is_control)
                    || purpose.trim().is_empty()
                    || purpose.len() > 2048
                    || purpose.chars().any(char::is_control)
                {
                    Err("invalid AI Assistant directory selection")
                } else {
                    Ok(())
                }
            }
            AiAssistantObjectContextOperation::DecideDirectory {
                directory_request_id,
                ..
            }
            | AiAssistantObjectContextOperation::RevokeDirectory {
                directory_request_id,
                ..
            } => validate_wire_id(
                directory_request_id,
                "invalid AI Assistant directory request id",
            ),
            AiAssistantObjectContextOperation::AttachTerminalOutput {
                object_ref,
                display_summary,
            } => validate_terminal_selection(object_ref, display_summary),
            AiAssistantObjectContextOperation::AttachWindow {
                object_ref,
                display_summary,
            } => validate_window_selection(object_ref, display_summary),
            AiAssistantObjectContextOperation::Detach { attachment_id } => {
                validate_wire_id(attachment_id, "invalid AI Assistant attachment id")
            }
        }
    }
}

fn validate_window_selection(
    object_ref: &ObjectRef,
    display_summary: &str,
) -> Result<(), &'static str> {
    if object_ref.object_kind != ObjectKind::Window {
        return Err("AI Assistant window selection requires an edge-issued window reference");
    }
    validate_wire_id(
        &object_ref.token,
        "invalid AI Assistant window reference token",
    )?;
    validate_wire_id(
        &object_ref.snapshot_id,
        "invalid AI Assistant window snapshot id",
    )?;
    validate_wire_id(
        &object_ref.expires_at,
        "invalid AI Assistant window reference expiry",
    )?;
    if display_summary.trim().is_empty() || display_summary.len() > 512 {
        return Err("invalid AI Assistant window display summary");
    }
    Ok(())
}

fn validate_terminal_selection(
    object_ref: &ObjectRef,
    display_summary: &str,
) -> Result<(), &'static str> {
    if object_ref.object_kind != ObjectKind::TerminalOutput {
        return Err("AI Assistant terminal selection requires a terminal output reference");
    }
    validate_wire_id(
        &object_ref.token,
        "invalid AI Assistant terminal reference token",
    )?;
    validate_wire_id(
        &object_ref.snapshot_id,
        "invalid AI Assistant terminal snapshot id",
    )?;
    validate_wire_id(
        &object_ref.expires_at,
        "invalid AI Assistant terminal reference expiry",
    )?;
    if display_summary.trim().is_empty() || display_summary.len() > 512 {
        return Err("invalid AI Assistant terminal display summary");
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

pub type AiAssistantObjectContextUpdated = AiAssistantContextUpdated;

impl AiAssistantAsk {
    pub fn validate(&self) -> Result<(), &'static str> {
        let question = self.question.trim();
        if question.is_empty() || question.len() > 16 * 1024 {
            return Err("AI Assistant question must be 1..=16384 bytes");
        }
        validate_wire_id(
            &self.client_message_id,
            "invalid AI Assistant client message id",
        )?;
        validate_selected_capability_ids(&self.selected_capability_ids)
            .and_then(|_| validate_attachment_ids(&self.selected_attachment_ids))
    }
}

fn validate_selected_capability_ids(
    selected_capability_ids: &[String],
) -> Result<(), &'static str> {
    if selected_capability_ids.len() > 16 {
        return Err("too many selected AI Assistant capabilities");
    }
    let mut unique = std::collections::BTreeSet::new();
    for capability_id in selected_capability_ids {
        if capability_id.trim().is_empty()
            || capability_id.len() > 256
            || !unique.insert(capability_id.as_str())
        {
            return Err("invalid or duplicate selected AI Assistant capability");
        }
    }
    Ok(())
}

fn validate_attachment_ids(attachment_ids: &[String]) -> Result<(), &'static str> {
    if attachment_ids.len() > 32 {
        return Err("too many selected AI Assistant attachments");
    }
    let mut unique = std::collections::BTreeSet::new();
    for attachment_id in attachment_ids {
        if attachment_id.trim().is_empty()
            || attachment_id.len() > 512
            || !unique.insert(attachment_id.as_str())
        {
            return Err("invalid or duplicate selected AI Assistant attachment");
        }
    }
    Ok(())
}

/// AI Assistant streams the shared agent-loop event contract over its own
/// signaling discriminant.
pub type AiAssistantEvent = crate::agent_event::AgentEvent;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_round_trips_without_any_mutation_payload() {
        let ask = AiAssistantAsk {
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
        let back: AiAssistantAsk = serde_json::from_value(value).unwrap();
        assert_eq!(back, ask);
    }

    #[test]
    fn context_update_is_bounded_and_ack_contains_no_attachment_secret() {
        let update = AiAssistantContextUpdate {
            conversation_id: "assistant-1".into(),
            client_request_id: "context-change-1".into(),
            selected_capability_ids: vec!["desktop.session.inspect".into()],
        };
        update.validate().unwrap();
        let back: AiAssistantContextUpdate =
            serde_json::from_value(serde_json::to_value(&update).unwrap()).unwrap();
        assert_eq!(back, update);

        let ack = serde_json::to_value(AiAssistantContextUpdated {
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
    fn removed_file_context_operations_are_rejected() {
        for kind in ["attach_file", "refresh_file"] {
            let value = serde_json::json!({"kind":kind,"object_ref":{"token":"file","snapshot_id":"snapshot","object_kind":"file","expires_at":"2030-01-01T00:00:00Z"},"display_summary":"file","stale_attachment_id":"old"});
            assert!(serde_json::from_value::<AiAssistantObjectContextOperation>(value).is_err());
        }
    }

    #[test]
    fn terminal_attachment_update_accepts_only_an_edge_terminal_reference() {
        let object_ref = ObjectRef {
            token: "edge-terminal-token".into(),
            snapshot_id: "worker-1:9".into(),
            object_kind: ObjectKind::TerminalOutput,
            expires_at: "2026-08-25T20:00:00Z".into(),
        };
        let update = AiAssistantObjectContextUpdate {
            conversation_id: "assistant-1".into(),
            client_request_id: "terminal-change-1".into(),
            operation: AiAssistantObjectContextOperation::AttachTerminalOutput {
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
        invalid_kind.operation = AiAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref: ObjectRef {
                object_kind: ObjectKind::File,
                ..object_ref
            },
            display_summary: "not terminal output".into(),
        };
        assert!(invalid_kind.validate().is_err());
    }

    #[test]
    fn window_attachment_update_accepts_only_an_edge_window_reference() {
        let object_ref = ObjectRef {
            token: "edge-window-token".into(),
            snapshot_id: "worker-1:10".into(),
            object_kind: ObjectKind::Window,
            expires_at: "2026-08-25T20:00:00Z".into(),
        };
        let update = AiAssistantObjectContextUpdate {
            conversation_id: "assistant-1".into(),
            client_request_id: "window-change-1".into(),
            operation: AiAssistantObjectContextOperation::AttachWindow {
                object_ref: object_ref.clone(),
                display_summary: "Calculator — Main Window".into(),
            },
        };
        update.validate().unwrap();
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(value["operation"]["kind"], "attach_window");
        assert!(value["operation"].get("window_handle").is_none());
        assert!(value["operation"].get("process_id").is_none());

        let mut invalid_kind = update;
        invalid_kind.operation = AiAssistantObjectContextOperation::AttachWindow {
            object_ref: ObjectRef {
                object_kind: ObjectKind::UiElement,
                ..object_ref
            },
            display_summary: "not a window".into(),
        };
        assert!(invalid_kind.validate().is_err());
    }
}
