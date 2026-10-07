//! Finite delegated-task state independent of session turn state.

use serde::{Deserialize, Serialize};

use super::{
    DelegatedTaskBinding, SubAgentState, SubAgentWaitReason, TaskAssessment, TaskFinalReport,
    valid_id,
};
use crate::session::PersistedAgentSession;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningFence {
    pub input_revision: u64,
    pub control_revision: u64,
    pub source_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskDependency {
    Approval { permission_request_id: String },
    DirectoryApproval { directory_request_id: String },
    Work { work_id: String },
    Resource { reason: SubAgentWaitReason },
}

impl TaskDependency {
    fn validate(&self) -> bool {
        match self {
            Self::Approval {
                permission_request_id,
            } => valid_id(permission_request_id),
            Self::DirectoryApproval {
                directory_request_id,
            } => valid_id(directory_request_id),
            Self::Work { work_id } => valid_id(work_id),
            Self::Resource { reason } => matches!(
                reason,
                SubAgentWaitReason::ModelCapacity
                    | SubAgentWaitReason::WriterCapacity
                    | SubAgentWaitReason::DeviceUnavailable
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentRun {
    pub child_conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub name: String,
    pub binding: DelegatedTaskBinding,
    pub state: SubAgentState,
    pub state_revision: u64,
    pub source_paused: bool,
    pub dependencies: Vec<TaskDependency>,
    pub partial_report: Option<TaskFinalReport>,
    pub terminal_report: Option<TaskFinalReport>,
    pub failure_reason: Option<String>,
    // Historical audit count; text answers never consume a report-repair call.
    pub report_corrections_used: u8,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredReceipt {
    pub receipt_id: String,
    pub succeeded: bool,
    pub verification_complete: bool,
}

/// Loaded by a store from current action ledgers, never from model arguments.
#[derive(Debug, Clone, Default)]
pub struct CompletionFacts {
    pub accepted_receipt_ids: Vec<String>,
    pub available_evidence_ids: Vec<String>,
    pub required_receipts: Vec<RequiredReceipt>,
    /// A dispatched action lacks a validated result or recovery proof. A model
    /// assessment and a manually disposed unknown outcome cannot fill this gap.
    pub incomplete_action_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionDisposition {
    Complete,
    Unable,
    Waiting,
}

impl SubAgentRun {
    pub fn summary(
        &self,
    ) -> desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentSummary {
        desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentSummary {
            task_id: self.binding.task_id.clone(), group_id: self.binding.group_id.clone(),
            child_session_id: self.child_conversation_id.clone(),
            name: self.name.clone(), state: self.state, wait_reason: self.wait_reason(),
            input_revision: self.binding.input_revision, control_revision: self.binding.control_revision,
            state_revision: self.state_revision, source_goal_id: self.binding.source.goal_id().map(str::to_owned),
            source: match &self.binding.source {
                super::role::DelegationSource::UserInput { input_revision } => desk_agent_protocol::ai_assistant::subagent::SubAgentSource::UserInput { input_revision: *input_revision },
                super::role::DelegationSource::Goal { goal_id } => desk_agent_protocol::ai_assistant::subagent::SubAgentSource::Goal { goal_id: goal_id.clone() },
                super::role::DelegationSource::ScheduledOccurrence { schedule_id, occurrence_id } => desk_agent_protocol::ai_assistant::subagent::SubAgentSource::ScheduledOccurrence { schedule_id: schedule_id.clone(), occurrence_id: occurrence_id.clone() },
            },
            created_at: self.created_at.clone(), updated_at: self.updated_at.clone(),
        }
    }

    pub fn result(&self) -> desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentResult {
        desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentResult {
            task: self.summary(),
            report: self
                .terminal_report
                .clone()
                .or_else(|| self.partial_report.clone()),
            objective: self.binding.objective.clone(),
            acceptance_criteria: self.binding.acceptance_criteria.clone(),
            failure_reason: self.failure_reason.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.binding.validate(&self.child_conversation_id)?;
        if !valid_id(&self.child_conversation_id)
            || self.actor_id.is_empty()
            || self.device_id.is_empty()
            || self.name.trim().is_empty()
            || self.name.len() > super::MAX_SUBAGENT_NAME_BYTES
            || self.state_revision == 0
            || self.report_corrections_used > 1
            || self.dependencies.len() > 64
            || self
                .dependencies
                .iter()
                .any(|dependency| !dependency.validate())
            || (self.state.is_terminal() && !self.dependencies.is_empty())
            || (self.state == SubAgentState::Completed
                && self
                    .terminal_report
                    .as_ref()
                    .is_none_or(|report| report.assessment != TaskAssessment::Complete))
            || (!self.state.is_terminal() && self.terminal_report.is_some())
            || (self.state == SubAgentState::WaitingSource && !self.source_paused)
            || (self.source_paused
                && !self.state.is_terminal()
                && !matches!(
                    self.state,
                    SubAgentState::WaitingSource | SubAgentState::Cancelling
                ))
        {
            return Err("invalid delegated run state");
        }
        for report in [&self.partial_report, &self.terminal_report]
            .into_iter()
            .flatten()
        {
            report.validate()?;
        }
        Ok(())
    }

    pub fn fence(&self) -> PlanningFence {
        PlanningFence {
            input_revision: self.binding.input_revision,
            control_revision: self.binding.control_revision,
            source_epoch: self.binding.source_epoch,
        }
    }

    pub fn require_current(&self, fence: PlanningFence) -> Result<(), &'static str> {
        if fence != self.fence() {
            return Err("delegated task input, control or source changed");
        }
        if self.state.is_terminal() || self.state == SubAgentState::Cancelling {
            return Err("delegated task no longer admits planning or actions");
        }
        if self.source_paused {
            return Err("delegation source is paused");
        }
        Ok(())
    }

    pub fn claim_planning(
        &mut self,
        fence: PlanningFence,
        now_ms: i64,
        now: &str,
    ) -> Result<(), &'static str> {
        self.require_current(fence)?;
        if self.state != SubAgentState::Queued
            || !self.dependencies.is_empty()
            || now_ms >= self.binding.deadline_ms
        {
            return Err("delegated task is not ready for a planning claim");
        }
        self.state_revision = self.next_state_revision()?;
        self.state = SubAgentState::Running;
        self.updated_at = now.into();
        Ok(())
    }

    pub fn validate_session(&self, session: &PersistedAgentSession) -> Result<(), &'static str> {
        if session.conversation_id != self.child_conversation_id
            || session.actor_id != self.actor_id
            || session.device_id != self.device_id
            || session.agent_role.binding() != Some(&self.binding)
            || session.input_revision != self.binding.input_revision
            || session.control_revision != self.binding.control_revision
            || session.subagent_report_corrections_used != self.report_corrections_used
        {
            return Err("child session does not match its durable task");
        }
        Ok(())
    }

    pub fn wait_reason(&self) -> Option<SubAgentWaitReason> {
        match self.state {
            SubAgentState::WaitingSource => Some(SubAgentWaitReason::SourcePaused),
            SubAgentState::WaitingApproval => Some(SubAgentWaitReason::OwnerApproval),
            SubAgentState::WaitingWork => Some(SubAgentWaitReason::BackgroundWork),
            SubAgentState::WaitingResource => self.dependencies.iter().find_map(|dependency| {
                if let TaskDependency::Resource { reason } = dependency {
                    Some(*reason)
                } else {
                    None
                }
            }),
            _ => None,
        }
    }

    fn next_state_revision(&self) -> Result<u64, &'static str> {
        self.state_revision
            .checked_add(1)
            .ok_or("delegated task revision exhausted")
    }

    fn waiting_state(&self) -> SubAgentState {
        if self.source_paused {
            SubAgentState::WaitingSource
        } else if self.dependencies.iter().any(|dependency| {
            matches!(
                dependency,
                TaskDependency::Approval { .. } | TaskDependency::DirectoryApproval { .. }
            )
        }) {
            SubAgentState::WaitingApproval
        } else if self
            .dependencies
            .iter()
            .any(|dependency| matches!(dependency, TaskDependency::Work { .. }))
        {
            SubAgentState::WaitingWork
        } else if !self.dependencies.is_empty() {
            SubAgentState::WaitingResource
        } else {
            SubAgentState::Queued
        }
    }

    /// Called in the same source/root transaction that closes group admission.
    /// Dependencies remain intact; old in-flight model output is fenced out.
    pub fn pause_source(&mut self, source_epoch: u64, now: &str) -> Result<bool, &'static str> {
        if self.state.is_terminal() || self.state == SubAgentState::Cancelling {
            return Ok(false);
        }
        if source_epoch <= self.binding.source_epoch {
            return Err("source pause requires a newer epoch");
        }
        let revision = self.next_state_revision()?;
        self.binding.source_epoch = source_epoch;
        self.source_paused = true;
        self.state = SubAgentState::WaitingSource;
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(true)
    }

    /// The caller revalidates current source authority, grants, budget and deadline.
    pub fn resume_source(&mut self, source_epoch: u64, now: &str) -> Result<bool, &'static str> {
        if self.state.is_terminal() || self.state == SubAgentState::Cancelling {
            return Ok(false);
        }
        if !self.source_paused || source_epoch <= self.binding.source_epoch {
            return Err("source resume requires a paused task and newer epoch");
        }
        let revision = self.next_state_revision()?;
        self.binding.source_epoch = source_epoch;
        self.source_paused = false;
        self.state = self.waiting_state();
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(true)
    }

    pub fn set_dependencies(
        &mut self,
        dependencies: Vec<TaskDependency>,
        now: &str,
    ) -> Result<(), &'static str> {
        if self.state.is_terminal() || self.state == SubAgentState::Cancelling {
            return Err("settled or cancelling task cannot resume");
        }
        if dependencies.len() > 64 || dependencies.iter().any(|dependency| !dependency.validate()) {
            return Err("invalid delegated dependencies");
        }
        let revision = self.next_state_revision()?;
        self.dependencies = dependencies;
        self.state = self.waiting_state();
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(())
    }

    /// Durable facts may advance while planning holds its session lease. Empty
    /// dependencies preserve that running claim; they never imply completion.
    pub fn synchronize_dependencies(
        &mut self,
        dependencies: Vec<TaskDependency>,
        planning_active: bool,
        now: &str,
    ) -> Result<bool, &'static str> {
        if self.state.is_terminal() || self.state == SubAgentState::Cancelling {
            return Err("settled or cancelling task cannot accept planning state");
        }
        if dependencies.len() > 64 || dependencies.iter().any(|dependency| !dependency.validate()) {
            return Err("invalid delegated dependencies");
        }
        let mut next = self.clone();
        next.dependencies = dependencies;
        next.state = if planning_active && next.dependencies.is_empty() && !next.source_paused {
            SubAgentState::Running
        } else {
            next.waiting_state()
        };
        if next.dependencies == self.dependencies && next.state == self.state {
            return Ok(false);
        }
        next.state_revision = self.next_state_revision()?;
        next.updated_at = now.into();
        *self = next;
        Ok(true)
    }

    pub fn request_cancel(
        &mut self,
        expected: PlanningFence,
        now: &str,
    ) -> Result<bool, &'static str> {
        if expected != self.fence() {
            return Err("delegated cancellation was superseded");
        }
        if self.state.is_terminal() || self.state == SubAgentState::Cancelling {
            return Ok(false);
        }
        let revision = self.next_state_revision()?;
        let control = self
            .binding
            .control_revision
            .checked_add(1)
            .ok_or("delegated control exhausted")?;
        self.binding.control_revision = control;
        self.state = SubAgentState::Cancelling;
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(true)
    }

    /// Settles durable cancellation after permission/dispatch admission is closed.
    /// Unknown OS exit does not keep a cancelled delegated task alive indefinitely.
    pub fn settle_cancel(&mut self, now: &str) -> Result<(), &'static str> {
        if self.state != SubAgentState::Cancelling {
            return Err("delegated cancellation has not been requested");
        }
        let revision = self.next_state_revision()?;
        self.state = SubAgentState::Cancelled;
        self.dependencies.clear();
        self.terminal_report = self.partial_report.clone();
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(())
    }

    /// Explicit adjustment keeps source, deadline and spent/reserved budget intact.
    pub fn adjust(
        &mut self,
        expected: PlanningFence,
        objective: String,
        criteria: Vec<String>,
        now: &str,
    ) -> Result<(), &'static str> {
        if expected != self.fence()
            || self.state.is_terminal()
            || self.state == SubAgentState::Cancelling
        {
            return Err("delegated task cannot be adjusted");
        }
        super::role::validate_task(&objective, &criteria)?;
        let input = self
            .binding
            .input_revision
            .checked_add(1)
            .ok_or("delegated input exhausted")?;
        let control = self
            .binding
            .control_revision
            .checked_add(1)
            .ok_or("delegated control exhausted")?;
        let revision = self.next_state_revision()?;
        self.binding.objective = objective;
        self.binding.acceptance_criteria = criteria;
        self.binding.input_revision = input;
        self.binding.control_revision = control;
        self.partial_report = None;
        self.state = self.waiting_state();
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(())
    }

    pub fn evaluate_report(
        &self,
        expected: PlanningFence,
        report: &TaskFinalReport,
        facts: &CompletionFacts,
    ) -> Result<CompletionDisposition, &'static str> {
        self.require_current(expected)?;
        report.validate()?;
        facts.validate()?;
        if report
            .receipt_refs
            .iter()
            .any(|id| !facts.accepted_receipt_ids.contains(id))
            || report
                .evidence_refs
                .iter()
                .any(|id| !facts.available_evidence_ids.contains(id))
        {
            return Err("delegated report cites unavailable facts");
        }
        if !self.dependencies.is_empty() {
            return Ok(CompletionDisposition::Waiting);
        }
        match report.assessment {
            TaskAssessment::Complete => {
                if !facts.incomplete_action_ids.is_empty()
                    || facts.required_receipts.iter().any(|receipt| {
                        !receipt.succeeded
                            || !receipt.verification_complete
                            || !report.receipt_refs.contains(&receipt.receipt_id)
                    })
                {
                    return Err("delegated report has unfinished or unverified actions");
                }
                Ok(CompletionDisposition::Complete)
            }
            TaskAssessment::Unable => Ok(CompletionDisposition::Unable),
            TaskAssessment::Pending => Err("pending assessment has no durable dependency"),
        }
    }

    pub fn settle_report(
        &mut self,
        expected: PlanningFence,
        report: TaskFinalReport,
        facts: &CompletionFacts,
        now: &str,
    ) -> Result<CompletionDisposition, &'static str> {
        let disposition = self.evaluate_report(expected, &report, facts)?;
        let revision = self.next_state_revision()?;
        match disposition {
            CompletionDisposition::Waiting => {
                self.partial_report = Some(report);
                self.state = self.waiting_state();
            }
            CompletionDisposition::Complete | CompletionDisposition::Unable => {
                self.state = if disposition == CompletionDisposition::Complete {
                    SubAgentState::Completed
                } else {
                    SubAgentState::Failed
                };
                self.failure_reason = if disposition == CompletionDisposition::Unable {
                    report.reason.clone()
                } else {
                    None
                };
                self.terminal_report = Some(report);
                self.dependencies.clear();
            }
        }
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(disposition)
    }

    pub fn fail(&mut self, reason: &str, now: &str) -> Result<bool, &'static str> {
        if self.state.is_terminal() || self.state == SubAgentState::Cancelling {
            return Ok(false);
        }
        if reason.trim().is_empty() || reason.len() > 2048 {
            return Err("invalid delegated failure reason");
        }
        let revision = self.next_state_revision()?;
        self.state = SubAgentState::Failed;
        self.failure_reason = Some(reason.into());
        self.terminal_report = self.partial_report.clone();
        self.dependencies.clear();
        self.state_revision = revision;
        self.updated_at = now.into();
        Ok(true)
    }
}
