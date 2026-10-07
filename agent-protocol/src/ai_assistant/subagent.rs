//! Owner-visible delegation state. These values never carry execution authority.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const MAX_SUBAGENT_PAGE_SIZE: u32 = 20;
pub const MAX_SUBAGENT_REPORT_BYTES: usize = 32 * 1024;
pub const MAX_SUBAGENT_REPORT_ITEMS: usize = 32;
pub const MAX_SUBAGENT_REPORT_ITEM_BYTES: usize = 2 * 1024;
pub const MAX_SUBAGENT_REPORT_REFERENCES: usize = 200;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentState {
    Queued,
    Running,
    WaitingApproval,
    WaitingWork,
    WaitingResource,
    WaitingSource,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl SubAgentState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::WaitingWork => "waiting_work",
            Self::WaitingResource => "waiting_resource",
            Self::WaitingSource => "waiting_source",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentWaitReason {
    OwnerApproval,
    BackgroundWork,
    ModelCapacity,
    WriterCapacity,
    SourcePaused,
    DeviceUnavailable,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssessment {
    Complete,
    Unable,
    Pending,
}

/// Runtime-generated result metadata. Complete means the run ended, not business success.
/// Authority-controlled identifiers, revisions and status are deliberately absent.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskFinalReport {
    pub assessment: TaskAssessment,
    pub summary: String,
    pub findings: Vec<String>,
    pub delivered: Vec<String>,
    pub remaining: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub receipt_refs: Vec<String>,
    pub reason: Option<String>,
}

impl TaskFinalReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.summary.trim().is_empty() || self.summary.len() > MAX_SUBAGENT_REPORT_BYTES {
            return Err("invalid task report summary");
        }
        for values in [&self.findings, &self.delivered, &self.remaining] {
            if values.len() > MAX_SUBAGENT_REPORT_ITEMS
                || values.iter().any(|value| {
                    value.trim().is_empty() || value.len() > MAX_SUBAGENT_REPORT_ITEM_BYTES
                })
            {
                return Err("invalid task report items");
            }
        }
        for refs in [&self.evidence_refs, &self.receipt_refs] {
            if refs.len() > MAX_SUBAGENT_REPORT_REFERENCES
                || refs.iter().any(|value| {
                    value.is_empty()
                        || value.len() > 256
                        || !value.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'-' | b'_' | b':' | b'.')
                        })
                })
            {
                return Err("invalid task report references");
            }
            let unique: std::collections::BTreeSet<_> = refs.iter().collect();
            if unique.len() != refs.len() {
                return Err("duplicate task report references");
            }
        }
        if self.reason.as_ref().is_some_and(|reason| {
            reason.trim().is_empty() || reason.len() > MAX_SUBAGENT_REPORT_ITEM_BYTES
        }) {
            return Err("invalid task report reason");
        }
        match self.assessment {
            TaskAssessment::Complete if !self.remaining.is_empty() => {
                return Err("completed assessment contains remaining work");
            }
            TaskAssessment::Unable | TaskAssessment::Pending if self.reason.is_none() => {
                return Err("unfinished assessment requires a reason");
            }
            _ => {}
        }
        if serde_json::to_vec(self)
            .map_err(|_| "invalid task report encoding")?
            .len()
            > MAX_SUBAGENT_REPORT_BYTES
        {
            return Err("task report exceeds the result limit");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubAgentSource {
    UserInput {
        input_revision: u64,
    },
    Goal {
        goal_id: String,
    },
    ScheduledOccurrence {
        schedule_id: String,
        occurrence_id: String,
    },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSubAgentSummary {
    pub task_id: String,
    /// Recovery selector for this child; every read and decision rechecks ownership.
    pub child_session_id: String,
    pub group_id: String,
    pub name: String,
    pub state: SubAgentState,
    pub wait_reason: Option<SubAgentWaitReason>,
    pub input_revision: u64,
    pub control_revision: u64,
    pub state_revision: u64,
    pub source_goal_id: Option<String>,
    pub source: SubAgentSource,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSubAgentResult {
    pub task: AiAssistantSubAgentSummary,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub report: Option<TaskFinalReport>,
    pub failure_reason: Option<String>,
}

/// Durable attention/progress notification. Reading it in the UI does not mark
/// a result as observed or interpreted by the main model.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSubAgentEvent {
    pub event_id: String,
    pub task: AiAssistantSubAgentSummary,
    pub parent_input_revision: u64,
    pub parent_control_revision: u64,
    pub created_at: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSubAgentPage {
    pub items: Vec<AiAssistantSubAgentSummary>,
    pub total: u64,
    pub unfinished: u64,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantDelegationSnapshot {
    pub parent_session_id: Option<String>,
    pub task: Option<AiAssistantSubAgentSummary>,
    pub tasks: Option<AiAssistantSubAgentPage>,
    /// Include every unfinished child, even when it is outside the recent page.
    pub active_tasks: Vec<AiAssistantSubAgentSummary>,
    /// Bounded oldest unread tasks needing owner attention, including old groups.
    pub attention_tasks: Vec<AiAssistantSubAgentSummary>,
    pub attention_count: u64,
}

/// Owner controls use input/control fencing; progress revisions are observational.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSubAgentControl {
    pub client_request_id: String,
    pub task_id: String,
    pub expected_input_revision: u64,
    pub expected_control_revision: u64,
    pub action: SubAgentControlAction,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubAgentControlAction {
    Cancel,
    Adjust { message: String },
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentStopChoice {
    IncludeSubAgents,
    MainOnly,
}

/// Absence of a choice is valid only if the transaction finds no unfinished child.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantStopControl {
    pub client_request_id: String,
    pub expected_input_revision: u64,
    pub expected_control_revision: u64,
    pub subagent_choice: Option<SubAgentStopChoice>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantStopResult {
    pub input_revision: u64,
    pub control_revision: u64,
    pub stopped_subagents: Vec<AiAssistantSubAgentSummary>,
}
