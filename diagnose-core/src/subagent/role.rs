//! Server-owned role and immutable source binding, outside compressed history.

use serde::{Deserialize, Serialize};

use super::{
    MAX_ACCEPTANCE_CRITERIA, MAX_ACCEPTANCE_CRITERION_CHARS, MAX_DELEGATED_TASK_CHARS, valid_id,
};
use crate::registry::ToolEffect;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationSource {
    UserInput {
        input_revision: u64,
    },
    WorkbenchOwner {
        workbench_id: String,
        input_revision: u64,
    },
    IssueWork {
        source: Box<crate::issue_authority::IssueWorkSource>,
    },
    Goal {
        goal_id: String,
    },
    ConversationResume {
        schedule_id: String,
        run_id: String,
        input_revision: u64,
    },
    ScheduledOccurrence {
        schedule_id: String,
        occurrence_id: String,
    },
}

impl DelegationSource {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::UserInput { input_revision } if *input_revision > 0 => Ok(()),
            Self::WorkbenchOwner {
                workbench_id,
                input_revision,
            } if valid_id(workbench_id) && *input_revision > 0 => Ok(()),
            Self::Goal { goal_id } if valid_id(goal_id) => Ok(()),
            Self::ScheduledOccurrence {
                schedule_id,
                occurrence_id,
            } if valid_id(schedule_id) && valid_id(occurrence_id) => Ok(()),
            Self::ConversationResume {
                schedule_id,
                run_id,
                input_revision,
            } if valid_id(schedule_id) && valid_id(run_id) && *input_revision > 0 => Ok(()),
            Self::IssueWork { source } => source.validate(),
            _ => Err("invalid delegation source"),
        }
    }

    pub fn uses_request_targets(&self) -> bool {
        self.request_input_revision().is_some() || self.goal_id().is_some()
    }

    pub fn allows_delegated_review(&self) -> bool {
        if let Self::IssueWork { source } = self {
            return source.work_kind == crate::issue_authority::IssueWorkKind::DeviceInvestigation;
        }
        self.request_input_revision().is_some() || self.goal_id().is_some()
    }

    pub fn request_input(session: &crate::session::PersistedAgentSession) -> Self {
        if let Some(source) = session.agent_role.issue_source() {
            return Self::IssueWork {
                source: Box::new(source.clone()),
            };
        }
        let input_revision = session.input_revision;
        match session.agent_role.workbench_id() {
            Some(workbench_id) => Self::WorkbenchOwner {
                workbench_id: workbench_id.into(),
                input_revision,
            },
            None => Self::UserInput { input_revision },
        }
    }

    pub fn request_input_revision(&self) -> Option<u64> {
        match self {
            Self::UserInput { input_revision }
            | Self::WorkbenchOwner { input_revision, .. }
            | Self::ConversationResume { input_revision, .. } => Some(*input_revision),
            Self::IssueWork { .. } => Some(1),
            _ => None,
        }
    }

    pub fn workbench_id(&self) -> Option<&str> {
        match self {
            Self::WorkbenchOwner { workbench_id, .. } => Some(workbench_id),
            Self::IssueWork { source } => Some(&source.workbench_id),
            _ => None,
        }
    }

    pub fn goal_id(&self) -> Option<&str> {
        match self {
            Self::Goal { goal_id } => Some(goal_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedTaskBinding {
    pub root_conversation_id: String,
    pub group_id: String,
    pub task_id: String,
    pub source: DelegationSource,
    pub targets: Vec<crate::dynamic_run::AssistantInputTarget>,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub input_revision: u64,
    pub control_revision: u64,
    pub source_epoch: u64,
    pub deadline_ms: i64,
}

impl DelegatedTaskBinding {
    pub fn validate(&self, child_conversation_id: &str) -> Result<(), &'static str> {
        if !valid_id(&self.root_conversation_id)
            || !valid_id(&self.group_id)
            || !valid_id(&self.task_id)
            || self.root_conversation_id == child_conversation_id
            || self.input_revision == 0
            || self.control_revision == 0
            || self.source_epoch == 0
            || self.deadline_ms <= 0
        {
            return Err("invalid delegated task binding");
        }
        validate_task(&self.objective, &self.acceptance_criteria)?;
        self.source.validate()?;
        crate::dynamic_run::AssistantInputTarget::validate_all(&self.targets)
    }
}

pub fn validate_task(objective: &str, criteria: &[String]) -> Result<(), &'static str> {
    if objective.trim().is_empty() || objective.chars().count() > MAX_DELEGATED_TASK_CHARS {
        return Err("invalid delegated objective");
    }
    if criteria.is_empty()
        || criteria.len() > MAX_ACCEPTANCE_CRITERIA
        || criteria.iter().any(|criterion| {
            criterion.trim().is_empty()
                || criterion.chars().count() > MAX_ACCEPTANCE_CRITERION_CHARS
        })
    {
        return Err("invalid delegated acceptance criteria");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRole {
    #[default]
    Main,
    Coordinator {
        workbench_id: String,
        request_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue_source: Option<Box<crate::issue_authority::IssueWorkSource>>,
    },
    SubAgent {
        binding: Box<DelegatedTaskBinding>,
    },
}

impl AgentRole {
    pub fn issue_source(&self) -> Option<&crate::issue_authority::IssueWorkSource> {
        match self {
            Self::Coordinator { issue_source, .. } => issue_source.as_deref(),
            Self::SubAgent { binding } => match &binding.source {
                DelegationSource::IssueWork { source } => Some(source),
                _ => None,
            },
            Self::Main => None,
        }
    }
    pub fn is_main(&self) -> bool {
        self.binding().is_none()
    }

    pub fn workbench_id(&self) -> Option<&str> {
        match self {
            Self::Coordinator { workbench_id, .. } => Some(workbench_id),
            _ => None,
        }
    }

    pub fn workbench_request_id(&self) -> Option<&str> {
        match self {
            Self::Coordinator { request_id, .. } => request_id.as_deref(),
            _ => None,
        }
    }

    pub fn binding(&self) -> Option<&DelegatedTaskBinding> {
        match self {
            Self::Main | Self::Coordinator { .. } => None,
            Self::SubAgent { binding } => Some(binding),
        }
    }

    pub fn allows_effect(&self, effect: ToolEffect) -> bool {
        if effect == ToolEffect::SubAgentReport {
            return self.binding().is_some();
        }
        if matches!(effect, ToolEffect::Mutating | ToolEffect::ExternalWrite)
            && self.workbench_id().is_some()
        {
            return false;
        }
        self.is_main()
            || !matches!(
                effect,
                ToolEffect::SchedulePlanning
                    | ToolEffect::MemoryQuery
                    | ToolEffect::MemoryWrite
                    | ToolEffect::AssistantHistory
                    | ToolEffect::GoalOpenPlanning
                    | ToolEffect::GoalControl
                    | ToolEffect::SubAgentPlanning
                    | ToolEffect::SubAgentControl
                    | ToolEffect::SubAgentQuery
                    | ToolEffect::SubAgentWait
            )
    }
}
