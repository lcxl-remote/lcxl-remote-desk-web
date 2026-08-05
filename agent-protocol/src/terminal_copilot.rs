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

use crate::content_safety::StreamRetractionReason;
use crate::exec::ExecDecision;
use crate::provenance::AiProvenance;
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

/// One prior exchange in a multi-turn copilot conversation, supplied by the
/// control end so a **stateless** central brain can continue the thread without
/// server-side session storage. Non-authoritative prompt context only: the server
/// re-redacts and length-caps it before any model dial.
///
/// Used only by the stateless OSS signal path. The manager path keeps its own
/// DB-authoritative session (keyed by the subject-namespaced conversation key) and
/// **ignores** this field — the control end can never inject or rewrite history
/// there.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CopilotHistoryTurn {
    /// The operator's message for that turn (their request, or the error passage
    /// they asked about).
    pub user: String,
    /// The assistant's prior answer text (the explanation prose). Suggestions are
    /// not replayed — only the prose is needed as conversational context.
    pub assistant: String,
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
    /// Prior turns of this conversation, oldest first. Consumed by the stateless
    /// signal path to continue a multi-turn thread; **ignored** by the manager
    /// path (its DB session is authoritative). Server-capped and re-redacted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<CopilotHistoryTurn>,
    pub context: TerminalContext,
    /// Manager-only model selection: the id of a `model` catalog row the operator
    /// picked in the selector. The manager authorizes it against the operator's
    /// gated catalog and pins it for the whole request. The open-source
    /// single-model desk-server has no model catalog and **ignores** this field;
    /// `None` (the default, sent by older control ends and every non-manager
    /// client) leaves the server to pick its default model.
    #[serde(default)]
    pub model_id: Option<i32>,
    /// Manager-only org context hint: the id of the organization the operator is
    /// acting within (the org view of the model selector). NON-authoritative — the
    /// manager validates the operator's membership in this org AND the org's
    /// device-access grant to the target device before trusting it, and only then
    /// resolves the model against the org catalog; a hint that fails either check
    /// is dropped and the request falls back to the personal view. The open-source
    /// single-instance desk-server has no org concept and **ignores** this field;
    /// `None` (the default, sent by older control ends and every non-manager
    /// client) is the personal view.
    #[serde(default)]
    pub org_id: Option<i32>,
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
    /// Marks all provisional explanation text since the previous commit.
    /// Carries no text and is not terminal.
    PartialCommitted,
    /// Terminal: clears provisional text and selects a fixed local message from
    /// the closed retraction reason.
    Retracted,
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
    /// `kind = Retracted`: closed reason for clearing provisional text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retraction_reason: Option<StreamRetractionReason>,
    /// Machine-readable AI marking for the `Final` (answer) content frame. Absent
    /// on non-content frames (partial / tool / error). Its absence on a content
    /// frame does not mean "not AI" — the frame kind already establishes that;
    /// consumers mark such frames AI regardless (fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<AiProvenance>,
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
            retraction_reason: None,
            provenance: None,
        }
    }

    /// Attach machine-readable AI provenance to the `Final` content frame.
    /// Emitters call this on the answer frame; a frame without it is still
    /// treated as AI by its kind (fail-closed).
    pub fn with_provenance(mut self, provenance: AiProvenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    /// A `Partial` frame carrying a streaming explanation fragment.
    pub fn partial(request_id: impl Into<String>, seq: u32, fragment: impl Into<String>) -> Self {
        Self {
            partial_text: Some(fragment.into()),
            ..Self::base(request_id, seq, TerminalCopilotEventKind::Partial)
        }
    }
    /// A non-terminal marker committing provisional explanation text.
    pub fn partial_committed(request_id: impl Into<String>, seq: u32) -> Self {
        Self::base(request_id, seq, TerminalCopilotEventKind::PartialCommitted)
    }

    /// A terminal retraction that never repeats the removed model text.
    pub fn retracted(
        request_id: impl Into<String>,
        seq: u32,
        reason: StreamRetractionReason,
        error: Option<AgentError>,
    ) -> Self {
        Self {
            retraction_reason: Some(reason),
            error,
            ..Self::base(request_id, seq, TerminalCopilotEventKind::Retracted)
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

    /// Whether this frame ends its request's stream. A commit marker is not
    /// terminal; a retraction is.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            TerminalCopilotEventKind::Final
                | TerminalCopilotEventKind::Error
                | TerminalCopilotEventKind::Retracted
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
            error_code: None,
        }
    }

    #[test]
    fn final_error_and_retracted_are_terminal() {
        assert!(!TerminalCopilotEvent::partial("r", 0, "x").is_terminal());
        assert!(!TerminalCopilotEvent::tool_started("r", 1, "read_process_list").is_terminal());
        assert!(TerminalCopilotEvent::final_answer("r", 2, sample_answer()).is_terminal());
        assert!(TerminalCopilotEvent::error("r", 3, sample_error()).is_terminal());
        assert!(!TerminalCopilotEvent::partial_committed("r", 2).is_terminal());
        let retracted =
            TerminalCopilotEvent::retracted("r", 3, StreamRetractionReason::PolicyBlocked, None);
        assert!(retracted.is_terminal());
        let json = serde_json::to_string(&retracted).expect("retraction JSON");
        assert!(json.contains("\"kind\":\"retracted\""));
        assert!(json.contains("\"retraction_reason\":\"policy_blocked\""));
        assert!(!json.contains("partial_text"));
        let cfg = unbounded_config();
        let bytes = wincode::config::serialize(&retracted, cfg).expect("wincode encode");
        let decoded: TerminalCopilotEvent =
            wincode::config::deserialize(&bytes, cfg).expect("wincode decode");
        assert_eq!(decoded, retracted);
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
    fn final_frame_carries_provenance_and_round_trips() {
        use crate::provenance::{AI_MARKING_SCHEME_V1, AiProvenance};

        let original =
            TerminalCopilotEvent::final_answer("req-1", 7, sample_answer()).with_provenance(
                AiProvenance::stamp(Some("gpt-4o".into()), Some("2026-07-14T00:00:00Z".into())),
            );
        let prov = original
            .provenance
            .as_ref()
            .expect("final frame carries provenance");
        assert_eq!(prov.marking_scheme.as_deref(), Some(AI_MARKING_SCHEME_V1));

        let json = serde_json::to_string(&original).expect("json encode");
        let back: TerminalCopilotEvent = serde_json::from_str(&json).expect("json decode");
        assert_eq!(original, back);

        let cfg = unbounded_config();
        let bytes = wincode::config::serialize(&original, cfg).expect("wincode encode");
        let decoded: TerminalCopilotEvent =
            wincode::config::deserialize(&bytes, cfg).expect("wincode decode");
        assert_eq!(decoded, original);
    }

    /// A non-content frame (partial) omits provenance entirely from its JSON.
    #[test]
    fn partial_frame_omits_provenance() {
        let p = TerminalCopilotEvent::partial("r", 0, "frag");
        assert!(p.provenance.is_none());
        let json = serde_json::to_string(&p).expect("json encode");
        assert!(
            !json.contains("provenance"),
            "partial frame should omit provenance: {json}"
        );
    }

    #[test]
    fn ask_wire_round_trips() {
        let cfg = unbounded_config();
        let original = TerminalCopilotAsk {
            conversation_id: Some("c-1".into()),
            mode: TerminalCopilotMode::ExplainError,
            question: None,
            locale: Some("zh-CN".into()),
            history: vec![CopilotHistoryTurn {
                user: "how do I list listeners?".into(),
                assistant: "Use ss or lsof.".into(),
            }],
            context: TerminalContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: Some("/srv".into()),
                recent_output: "bind: address already in use".into(),
                last_command: Some("./server".into()),
                error_text: Some("address already in use".into()),
            },
            model_id: Some(9),
            org_id: Some(4),
        };
        let bytes = wincode::config::serialize(&original, cfg).expect("wincode encode");
        let decoded: TerminalCopilotAsk =
            wincode::config::deserialize(&bytes, cfg).expect("wincode decode");
        assert_eq!(decoded, original);
    }

    /// The manager-only `model_id` is `#[serde(default)]`: a body carrying it
    /// decodes to `Some`, and one omitting it (an open-source desk-server or an
    /// older control end) decodes to `None`. Wire-parity guarantee for the field.
    #[test]
    fn model_id_is_serde_default_for_wire_parity() {
        let base = r#""mode":"how_to","context":{"os":"linux","shell":"bash","recent_output":""}"#;
        let with_model: TerminalCopilotAsk =
            serde_json::from_str(&format!("{{{base},\"model_id\":7}}")).expect("decode with model");
        assert_eq!(with_model.model_id, Some(7));

        let without_model: TerminalCopilotAsk =
            serde_json::from_str(&format!("{{{base}}}")).expect("decode without model");
        assert_eq!(
            without_model.model_id, None,
            "an omitted model_id decodes to None (open-source / legacy parity)"
        );
    }

    /// The manager-only `org_id` context hint is `#[serde(default)]`, like
    /// `model_id`: a body carrying it decodes to `Some`, and one omitting it decodes
    /// to `None`. Wire-parity guarantee for the field.
    #[test]
    fn org_id_is_serde_default_for_wire_parity() {
        let base = r#""mode":"how_to","context":{"os":"linux","shell":"bash","recent_output":""}"#;
        let with_org: TerminalCopilotAsk =
            serde_json::from_str(&format!("{{{base},\"org_id\":8}}")).expect("decode with org");
        assert_eq!(with_org.org_id, Some(8));

        let without_org: TerminalCopilotAsk =
            serde_json::from_str(&format!("{{{base}}}")).expect("decode without org");
        assert_eq!(
            without_org.org_id, None,
            "an omitted org_id decodes to None (open-source / legacy parity)"
        );
    }
}
