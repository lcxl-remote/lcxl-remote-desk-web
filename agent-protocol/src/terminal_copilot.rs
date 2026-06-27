//! Wire types for the in-terminal AI copilot.
//!
//! The copilot is a manager-owned AI control frame, structurally a sibling of
//! [`crate::diagnose`]: a control end sends a [`TerminalCopilotAsk`] and the
//! orchestrator streams back notification-style [`TerminalCopilotEvent`] frames
//! (`request_id` + `seq` + `kind`, closed on the first `Final` / `Error`).
//!
//! Trust boundary (the server is the source of truth):
//! - The target device is **not** in this payload — it rides the outer
//!   `SignalingModel.to_connection_id`, resolved and authorized server-side
//!   exactly like a `Diagnose` frame.
//! - [`TerminalContext`] is a **non-authoritative** prompt hint only; it never
//!   participates in target resolution or authorization.
//! - Each [`CommandSuggestion`] carries a server-computed [`ExecDecision`]; the
//!   control end drives its available actions from that, never from a
//!   model-self-reported field. No suggestion is automatically executable
//!   through the AI path (see the project's suggest-only invariant).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::exec::ExecDecision;
use crate::{AgentError, RiskLevel};

/// Which kind of help the operator asked for.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCopilotMode {
    /// "How do I …" — turn a natural-language intent into command suggestions.
    HowTo,
    /// "Why did this fail / how do I fix it" — explain a terminal error and
    /// suggest a remedy.
    ExplainError,
}

/// Non-authoritative terminal context, supplied by the control end purely as a
/// prompt hint. Redacted and length-capped server-side before any model dial.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalContext {
    /// Operating system family of the terminal host (e.g. `windows` / `linux`).
    pub os: String,
    /// Shell family (e.g. `pwsh` / `bash` / `cmd`).
    pub shell: String,
    /// Current working directory, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Recent terminal scrollback (already truncated by the control end; the
    /// server re-redacts and caps it again).
    pub recent_output: String,
    /// The last command the operator ran, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command: Option<String>,
    /// `ExplainError`: the error passage the operator is asking about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

/// Control end → server request payload. The target device is carried by the
/// outer `SignalingModel.to_connection_id`, **not** here.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalCopilotAsk {
    /// Non-authoritative conversation intent; the orchestrator validates it and
    /// derives a subject-namespaced key, falling back to the request id when
    /// empty / invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub mode: TerminalCopilotMode,
    /// `HowTo`: the operator's natural-language request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// BCP-47 locale of the control end's UI (e.g. `zh-CN`), steering the
    /// model's natural-language answer. Non-authoritative; absent leaves the
    /// model's default language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    pub context: TerminalContext,
}

/// One command the copilot proposes. `risk` / `decision` are server-computed and
/// unforgeable; the control end gates the available actions on `decision`.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CommandSuggestion {
    pub command: String,
    pub shell: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// What this command does, in one line.
    pub note: String,
    /// Risk band from the shared command classifier.
    pub risk: RiskLevel,
    /// Server-computed execution decision. `Blocked` is hidden / never injected;
    /// `NotExecutable` / `ConfirmRequired` are suggest-only (fill / copy) in the
    /// AI path — none is automatically executable.
    pub decision: ExecDecision,
}

/// The copilot's structured answer (delivered in the terminal `Final` frame).
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalCopilotAnswer {
    /// Markdown explanation: the how-to, or the root cause + fix rationale.
    pub explanation_md: String,
    /// Zero or more proposed commands.
    pub suggestions: Vec<CommandSuggestion>,
}

/// Discriminant for a [`TerminalCopilotEvent`] frame.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCopilotEventKind {
    /// A streaming explanation fragment.
    Partial,
    /// A read-only evidence tool was dispatched.
    ToolStarted,
    /// Terminal: the structured answer.
    Final,
    /// Terminal: the request failed.
    Error,
}

/// One streamed frame of a copilot turn (server → control end). Notification
/// style: the control end aggregates by `request_id`, orders by `seq`, and
/// closes the stream on the first `Final` / `Error`. The wire `request_id` is
/// generated by the control end's signaling layer and echoed by the server; it
/// is used for correlation only, not as a trusted field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct TerminalCopilotEvent {
    /// Correlates back to the originating `TerminalCopilotAsk` request.
    pub request_id: String,
    /// Monotonic per-stream sequence number.
    pub seq: u32,
    pub kind: TerminalCopilotEventKind,
    /// `kind = Partial`: an incremental explanation fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_text: Option<String>,
    /// `kind = ToolStarted`: the model-facing name of the read-only tool run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// `kind = Final`: the structured answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<TerminalCopilotAnswer>,
    /// `kind = Error`: the failure (carries `safe_for_model` / `retryable`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentError>,
}

impl TerminalCopilotEvent {
    /// An empty frame of `kind` with all payload fields cleared; the public
    /// constructors set only the field their kind carries.
    fn base(request_id: impl Into<String>, seq: u32, kind: TerminalCopilotEventKind) -> Self {
        Self {
            request_id: request_id.into(),
            seq,
            kind,
            partial_text: None,
            tool_name: None,
            answer: None,
            error: None,
        }
    }

    /// A `Partial` frame carrying a streaming explanation fragment.
    pub fn partial(request_id: impl Into<String>, seq: u32, fragment: impl Into<String>) -> Self {
        Self {
            partial_text: Some(fragment.into()),
            ..Self::base(request_id, seq, TerminalCopilotEventKind::Partial)
        }
    }

    /// A `ToolStarted` frame: a read-only evidence tool was dispatched.
    pub fn tool_started(
        request_id: impl Into<String>,
        seq: u32,
        tool_name: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: Some(tool_name.into()),
            ..Self::base(request_id, seq, TerminalCopilotEventKind::ToolStarted)
        }
    }

    /// A terminal `Final` frame carrying the structured answer.
    pub fn final_answer(
        request_id: impl Into<String>,
        seq: u32,
        answer: TerminalCopilotAnswer,
    ) -> Self {
        Self {
            answer: Some(answer),
            ..Self::base(request_id, seq, TerminalCopilotEventKind::Final)
        }
    }

    /// A terminal `Error` frame.
    pub fn error(request_id: impl Into<String>, seq: u32, error: AgentError) -> Self {
        Self {
            error: Some(error),
            ..Self::base(request_id, seq, TerminalCopilotEventKind::Error)
        }
    }

    /// Whether this frame ends its request's stream (`Final` / `Error`).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            TerminalCopilotEventKind::Final | TerminalCopilotEventKind::Error
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentErrorKind;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    fn sample_answer() -> TerminalCopilotAnswer {
        TerminalCopilotAnswer {
            explanation_md: "Port 8080 is held by another process.".into(),
            suggestions: vec![CommandSuggestion {
                command: "lsof -i :8080".into(),
                shell: "bash".into(),
                cwd: None,
                note: "List the process holding port 8080.".into(),
                risk: RiskLevel::Low,
                decision: ExecDecision::NotExecutable,
            }],
        }
    }

    fn sample_error() -> AgentError {
        AgentError {
            kind: AgentErrorKind::Internal,
            message: "boom".into(),
            retryable: false,
            safe_for_model: true,
        }
    }

    #[test]
    fn only_final_and_error_are_terminal() {
        assert!(!TerminalCopilotEvent::partial("r", 0, "x").is_terminal());
        assert!(!TerminalCopilotEvent::tool_started("r", 1, "read_process_list").is_terminal());
        assert!(TerminalCopilotEvent::final_answer("r", 2, sample_answer()).is_terminal());
        assert!(TerminalCopilotEvent::error("r", 3, sample_error()).is_terminal());
    }

    #[test]
    fn constructors_set_only_their_payload() {
        let p = TerminalCopilotEvent::partial("r", 0, "frag");
        assert_eq!(p.partial_text.as_deref(), Some("frag"));
        assert!(p.tool_name.is_none() && p.answer.is_none() && p.error.is_none());

        let f = TerminalCopilotEvent::final_answer("r", 1, sample_answer());
        assert!(f.answer.is_some());
        assert!(f.partial_text.is_none() && f.tool_name.is_none() && f.error.is_none());
    }

    #[test]
    fn event_wire_round_trips() {
        let cfg = unbounded_config();
        let original = TerminalCopilotEvent::final_answer("req-1", 7, sample_answer());
        let bytes = wincode::config::serialize(&original, cfg).expect("wincode encode");
        let decoded: TerminalCopilotEvent =
            wincode::config::deserialize(&bytes, cfg).expect("wincode decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ask_wire_round_trips() {
        let cfg = unbounded_config();
        let original = TerminalCopilotAsk {
            conversation_id: Some("c-1".into()),
            mode: TerminalCopilotMode::ExplainError,
            question: None,
            locale: Some("zh-CN".into()),
            context: TerminalContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: Some("/srv".into()),
                recent_output: "bind: address already in use".into(),
                last_command: Some("./server".into()),
                error_text: Some("address already in use".into()),
            },
        };
        let bytes = wincode::config::serialize(&original, cfg).expect("wincode encode");
        let decoded: TerminalCopilotAsk =
            wincode::config::deserialize(&bytes, cfg).expect("wincode decode");
        assert_eq!(decoded, original);
    }
}
