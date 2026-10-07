//! Source-scoped delegation group and admission independent of main input changes.

use serde::{Deserialize, Serialize};

use super::{
    DelegationSource,
    budget::{BudgetLedger, DelegationLimits},
    valid_id,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAdmission {
    Open,
    Paused,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationGroup {
    pub group_id: String,
    pub root_conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub source: DelegationSource,
    pub source_epoch: u64,
    pub source_admission: SourceAdmission,
    pub parent_input_revision: u64,
    pub parent_control_revision: u64,
    pub parent_active: bool,
    pub limits: DelegationLimits,
    pub budget: BudgetLedger,
    pub tasks_created: u32,
    pub required_task_ids: Vec<String>,
    pub version: u64,
}

impl DelegationGroup {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.source.validate()?;
        self.limits.validate()?;
        let ids: std::collections::BTreeSet<_> = self.required_task_ids.iter().collect();
        if !valid_id(&self.group_id)
            || !valid_id(&self.root_conversation_id)
            || self.actor_id.is_empty()
            || self.device_id.is_empty()
            || self.source_epoch == 0
            || self.parent_input_revision == 0
            || self.parent_control_revision == 0
            || self.version == 0
            || self.required_task_ids.len() > self.tasks_created as usize
            || ids.len() != self.required_task_ids.len()
            || self.required_task_ids.iter().any(|id| !valid_id(id))
            || !self.budget.child_charged.fits(self.budget.charged)
            || !self.budget.child_outstanding.fits(self.budget.outstanding)
        {
            return Err("invalid delegation group");
        }
        Ok(())
    }

    /// Count unfinished children across every group under the same root lock.
    pub fn admit_child(
        &mut self,
        task_id: &str,
        required: bool,
        root_unfinished: usize,
        now_ms: i64,
        limits: desk_agent_protocol::ai_assistant::subagent_policy::SubAgentLimits,
    ) -> Result<(), &'static str> {
        self.validate()?;
        super::policy::validate_limits(limits)?;
        if self.source_admission != SourceAdmission::Open || !self.parent_active {
            return Err("delegation source no longer admits new children");
        }
        if now_ms >= self.limits.deadline_ms {
            return Err("delegation deadline reached");
        }
        if root_unfinished >= limits.max_unfinished_per_root as usize {
            return Err("delegation child limit reached");
        }
        if !valid_id(task_id) || self.required_task_ids.iter().any(|id| id == task_id) {
            return Err("invalid new delegated task id");
        }
        let version = self
            .version
            .checked_add(1)
            .ok_or("delegation group version exhausted")?;
        self.tasks_created = self
            .tasks_created
            .checked_add(1)
            .ok_or("delegation creation counter exhausted")?;
        if required {
            self.required_task_ids.push(task_id.into());
        }
        self.version = version;
        Ok(())
    }

    /// Changes source admission and epoch together before fencing child planners.
    pub fn set_source_admission(
        &mut self,
        admission: SourceAdmission,
    ) -> Result<u64, &'static str> {
        if self.source_admission == SourceAdmission::Closed {
            return Err("closed delegation source cannot resume");
        }
        if admission == self.source_admission {
            return Ok(self.source_epoch);
        }
        if admission == SourceAdmission::Paused
            && !matches!(self.source, DelegationSource::Goal { .. })
        {
            return Err("only a main goal can pause its delegated source");
        }
        let epoch = self
            .source_epoch
            .checked_add(1)
            .ok_or("delegation source epoch exhausted")?;
        let version = self
            .version
            .checked_add(1)
            .ok_or("delegation group version exhausted")?;
        self.source_epoch = epoch;
        self.source_admission = admission;
        self.version = version;
        Ok(epoch)
    }

    /// Main-only stop disables this group's wait and automatic interpretation;
    /// child admission for existing tasks and their source epoch remain intact.
    pub fn stop_parent(&mut self) -> Result<(), &'static str> {
        if !self.parent_active {
            return Ok(());
        }
        let version = self
            .version
            .checked_add(1)
            .ok_or("delegation group version exhausted")?;
        self.parent_active = false;
        self.version = version;
        Ok(())
    }

    /// Only an explicit owner goal resume can reactivate main planning. Existing
    /// child limits, revisions, source admission and epoch are retained.
    pub fn resume_parent(
        &mut self,
        input_revision: u64,
        control_revision: u64,
    ) -> Result<(), &'static str> {
        if !matches!(self.source, DelegationSource::Goal { .. })
            || self.source_admission != SourceAdmission::Open
            || input_revision != self.parent_input_revision
            || control_revision < self.parent_control_revision
        {
            return Err("invalid main goal resume");
        }
        if self.parent_active && self.parent_control_revision == control_revision {
            return Ok(());
        }
        self.version = self
            .version
            .checked_add(1)
            .ok_or("delegation group version exhausted")?;
        self.parent_active = true;
        self.parent_control_revision = control_revision;
        self.validate()
    }

    pub fn can_interpret(&self, parent_input_revision: u64, parent_control_revision: u64) -> bool {
        self.parent_active
            && self.source_admission == SourceAdmission::Open
            && self.parent_input_revision == parent_input_revision
            && self.parent_control_revision == parent_control_revision
    }

    pub fn required_dependencies_terminal(&self, tasks: &[(String, super::SubAgentState)]) -> bool {
        self.required_task_ids.iter().all(|id| {
            tasks
                .iter()
                .any(|(task_id, state)| id == task_id && state.is_terminal())
        })
    }

    pub fn required_dependencies_complete(&self, tasks: &[(String, super::SubAgentState)]) -> bool {
        self.required_task_ids.iter().all(|id| {
            tasks
                .iter()
                .any(|(task_id, state)| id == task_id && *state == super::SubAgentState::Completed)
        })
    }
}
