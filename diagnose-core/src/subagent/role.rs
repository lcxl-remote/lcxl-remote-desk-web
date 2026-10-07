//! Server-owned role and immutable source binding, outside compressed history.

use serde::{Deserialize, Serialize};

use super::{
    MAX_ACCEPTANCE_CRITERIA, MAX_ACCEPTANCE_CRITERION_BYTES, MAX_DELEGATED_TASK_BYTES, valid_id,
};
use crate::registry::ToolEffect;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationSource {
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

impl DelegationSource {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::UserInput { input_revision } if *input_revision > 0 => Ok(()),
            Self::Goal { goal_id } if valid_id(goal_id) => Ok(()),
            Self::ScheduledOccurrence {
                schedule_id,
                occurrence_id,
            } if valid_id(schedule_id) && valid_id(occurrence_id) => Ok(()),
            _ => Err("invalid delegation source"),
        }
    }

    pub fn allows_delegated_review(&self) -> bool {
        matches!(self, Self::UserInput { .. } | Self::Goal { .. })
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
        self.source.validate()
    }
}

pub fn validate_task(objective: &str, criteria: &[String]) -> Result<(), &'static str> {
    if objective.trim().is_empty() || objective.len() > MAX_DELEGATED_TASK_BYTES {
        return Err("invalid delegated objective");
    }
    if criteria.is_empty()
        || criteria.len() > MAX_ACCEPTANCE_CRITERIA
        || criteria.iter().any(|criterion| {
            criterion.trim().is_empty() || criterion.len() > MAX_ACCEPTANCE_CRITERION_BYTES
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
    SubAgent {
        binding: Box<DelegatedTaskBinding>,
    },
}

impl AgentRole {
    pub fn is_main(&self) -> bool {
        matches!(self, Self::Main)
    }

    pub fn binding(&self) -> Option<&DelegatedTaskBinding> {
        match self {
            Self::Main => None,
            Self::SubAgent { binding } => Some(binding),
        }
    }

    pub fn allows_effect(&self, effect: ToolEffect) -> bool {
        self.is_main()
            || !matches!(
                effect,
                ToolEffect::SchedulePlanning
                    | ToolEffect::GoalOpenPlanning
                    | ToolEffect::GoalControl
                    | ToolEffect::SubAgentPlanning
                    | ToolEffect::SubAgentControl
                    | ToolEffect::SubAgentQuery
                    | ToolEffect::SubAgentWait
            )
    }
}
