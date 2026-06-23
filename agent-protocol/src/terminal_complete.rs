//! Wire types for the in-terminal AI command completion (ghost-text).
//!
//! Completion is a sibling of [`crate::terminal_copilot`] but a **non-streaming**
//! request/response: the control end sends a [`TerminalCompleteAsk`] carrying the
//! command prefix the operator is typing, and the orchestrator answers with a
//! single [`TerminalCompleteResult`] (best-first candidates, or an error). The
//! control end keys each result on the echoed `request_id` and discards stale
//! ones, so an overtaken in-flight ask is simply ignored.
//!
//! Trust boundary (the server is the source of truth):
//! - The target device is **not** in this payload — it rides the outer
//!   `SignalingModel.to_connection_id`, resolved and authorized server-side
//!   exactly like a `Diagnose` / `TerminalCopilotAsk` frame.
//! - [`TerminalCompletionContext`] is a **non-authoritative** prompt hint only; it
//!   never participates in target resolution or authorization.
//! - Each [`CommandCompletion`] carries a server-computed `risk` / [`ExecDecision`]
//!   over the *full* command (`prefix` + `completion`); the control end gates its
//!   actions on that, never on a model-self-reported field. Accepting a candidate
//!   fills the input (suggest-only invariant) — it is never auto-run, and a
//!   `Blocked` candidate is hidden entirely.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::exec::ExecDecision;
use crate::{AgentError, RiskLevel};

/// Non-authoritative terminal context for a completion ask, supplied by the
/// control end purely as a prompt hint. Redacted and length-capped server-side
/// before any model dial. Deliberately leaner than
/// [`crate::terminal_copilot::TerminalContext`]: completion is latency-sensitive,
/// so only the environment plus a short scrollback is carried.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalCompletionContext {
    /// Operating system family of the terminal host (e.g. `windows` / `linux`).
    pub os: String,
    /// Shell family (e.g. `pwsh` / `bash` / `cmd`).
    pub shell: String,
    /// Current working directory, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Recent terminal scrollback (already truncated by the control end; the
    /// server re-redacts and caps it again).
    #[serde(default)]
    pub recent_output: String,
}

/// Control end → server completion request. The target device is carried by the
/// outer `SignalingModel.to_connection_id`, **not** here.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalCompleteAsk {
    /// The partial command the operator has typed so far. The completion(s) are
    /// the suffix(es) that complete it; the server classifies `prefix + completion`
    /// as the full command.
    pub prefix: String,
    pub context: TerminalCompletionContext,
}

/// One completion candidate. `risk` / `decision` are server-computed over the full
/// command (`prefix` + `completion`) and unforgeable; the control end gates its
/// actions on `decision` (a `Blocked` candidate is hidden, never shown as ghost
/// text).
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CommandCompletion {
    /// The suffix that completes the operator's prefix (the ghost text). Accepting
    /// it fills `prefix + completion` into the input without executing it.
    pub completion: String,
    /// What the completed command does, in one line.
    pub note: String,
    /// Risk band of the full command from the shared classifier.
    pub risk: RiskLevel,
    /// Server-computed execution decision for the full command. `Blocked` is
    /// hidden; nothing is auto-run through the AI path (suggest-only invariant).
    pub decision: ExecDecision,
}

/// Server → control end completion response (non-streaming). On success
/// `completions` is the best-first candidate list (possibly empty when nothing
/// applies); on failure `error` is set and `completions` is empty. The
/// `request_id` echoes the originating ask for correlation only — it is not a
/// trusted field.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalCompleteResult {
    /// Correlates back to the originating [`TerminalCompleteAsk`].
    pub request_id: String,
    /// Best-first completion candidates. Empty when nothing applies or on error.
    #[serde(default)]
    pub completions: Vec<CommandCompletion>,
    /// Set when the request failed (quota, redaction, model). On error
    /// `completions` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentError>,
}

impl TerminalCompleteResult {
    /// A successful result carrying the best-first candidate list.
    pub fn ok(request_id: impl Into<String>, completions: Vec<CommandCompletion>) -> Self {
        Self {
            request_id: request_id.into(),
            completions,
            error: None,
        }
    }

    /// A failed result: no candidates, the failure carried in `error`.
    pub fn failed(request_id: impl Into<String>, error: AgentError) -> Self {
        Self {
            request_id: request_id.into(),
            completions: Vec::new(),
            error: Some(error),
        }
    }

    /// Whether this result reports a failure.
    pub fn is_error(&self) -> bool {
        self.error.is_some()
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

    fn sample_completion() -> CommandCompletion {
        CommandCompletion {
            completion: "ctl status nginx".into(),
            note: "Show the nginx service status.".into(),
            risk: RiskLevel::Low,
            decision: ExecDecision::NotExecutable,
        }
    }

    #[test]
    fn ok_and_failed_constructors_are_disjoint() {
        let ok = TerminalCompleteResult::ok("r", vec![sample_completion()]);
        assert!(!ok.is_error());
        assert_eq!(ok.completions.len(), 1);
        assert!(ok.error.is_none());

        let err = TerminalCompleteResult::failed(
            "r",
            AgentError {
                kind: AgentErrorKind::SessionUnavailable,
                message: "busy".into(),
                retryable: true,
                safe_for_model: true,
            },
        );
        assert!(err.is_error());
        assert!(err.completions.is_empty());
    }

    #[test]
    fn ask_wire_round_trips() {
        let cfg = unbounded_config();
        let original = TerminalCompleteAsk {
            prefix: "system".into(),
            context: TerminalCompletionContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: Some("/srv".into()),
                recent_output: "$ systemctl status".into(),
            },
        };
        let bytes = wincode::config::serialize(&original, cfg).expect("wincode encode");
        let decoded: TerminalCompleteAsk =
            wincode::config::deserialize(&bytes, cfg).expect("wincode decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn result_wire_round_trips() {
        let cfg = unbounded_config();
        let original = TerminalCompleteResult::ok("req-1", vec![sample_completion()]);
        let bytes = wincode::config::serialize(&original, cfg).expect("wincode encode");
        let decoded: TerminalCompleteResult =
            wincode::config::deserialize(&bytes, cfg).expect("wincode decode");
        assert_eq!(decoded, original);
    }
}
