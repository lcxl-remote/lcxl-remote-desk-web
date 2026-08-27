//! Agent capability and sealed execution IPC payloads.

use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[cfg(doc)]
use super::{ServiceToWorker, WorkerToService};
// ================= AI agent IPC payloads =================

/// Payload for [`ServiceToWorker::InvokeAgentCapability`]. Embeds the full
/// [`desk_agent_protocol::AgentEnvelope`] (already server-stamped) so the
/// IPC layer does not re-spell any of its fields — `desk-agent-protocol`
/// derives the same `wincode` schema this transport uses. `connection_id`
/// is `Option` for the same reason as the manager-plane payloads: an
/// orchestrator-initiated call may carry no originating control-end
/// connection and is correlated by `request_id` alone.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct AgentRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub envelope: desk_agent_protocol::ReadonlyAgentEnvelope,
}

/// A sealed Computer Use mutation. It is deliberately separate from
/// [`AgentRequestPayload`], whose envelope can only represent read operations.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ComputerActionPlanPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub plan: desk_agent_protocol::computer_use::SealedComputerActionPlan,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ComputerActionCancelPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub cancel: desk_agent_protocol::computer_use::ComputerActionCancel,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ComputerActionStateQueryPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub query: desk_agent_protocol::computer_use::ComputerActionStateQuery,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ComputerActionStartedPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub started: desk_agent_protocol::computer_use::ComputerActionStarted,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ComputerActionCompletedPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub completed: desk_agent_protocol::computer_use::ComputerActionCompleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ComputerActionStateReportedPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub state: desk_agent_protocol::computer_use::ComputerActionStateReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ComputerUseReadinessPayload {
    pub readiness: desk_agent_protocol::computer_use::ComputerUseReadiness,
}

/// Payload for [`WorkerToService::AgentCapabilityCompleted`]. Reuses
/// [`desk_agent_protocol::AgentOutcome`] verbatim — the same shape the
/// daemon then ships to the control end as `AgentCapabilityCompleted`
/// signaling_data, so there is no daemon-side re-mapping. Mirrors the
/// `VirtualDisplayModeOutcome` Applied/Failed precedent.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct AgentResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub outcome: desk_agent_protocol::AgentOutcome,
}

/// Payload for [`ServiceToWorker::ExecPlan`]. Carries the sealed
/// [`desk_agent_protocol::exec::ExecPlan`] plus the signaling correlation
/// (`request_id`) and originating control-end `connection_id`, which the worker
/// echoes back in [`ExecResultIpcPayload`] so the daemon can route the outbound
/// `ExecutionCompleted` without keeping its own in-flight map.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecPlanPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub plan: desk_agent_protocol::exec::ExecPlan,
    /// Originating ConfirmExec frame `request_id` (the manager's authorization
    /// ledger key). The worker echoes it back in [`ExecResultIpcPayload`] so the
    /// `command_completed` audit event can be attributed to the real operator.
    /// `None` on the single-machine / non-manager path.
    pub audit_source_request_id: Option<String>,
}

/// Payload for [`ServiceToWorker::ExecCancel`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecCancelPayload {
    /// The one dispatch to stop. Keyed on the generation rather than the task so
    /// a cancel aimed at an earlier attempt can never kill its retry.
    pub execution_generation: String,
}

/// Payload for [`WorkerToService::ExecutionCompleted`]. Embeds the
/// [`desk_agent_protocol::exec::ExecResultPayload`] (tagged with
/// `exec_request_id`) the daemon ships to the control end verbatim, plus the
/// echoed `request_id` / `connection_id` for routing.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecResultIpcPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub result: desk_agent_protocol::exec::ExecResultPayload,
    /// Echoed `audit_source_request_id` from the [`ExecPlanPayload`] so the
    /// daemon can attribute the `command_completed` audit event to the
    /// originating ConfirmExec frame (the manager's ledger key). `None` on the
    /// single-machine / non-manager path.
    pub audit_source_request_id: Option<String>,
}

/// Payload for [`WorkerToService::ExecSpawnReport`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecSpawnReportPayload {
    /// The dispatch this reports on: the `request_id` the plan was sent under,
    /// which is also the execution generation the daemon's ledger keys on.
    pub request_id: String,
    /// Echoed from the [`ExecPlanPayload`] so the daemon can route the outbound
    /// lifecycle frame to whoever asked for the execution, exactly as it routes
    /// the result.
    pub connection_id: Option<String>,
    pub report: ExecSpawnReport,
}

/// Payload for [`WorkerToService::ExecHeartbeat`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecHeartbeatPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    /// Milliseconds since the worker began this execution. The worker's own
    /// elapsed time, not a wall clock, so nothing downstream has to reconcile two
    /// machines' clocks to decide whether progress is being made.
    pub running_ms: u64,
}

/// What became of a spawn attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub enum ExecSpawnReport {
    /// The process is running and contained.
    Started {
        /// How to find and reclaim its process tree — a job name on Windows, a
        /// process group on Unix. `None` if the platform could not name the
        /// container even after the spawn.
        containment_identity: Option<String>,
    },
    /// The command never started, so it provably did not run. Worth distinguishing
    /// from an unknown outcome: a caller may safely retry this one.
    Failed {
        /// Operator-facing reason (missing program, containment refused, …).
        reason: String,
    },
}
