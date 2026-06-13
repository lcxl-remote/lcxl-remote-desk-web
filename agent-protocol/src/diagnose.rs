//! AI Diagnose orchestration wire types (control end ↔ server).
//!
//! These ride the `Diagnose` / `DiagnoseEvent` signaling types. Unlike a raw
//! capability call ([`crate::AgentEnvelope`]), diagnose is an
//! **orchestrator-layer** request: a user question + collection options go in,
//! a streamed structured [`Diagnosis`] comes out.
//!
//! [`DiagnoseEvent`] is a **notification-style stream** — `request_id` + `seq` +
//! `kind`, never a one-shot response. The signaling layer's per-`request_id`
//! callback map consumes the first matching response frame and drops the rest,
//! so streaming frames must be emitted with `response_state = None` and
//! correlated here by `seq`/`kind` instead (the router enforces this).
//!
//! All types derive `serde` (JSON control-end wire), `wincode` (so they can
//! later cross the daemon ↔ worker IPC unchanged), and `utoipa::ToSchema`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::{AgentError, RiskLevel};

/// Control end → server: start a diagnosis. Carries only non-authoritative
/// intent; the server owns target/actor/scope just as it does for
/// [`crate::AgentRequestData`].
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DiagnoseRequestData {
    /// The user's question.
    pub question: String,
    /// Whether to include a screenshot. Honoured only if the server policy
    /// (`allow_screen`) also permits it.
    #[serde(default)]
    pub include_screen: bool,
    /// Optional explicit context selection (dotted capability names). Empty =
    /// the server's default read set.
    #[serde(default)]
    pub context_kinds: Vec<String>,
}

/// Confidence the model assigns to a diagnosis.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    /// Default for an empty / degraded diagnosis (e.g. structured parse fell
    /// back to raw text).
    #[default]
    Low,
}

/// One diagnostic finding with references back into the collected evidence.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct Finding {
    pub title: String,
    /// References into the evidence (e.g. `"network.ports[3]"`).
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub explanation: String,
}

/// A command the model suggests. M1b is **suggest-only** — nothing executes;
/// `requires_confirmation` is a placeholder the M2 confirm flow will honour.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SuggestedCommand {
    pub shell: String,
    pub command: String,
    pub purpose: String,
    pub risk: RiskLevel,
    pub requires_confirmation: bool,
}

/// The structured diagnosis the UI renders (Summary / Evidence / Suggested
/// commands / Next steps / Data collected).
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct Diagnosis {
    pub summary: String,
    pub confidence: Confidence,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub commands: Vec<SuggestedCommand>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    /// What the model says it could not determine / is missing.
    #[serde(default)]
    pub missing_info: Vec<String>,
    /// Dotted capability names actually collected for this diagnosis.
    #[serde(default)]
    pub collected: Vec<String>,
}

/// Kind of a streamed [`DiagnoseEvent`] frame. `Final` and `Error` are
/// terminal.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnoseEventKind {
    /// A lifecycle status update (collecting / redacting / modeling / ...).
    Status,
    /// An incremental summary token from the streaming model.
    Partial,
    /// Terminal: the structured result.
    Final,
    /// Terminal: the diagnosis failed.
    Error,
}

/// One streamed frame of a diagnosis (server → control end). Notification-style:
/// the control end aggregates by `request_id`, orders by `seq`, and closes the
/// stream on the first `Final` / `Error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct DiagnoseEvent {
    /// Correlates back to the originating `Diagnose` request.
    pub request_id: String,
    /// Monotonic per-stream sequence number.
    pub seq: u32,
    pub kind: DiagnoseEventKind,
    /// `kind = Status`: the lifecycle phase name.
    #[serde(default)]
    pub status: Option<String>,
    /// `kind = Partial`: an incremental summary fragment.
    #[serde(default)]
    pub partial_summary: Option<String>,
    /// `kind = Final`: the structured result.
    #[serde(default)]
    pub final_result: Option<Diagnosis>,
    /// `kind = Error`: the failure (uses `AgentError` so `safe_for_model` /
    /// `retryable` carry through to the UI).
    #[serde(default)]
    pub error: Option<AgentError>,
}

impl DiagnoseEvent {
    /// A `Status` frame announcing a lifecycle phase.
    pub fn status(request_id: impl Into<String>, seq: u32, phase: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            seq,
            kind: DiagnoseEventKind::Status,
            status: Some(phase.into()),
            partial_summary: None,
            final_result: None,
            error: None,
        }
    }

    /// A `Partial` frame carrying a streaming summary fragment.
    pub fn partial(request_id: impl Into<String>, seq: u32, fragment: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            seq,
            kind: DiagnoseEventKind::Partial,
            status: None,
            partial_summary: Some(fragment.into()),
            final_result: None,
            error: None,
        }
    }

    /// A terminal `Final` frame carrying the structured diagnosis.
    pub fn final_result(request_id: impl Into<String>, seq: u32, diagnosis: Diagnosis) -> Self {
        Self {
            request_id: request_id.into(),
            seq,
            kind: DiagnoseEventKind::Final,
            status: None,
            partial_summary: None,
            final_result: Some(diagnosis),
            error: None,
        }
    }

    /// A terminal `Error` frame.
    pub fn error(request_id: impl Into<String>, seq: u32, error: AgentError) -> Self {
        Self {
            request_id: request_id.into(),
            seq,
            kind: DiagnoseEventKind::Error,
            status: None,
            partial_summary: None,
            final_result: None,
            error: Some(error),
        }
    }

    /// Whether this is a terminal frame (`Final` or `Error`).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            DiagnoseEventKind::Final | DiagnoseEventKind::Error
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

    fn sample_diagnosis() -> Diagnosis {
        Diagnosis {
            summary: "Port 8080 is already used by old-api.exe.".into(),
            confidence: Confidence::High,
            findings: vec![Finding {
                title: "Port conflict".into(),
                evidence_refs: vec!["network.ports[3]".into()],
                explanation: "old-api.exe (pid 1234) holds 8080.".into(),
            }],
            commands: vec![SuggestedCommand {
                shell: "powershell".into(),
                command: "Get-NetTCPConnection -LocalPort 8080".into(),
                purpose: "Confirm the owner process".into(),
                risk: RiskLevel::Low,
                requires_confirmation: false,
            }],
            next_steps: vec!["Decide whether to stop the conflicting service".into()],
            missing_info: vec![],
            collected: vec!["network.ports".into(), "process.list".into()],
        }
    }

    #[test]
    fn request_data_round_trips_and_omits_nothing() {
        let req = DiagnoseRequestData {
            question: "Why is the container failing?".into(),
            include_screen: true,
            context_kinds: vec!["container.list".into(), "container.logs".into()],
        };
        let json = serde_json::to_string(&req).expect("json encode");
        let back: DiagnoseRequestData = serde_json::from_str(&json).expect("json decode");
        assert_eq!(req, back);

        let config = unbounded_config();
        let bytes = wincode::config::serialize(&req, config).expect("wincode encode");
        let back2: DiagnoseRequestData =
            wincode::config::deserialize(&bytes, config).expect("wincode decode");
        assert_eq!(req, back2);
    }

    #[test]
    fn diagnose_event_frames_round_trip_each_kind() {
        let config = unbounded_config();
        let frames = [
            DiagnoseEvent::status("req_1", 0, "collecting"),
            DiagnoseEvent::partial("req_1", 1, "Port 8080 ..."),
            DiagnoseEvent::final_result("req_1", 2, sample_diagnosis()),
            DiagnoseEvent::error(
                "req_1",
                3,
                AgentError {
                    kind: AgentErrorKind::RedactionFailed,
                    message: "redactor failed".into(),
                    retryable: false,
                    safe_for_model: true,
                },
            ),
        ];
        for frame in frames {
            let json = serde_json::to_string(&frame).expect("json encode");
            let back: DiagnoseEvent = serde_json::from_str(&json).expect("json decode");
            assert_eq!(frame, back);

            let bytes = wincode::config::serialize(&frame, config).expect("wincode encode");
            let back2: DiagnoseEvent =
                wincode::config::deserialize(&bytes, config).expect("wincode decode");
            assert_eq!(frame, back2);
        }
    }

    #[test]
    fn only_final_and_error_are_terminal() {
        assert!(!DiagnoseEvent::status("r", 0, "x").is_terminal());
        assert!(!DiagnoseEvent::partial("r", 1, "y").is_terminal());
        assert!(DiagnoseEvent::final_result("r", 2, Diagnosis::default()).is_terminal());
        assert!(
            DiagnoseEvent::error(
                "r",
                3,
                AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "x".into(),
                    retryable: true,
                    safe_for_model: true,
                },
            )
            .is_terminal()
        );
    }

    #[test]
    fn confidence_defaults_to_low() {
        assert_eq!(Confidence::default(), Confidence::Low);
        assert_eq!(Diagnosis::default().confidence, Confidence::Low);
    }

    #[test]
    fn utoipa_schema_is_generated() {
        use utoipa::PartialSchema;
        let _ = DiagnoseRequestData::schema();
        let _ = DiagnoseEvent::schema();
        let _ = Diagnosis::schema();
    }
}
