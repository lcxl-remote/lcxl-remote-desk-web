//! Neutral streaming event contract for Device Assistant agent turns.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::AgentError;
use crate::content_safety::StreamRetractionReason;
use crate::provenance::AiProvenance;

/// Kind of a streamed [`AgentEvent`] frame. `Answer`, `Error`, and `Retracted` are terminal.
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
pub enum AgentEventKind {
    /// A lifecycle status update (collecting / redacting / modeling / ...).
    Status,
    /// An incremental summary / answer token from the streaming model.
    Partial,
    /// Marks all provisional text since the previous commit as reviewed.
    /// Carries no text and is not terminal.
    PartialCommitted,
    /// Terminal: provisional text is atomically removed and replaced by a fixed,
    /// locally selected policy/unavailable/incomplete message.
    Retracted,
    /// Terminal: the agent turn failed.
    Error,
    /// An agentic turn has started (carries `turn_id`).
    TurnStarted,
    /// A tool call was dispatched (read tool) or is awaiting approval (mutating
    /// tool); carries `tool_name` + `tool_call_id` + `tool_arguments_json` (and
    /// `awaiting_approval`).
    ToolStarted,
    /// A dispatched tool call produced its result; carries `tool_call_id` +
    /// `tool_ok` + `tool_output`, plus `background_task_id` when execution
    /// continues in the background.
    ToolFinished,
    /// Terminal for the current planning turn: a normalized permission batch was
    /// durably recorded and is waiting for a separate trusted user decision.
    PermissionRequired,
    /// Terminal: the agentic turn committed a final natural-language answer.
    Answer,
}

/// One streamed frame of a Device Assistant turn (server → control end).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct AgentEvent {
    /// Correlates back to the originating Device Assistant request.
    pub request_id: String,
    /// Monotonic per-stream sequence number.
    pub seq: u32,
    pub kind: AgentEventKind,
    /// `kind = Status`: the lifecycle phase name.
    #[serde(default)]
    pub status: Option<String>,
    /// `kind = Partial`: an incremental summary fragment.
    #[serde(default)]
    pub partial_summary: Option<String>,
    /// `kind = Error`: the failure (uses `AgentError` so `safe_for_model` /
    /// `retryable` carry through to the UI).
    #[serde(default)]
    pub error: Option<AgentError>,
    /// `kind = Retracted`, or a policy `Error` before/after provisional text:
    /// closed reason used to select the client-side safety guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retraction_reason: Option<StreamRetractionReason>,
    /// `kind = TurnStarted`: the id of the agentic turn that started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// `kind = Status`, `status = context_compacted`: committed checkpoint
    /// generation. Older control ends safely ignore this optional metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_generation: Option<u32>,
    /// `kind = Status`, `status = context_compacted`: number of original
    /// messages covered by the committed checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_message_count: Option<u32>,
    /// `kind = ToolStarted`: the model-facing name of the tool being run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// `kind = ToolStarted`: the raw JSON arguments produced by the model.
    /// Consumers may pretty-print valid JSON but must treat it as untrusted text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_arguments_json: Option<String>,
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
    /// `kind = ToolFinished`: the same redacted, bounded text returned to the
    /// model. It is Provider output, not trusted markup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<String>,
    /// `kind = ToolFinished`: the server-issued execution request id when this
    /// result reports a command continuing as a background task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_task_id: Option<String>,
    /// `kind = PermissionRequired`: stable id of the durable request projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_request_id: Option<String>,
    /// `kind = PermissionRequired`: bounded number of independently decidable
    /// items. Full request details come from the durable session projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_item_count: Option<u32>,
    /// `kind = Answer`: the agentic turn's final natural-language answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Machine-readable AI marking for answer content frames.
    /// Absent on non-content frames (status / tool / error). Its absence on a
    /// content frame does not mean "not AI" — the frame kind already establishes
    /// that; consumers mark such frames AI regardless (fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<AiProvenance>,
}

impl AgentEvent {
    /// An empty frame of `kind` with all payload fields cleared; the public
    /// constructors set only the field their kind carries.
    fn base(request_id: impl Into<String>, seq: u32, kind: AgentEventKind) -> Self {
        Self {
            request_id: request_id.into(),
            seq,
            kind,
            status: None,
            partial_summary: None,
            error: None,
            retraction_reason: None,
            turn_id: None,
            checkpoint_generation: None,
            covered_message_count: None,
            tool_name: None,
            tool_arguments_json: None,
            tool_call_id: None,
            awaiting_approval: false,
            tool_ok: None,
            tool_output: None,
            background_task_id: None,
            permission_request_id: None,
            permission_item_count: None,
            answer: None,
            provenance: None,
        }
    }

    /// Attach machine-readable AI provenance to an answer content frame. Emitters
    /// call this on the content frames they build; frames
    /// without it are still treated as AI by their kind (fail-closed).
    pub fn with_provenance(mut self, provenance: AiProvenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    /// A `Status` frame announcing a lifecycle phase.
    pub fn status(request_id: impl Into<String>, seq: u32, phase: impl Into<String>) -> Self {
        Self {
            status: Some(phase.into()),
            ..Self::base(request_id, seq, AgentEventKind::Status)
        }
    }

    /// A lifecycle status associated with one agent turn.
    pub fn status_for_turn(
        request_id: impl Into<String>,
        seq: u32,
        phase: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        let mut event = Self::status(request_id, seq, phase);
        event.turn_id = Some(turn_id.into());
        event
    }

    /// A context-compaction status carrying only non-sensitive checkpoint
    /// metadata; the summary and provider details never enter the stream.
    pub fn context_compacted(
        request_id: impl Into<String>,
        seq: u32,
        turn_id: impl Into<String>,
        generation: u32,
        covered_message_count: u32,
    ) -> Self {
        let mut event = Self::status_for_turn(request_id, seq, "context_compacted", turn_id);
        event.checkpoint_generation = Some(generation);
        event.covered_message_count = Some(covered_message_count);
        event
    }

    /// A `Partial` frame carrying a streaming summary / answer fragment.
    pub fn partial(request_id: impl Into<String>, seq: u32, fragment: impl Into<String>) -> Self {
        Self {
            partial_summary: Some(fragment.into()),
            ..Self::base(request_id, seq, AgentEventKind::Partial)
        }
    }
    /// A non-terminal marker committing all provisional text emitted since the
    /// previous commit marker.
    pub fn partial_committed(request_id: impl Into<String>, seq: u32) -> Self {
        Self::base(request_id, seq, AgentEventKind::PartialCommitted)
    }

    /// A terminal retraction. It never carries the provisional text, provider
    /// rationale, category, or threshold.
    pub fn retracted(
        request_id: impl Into<String>,
        seq: u32,
        reason: StreamRetractionReason,
        error: Option<AgentError>,
    ) -> Self {
        Self {
            retraction_reason: Some(reason),
            error,
            ..Self::base(request_id, seq, AgentEventKind::Retracted)
        }
    }

    /// A terminal `Error` frame.
    pub fn error(request_id: impl Into<String>, seq: u32, error: AgentError) -> Self {
        Self {
            error: Some(error),
            ..Self::base(request_id, seq, AgentEventKind::Error)
        }
    }

    /// A terminal policy `Error` when no provisional text needs retracting. The
    /// reason remains machine-readable so `SafeRedirect` is not collapsed into a
    /// generic blocked message.
    pub fn error_with_retraction_reason(
        request_id: impl Into<String>,
        seq: u32,
        error: AgentError,
        reason: StreamRetractionReason,
    ) -> Self {
        Self {
            error: Some(error),
            retraction_reason: Some(reason),
            ..Self::base(request_id, seq, AgentEventKind::Error)
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
            ..Self::base(request_id, seq, AgentEventKind::TurnStarted)
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
        arguments_json: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: Some(tool_name.into()),
            tool_call_id: Some(tool_call_id.into()),
            awaiting_approval,
            tool_arguments_json: Some(arguments_json.into()),
            ..Self::base(request_id, seq, AgentEventKind::ToolStarted)
        }
    }

    /// A `ToolFinished` frame: a dispatched tool call produced its result.
    pub fn tool_finished(
        request_id: impl Into<String>,
        seq: u32,
        tool_call_id: impl Into<String>,
        ok: bool,
        output: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            tool_ok: Some(ok),
            tool_output: Some(output.into()),
            ..Self::base(request_id, seq, AgentEventKind::ToolFinished)
        }
    }

    /// Attach the stable background-task correlation id to a `ToolFinished`
    /// dispatch receipt.
    pub fn with_background_task_id(mut self, background_task_id: impl Into<String>) -> Self {
        self.background_task_id = Some(background_task_id.into());
        self
    }

    pub fn permission_required(
        request_id: impl Into<String>,
        seq: u32,
        permission_request_id: impl Into<String>,
        item_count: u32,
    ) -> Self {
        Self {
            permission_request_id: Some(permission_request_id.into()),
            permission_item_count: Some(item_count),
            ..Self::base(request_id, seq, AgentEventKind::PermissionRequired)
        }
    }

    /// A terminal `Answer` frame carrying the agentic turn's final answer text.
    pub fn answer(request_id: impl Into<String>, seq: u32, answer: impl Into<String>) -> Self {
        Self {
            answer: Some(answer.into()),
            ..Self::base(request_id, seq, AgentEventKind::Answer)
        }
    }

    /// Whether this is a terminal frame. `PartialCommitted` is deliberately
    /// non-terminal; `Retracted` participates in the exactly-one terminal latch.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            AgentEventKind::Answer
                | AgentEventKind::Error
                | AgentEventKind::Retracted
                | AgentEventKind::PermissionRequired
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_kinds_are_closed() {
        assert!(!AgentEvent::status("r", 0, "running").is_terminal());
        assert!(AgentEvent::answer("r", 1, "done").is_terminal());
    }
}
