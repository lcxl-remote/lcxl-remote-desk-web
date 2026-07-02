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

use crate::{AgentError, AgentErrorKind, RiskLevel};

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
    /// BCP-47 locale tag of the control-end UI (e.g. `zh-CN`, `en-US`) so the
    /// model answers in the user's language. `None`/empty leaves the model's
    /// default (English). Only steers natural-language text; the JSON shape and
    /// enum values stay in English.
    #[serde(default)]
    pub locale: Option<String>,
    /// Client intent: a stable id so follow-up questions continue the same
    /// agentic session and the model sees the prior turns. The server NEVER uses
    /// this value as a storage key directly — it derives a subject-namespaced
    /// `conversation_key` from it (see the diagnose entry). `None`/empty starts a
    /// fresh single-question conversation. Non-authoritative; shape-validated
    /// (trimmed, length- and charset-bounded) before use.
    #[serde(default)]
    pub conversation_id: Option<String>,
}

// ===================== Remote-collect RPC (A ↔ B) =====================
//
// In the thin-edge model the orchestrator runs centrally (A). To gather
// evidence it asks the edge (B) to run its read-only collectors over the
// already-established B→A signaling socket: A pushes a `CollectRequest`, B
// replies with a chunked `CollectResponse` carrying the serialized
// `EvidenceSnapshot`. These ride dedicated `SignalingType` variants; A is the
// only party permitted to issue a request and the only party permitted to
// consume a response (enforced at the signaling layer).

/// Per-frame size limit the manager's signaling WebSocket endpoint must accept
/// so a chunked [`CollectResponse`] can be received. A chunk rides the signaling
/// socket — not the daemon↔worker IPC channel — as a single (unfragmented) WS
/// text frame. The actix-ws codec defaults its per-frame ceiling to 64 KiB and
/// rejects anything larger with `ProtocolError::Overflow` ("payload reached size
/// limit") *before* continuation aggregation, dropping the host connection. The
/// manager therefore raises `max_frame_size` (and `max_continuation_size`) to
/// this value; [`COLLECT_CHUNK_PAYLOAD_LIMIT`] is held below it with headroom for
/// the surrounding JSON + `SignalingModel` envelope.
pub const SIGNALING_FRAME_LIMIT: usize = 1024 * 1024;

/// Upper bound on the base64 `payload_b64` slice in a single
/// [`CollectResponseChunk`]. Held well below [`SIGNALING_FRAME_LIMIT`] so the
/// surrounding JSON envelope (request id, seq, totals, hash, and the
/// `SignalingModel` wrapper) cannot push the wire frame past the WebSocket's
/// continuation cap. This budgets the inflated base64 text (base64 grows raw
/// bytes by ~4/3, so the raw slice per chunk is ~3/4 of this); the 256 KiB of
/// headroom under the cap dwarfs the few hundred bytes of envelope while keeping
/// the chunk count small for typical snapshots.
pub const COLLECT_CHUNK_PAYLOAD_LIMIT: usize = 768 * 1024;

/// A→B: ask the edge to collect read-only evidence for a diagnosis.
///
/// Only the central manager may issue this; the signaling layer drops a
/// `CollectRequest` arriving from any other source. The edge re-runs its local
/// `select_capabilities` gate against its own policy before collecting, so the
/// edge keeps final say over what evidence may leave the machine.
///
/// v1 reuses [`DiagnoseRequestData`] and therefore only drives the
/// parameter-free capabilities the generic selection path can build
/// (`system.info` / `process.list` / `network.ports` / `service.status` /
/// `log.recent` / `container.list` / `screen.capture.current`).
/// `container.inspect` / `container.logs` need a caller-supplied container id
/// and are **not** in v1; a future revision extends this with an explicit
/// read-context list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectRequest {
    /// Correlates the response chunks back to this request.
    pub request_id: String,
    /// The collection intent (question + `context_kinds` + `include_screen` +
    /// `locale`). Authoritative fields (actor / device) stay on A.
    pub request: DiagnoseRequestData,
}

/// One chunk of a B→A evidence response.
///
/// The edge serializes the whole [`crate::evidence::EvidenceSnapshot`] **once**
/// to JSON bytes, slices those bytes on byte boundaries (so multi-byte UTF-8 is
/// never split), and base64-encodes each slice into `payload_b64`. A reassembles
/// by ordering on `seq`, concatenating the decoded bytes, verifying `total_len`
/// and the final `sha256`, then deserializing the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectResponseChunk {
    /// Correlates back to the originating [`CollectRequest`].
    pub request_id: String,
    /// Zero-based, monotonic chunk index.
    pub seq: u32,
    /// Whether this is the final chunk.
    pub last: bool,
    /// Total length, in bytes, of the full (pre-base64) JSON byte stream.
    pub total_len: u64,
    /// Base64 of this chunk's byte slice.
    pub payload_b64: String,
    /// Hex SHA-256 of the full byte stream. Set only on the final chunk
    /// (`last = true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// B→A: the collection failed wholesale (gate denied, redaction failed, a fatal
/// collector error). Distinct from a per-capability error, which rides inside
/// the snapshot as an [`crate::AgentOutcome::Err`] entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectResponseError {
    /// Correlates back to the originating [`CollectRequest`].
    pub request_id: String,
    /// Structured failure class, so the central orchestrator can persist the
    /// right audit event (a fail-closed `RedactionFailed` is a distinct security
    /// event from a generic collection error) without parsing `reason`. Named
    /// `error_kind` to avoid colliding with the [`CollectResponse`] enum's `kind`
    /// discriminant tag.
    pub error_kind: AgentErrorKind,
    /// Model-safe reason describing why collection failed.
    pub reason: String,
}

/// B→A response frame for a [`CollectRequest`]: either a chunk of the evidence
/// snapshot or a wholesale failure. Carried by one `SignalingType` variant so
/// the manager has a single consume path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CollectResponse {
    Chunk(CollectResponseChunk),
    Error(CollectResponseError),
}

impl CollectResponse {
    /// The `request_id` this response correlates to, regardless of variant.
    pub fn request_id(&self) -> &str {
        match self {
            CollectResponse::Chunk(c) => &c.request_id,
            CollectResponse::Error(e) => &e.request_id,
        }
    }
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
    /// An incremental summary / answer token from the streaming model.
    Partial,
    /// Terminal: the structured result (single-turn diagnose).
    Final,
    /// Terminal: the diagnosis failed.
    Error,
    /// An agentic turn has started (carries `turn_id`).
    TurnStarted,
    /// A tool call was dispatched (read tool) or is awaiting approval (mutating
    /// tool); carries `tool_name` + `tool_call_id` (and `awaiting_approval`).
    ToolStarted,
    /// A dispatched tool call produced its result; carries `tool_call_id` +
    /// `tool_ok`.
    ToolFinished,
    /// Terminal: the agentic turn committed a final natural-language answer
    /// (carries `answer`). Distinct from [`Final`], which carries a structured
    /// [`Diagnosis`] for the single-turn path.
    ///
    /// [`Final`]: DiagnoseEventKind::Final
    Answer,
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
    /// `kind = TurnStarted`: the id of the agentic turn that started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// `kind = ToolStarted`: the model-facing name of the tool being run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// `kind = ToolStarted` / `ToolFinished`: the tool call id, correlating the
    /// start and finish of one call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// `kind = ToolStarted`: whether the tool is a mutating one waiting for the
    /// operator's approval (vs a read tool that runs immediately).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_approval: bool,
    /// `kind = ToolFinished`: whether the call produced a usable result (vs an
    /// error / rejection / unknown outcome).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_ok: Option<bool>,
    /// `kind = Answer`: the agentic turn's final natural-language answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

impl DiagnoseEvent {
    /// An empty frame of `kind` with all payload fields cleared; the public
    /// constructors set only the field their kind carries.
    fn base(request_id: impl Into<String>, seq: u32, kind: DiagnoseEventKind) -> Self {
        Self {
            request_id: request_id.into(),
            seq,
            kind,
            status: None,
            partial_summary: None,
            final_result: None,
            error: None,
            turn_id: None,
            tool_name: None,
            tool_call_id: None,
            awaiting_approval: false,
            tool_ok: None,
            answer: None,
        }
    }

    /// A `Status` frame announcing a lifecycle phase.
    pub fn status(request_id: impl Into<String>, seq: u32, phase: impl Into<String>) -> Self {
        Self {
            status: Some(phase.into()),
            ..Self::base(request_id, seq, DiagnoseEventKind::Status)
        }
    }

    /// A `Partial` frame carrying a streaming summary / answer fragment.
    pub fn partial(request_id: impl Into<String>, seq: u32, fragment: impl Into<String>) -> Self {
        Self {
            partial_summary: Some(fragment.into()),
            ..Self::base(request_id, seq, DiagnoseEventKind::Partial)
        }
    }

    /// A terminal `Final` frame carrying the structured diagnosis (single-turn).
    pub fn final_result(request_id: impl Into<String>, seq: u32, diagnosis: Diagnosis) -> Self {
        Self {
            final_result: Some(diagnosis),
            ..Self::base(request_id, seq, DiagnoseEventKind::Final)
        }
    }

    /// A terminal `Error` frame.
    pub fn error(request_id: impl Into<String>, seq: u32, error: AgentError) -> Self {
        Self {
            error: Some(error),
            ..Self::base(request_id, seq, DiagnoseEventKind::Error)
        }
    }

    /// A `TurnStarted` frame announcing an agentic turn.
    pub fn turn_started(
        request_id: impl Into<String>,
        seq: u32,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            turn_id: Some(turn_id.into()),
            ..Self::base(request_id, seq, DiagnoseEventKind::TurnStarted)
        }
    }

    /// A `ToolStarted` frame: a read tool dispatched, or — when
    /// `awaiting_approval` — a mutating tool waiting for the operator.
    pub fn tool_started(
        request_id: impl Into<String>,
        seq: u32,
        tool_name: impl Into<String>,
        tool_call_id: impl Into<String>,
        awaiting_approval: bool,
    ) -> Self {
        Self {
            tool_name: Some(tool_name.into()),
            tool_call_id: Some(tool_call_id.into()),
            awaiting_approval,
            ..Self::base(request_id, seq, DiagnoseEventKind::ToolStarted)
        }
    }

    /// A `ToolFinished` frame: a dispatched tool call produced its result.
    pub fn tool_finished(
        request_id: impl Into<String>,
        seq: u32,
        tool_call_id: impl Into<String>,
        ok: bool,
    ) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            tool_ok: Some(ok),
            ..Self::base(request_id, seq, DiagnoseEventKind::ToolFinished)
        }
    }

    /// A terminal `Answer` frame carrying the agentic turn's final answer text.
    pub fn answer(request_id: impl Into<String>, seq: u32, answer: impl Into<String>) -> Self {
        Self {
            answer: Some(answer.into()),
            ..Self::base(request_id, seq, DiagnoseEventKind::Answer)
        }
    }

    /// Whether this is a terminal frame: a single-turn `Final`, an agentic
    /// `Answer`, or an `Error` — each ends its request's stream.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            DiagnoseEventKind::Final | DiagnoseEventKind::Answer | DiagnoseEventKind::Error
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
            locale: Some("zh-CN".into()),
            conversation_id: Some("cv-abc123".into()),
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

    /// `conversation_id` round-trips through both JSON and wincode in both its
    /// `None` (fresh single-question) and `Some` (continuation) forms, and a
    /// missing JSON field deserializes back to `None` (so older control ends that
    /// omit it keep working).
    #[test]
    fn conversation_id_round_trips_none_and_some() {
        let config = unbounded_config();
        for conversation_id in [None, Some("cv-xyz_-9".to_string())] {
            let req = DiagnoseRequestData {
                question: "follow up?".into(),
                include_screen: false,
                context_kinds: vec![],
                locale: None,
                conversation_id: conversation_id.clone(),
            };
            let json = serde_json::to_string(&req).expect("json encode");
            let back: DiagnoseRequestData = serde_json::from_str(&json).expect("json decode");
            assert_eq!(req, back);

            let bytes = wincode::config::serialize(&req, config).expect("wincode encode");
            let back2: DiagnoseRequestData =
                wincode::config::deserialize(&bytes, config).expect("wincode decode");
            assert_eq!(req, back2);
        }

        // A request body that omits `conversation_id` entirely decodes to `None`.
        let legacy: DiagnoseRequestData =
            serde_json::from_str(r#"{"question":"q"}"#).expect("legacy decode");
        assert_eq!(legacy.conversation_id, None);
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
                    error_code: None,
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
    fn terminal_frames_are_final_answer_and_error() {
        assert!(!DiagnoseEvent::status("r", 0, "x").is_terminal());
        assert!(!DiagnoseEvent::partial("r", 1, "y").is_terminal());
        assert!(!DiagnoseEvent::turn_started("r", 0, "turn-1").is_terminal());
        assert!(!DiagnoseEvent::tool_started("r", 1, "sysinfo", "c1", false).is_terminal());
        assert!(!DiagnoseEvent::tool_finished("r", 2, "c1", true).is_terminal());
        assert!(DiagnoseEvent::answer("r", 3, "all good").is_terminal());
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
                    error_code: None,
                },
            )
            .is_terminal()
        );
    }

    /// The agentic tool/turn frames round-trip through both JSON and wincode, and
    /// a mutating tool start carries the awaiting-approval flag.
    #[test]
    fn agentic_event_frames_round_trip() {
        let config = unbounded_config();
        let frames = [
            DiagnoseEvent::turn_started("req_1", 0, "turn-1"),
            DiagnoseEvent::tool_started("req_1", 1, "read_system_info", "c1", false),
            DiagnoseEvent::tool_started("req_1", 2, "exec_command", "c2", true),
            DiagnoseEvent::tool_finished("req_1", 3, "c1", true),
            DiagnoseEvent::tool_finished("req_1", 4, "c2", false),
            DiagnoseEvent::answer("req_1", 5, "the host is healthy"),
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

        // The mutating tool start is flagged awaiting approval; a read start is not.
        assert!(DiagnoseEvent::tool_started("r", 0, "exec_command", "c", true).awaiting_approval);
        let read = DiagnoseEvent::tool_started("r", 0, "sysinfo", "c", false);
        assert!(!read.awaiting_approval);
        // The default-false flag is omitted from JSON.
        let json = serde_json::to_string(&read).unwrap();
        assert!(!json.contains("awaiting_approval"));
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

    #[test]
    fn collect_request_round_trips() {
        let req = CollectRequest {
            request_id: "req_1".into(),
            request: DiagnoseRequestData {
                question: "why slow?".into(),
                include_screen: true,
                context_kinds: vec!["system.info".into()],
                locale: Some("zh-CN".into()),
                conversation_id: None,
            },
        };
        let json = serde_json::to_string(&req).expect("encode");
        let back: CollectRequest = serde_json::from_str(&json).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn collect_response_chunk_omits_sha_when_absent() {
        let chunk = CollectResponseChunk {
            request_id: "req_1".into(),
            seq: 0,
            last: false,
            total_len: 10,
            payload_b64: "AAAA".into(),
            sha256: None,
        };
        let json = serde_json::to_string(&chunk).expect("encode");
        assert!(!json.contains("sha256"));
        let resp = CollectResponse::Chunk(chunk);
        let json = serde_json::to_string(&resp).expect("encode");
        let back: CollectResponse = serde_json::from_str(&json).expect("decode");
        assert_eq!(resp, back);
        assert_eq!(resp.request_id(), "req_1");
    }

    /// A maximally-sized chunk, once serialized as a `CollectResponse` and
    /// wrapped with the few extra bytes a `SignalingModel` adds, must stay below
    /// the signaling WebSocket's continuation cap. A chunk budget equal to (or
    /// above) the cap let the JSON envelope push the frame over it, so the
    /// receiver aborted the host link with "payload reached size limit" and the
    /// pending diagnosis failed with "target host disconnected before evidence
    /// was collected". Pin the headroom so the budget can never regress to the
    /// cap again.
    #[test]
    fn max_chunk_frame_stays_under_signaling_cap() {
        let chunk = CollectResponseChunk {
            request_id: "00000000-0000-0000-0000-000000000000".into(),
            seq: u32::MAX,
            last: true,
            total_len: u64::MAX,
            // A real chunk caps payload_b64 at COLLECT_CHUNK_PAYLOAD_LIMIT chars.
            payload_b64: "A".repeat(COLLECT_CHUNK_PAYLOAD_LIMIT),
            sha256: Some("f".repeat(64)),
        };
        let resp = CollectResponse::Chunk(chunk);
        let json = serde_json::to_string(&resp).expect("encode");
        // Allow generous slack for the SignalingModel wrapper layered on top.
        assert!(
            json.len() + 4096 < SIGNALING_FRAME_LIMIT,
            "serialized chunk frame ({} bytes) too close to the {}-byte cap",
            json.len(),
            SIGNALING_FRAME_LIMIT
        );
    }

    #[test]
    fn collect_response_error_round_trips_and_correlates() {
        let resp = CollectResponse::Error(CollectResponseError {
            request_id: "req_2".into(),
            error_kind: AgentErrorKind::RedactionFailed,
            reason: "redaction failed".into(),
        });
        let json = serde_json::to_string(&resp).expect("encode");
        let back: CollectResponse = serde_json::from_str(&json).expect("decode");
        assert_eq!(resp, back);
        assert_eq!(resp.request_id(), "req_2");
        match back {
            CollectResponse::Error(e) => assert_eq!(e.error_kind, AgentErrorKind::RedactionFailed),
            _ => panic!("expected error variant"),
        }
    }
}
