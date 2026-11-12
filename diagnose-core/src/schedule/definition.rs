//! The task one occurrence runs, pinned with its limits when the occurrence is claimed.
use desk_agent_protocol::schedule::policy::TaskBudget;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledTaskDefinition {
    pub schedule_id: String,
    pub task_revision: u64,
    pub target_device_id: String,
    pub prompt: String,
    pub locale: Option<String>,
    pub model_id: Option<i32>,
    /// Budget policy revision and limits in force when the occurrence was claimed.
    pub policy_revision: u64,
    pub budget: TaskBudget,
}

impl ScheduledTaskDefinition {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !crate::subagent::valid_id(&self.schedule_id)
            || !crate::subagent::valid_id(&self.target_device_id)
            || self.task_revision == 0
            || self.task_revision > i64::MAX as u64
            || self.policy_revision > i64::MAX as u64
            || self.prompt.trim().is_empty()
            || self.prompt.chars().count() > super::MAX_SCHEDULE_PROMPT_CHARS
            || self
                .locale
                .as_ref()
                .is_some_and(|locale| locale.is_empty() || locale.len() > 64)
            || !super::policy::valid_budget(&self.budget)
        {
            return Err("invalid scheduled task definition");
        }
        Ok(())
    }

    /// Stable bytes for comparing the pinned definition across claims.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).expect("task definition serializes")
    }

    /// End of the occurrence's runtime, counted from its original start.
    pub fn deadline_ms(&self, started_at_ms: i64) -> Option<i64> {
        i64::from(self.budget.max_runtime_seconds)
            .checked_mul(1_000)
            .and_then(|duration| started_at_ms.checked_add(duration))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> ScheduledTaskDefinition {
        ScheduledTaskDefinition {
            schedule_id: "sc1".into(),
            task_revision: 1,
            target_device_id: "device".into(),
            prompt: "Report the disk usage".into(),
            locale: Some("zh-CN".into()),
            model_id: None,
            policy_revision: 0,
            budget: crate::schedule::DEFAULT_TASK_BUDGET,
        }
    }

    #[test]
    fn a_definition_needs_a_task_prompt_and_valid_limits() {
        assert!(definition().validate().is_ok());
        assert_eq!(definition().deadline_ms(1_000), Some(901_000));
        let mut invalid = definition();
        invalid.prompt = " ".into();
        assert!(invalid.validate().is_err());
        invalid = definition();
        invalid.task_revision = 0;
        assert!(invalid.validate().is_err());
        invalid = definition();
        invalid.budget.max_calls_per_run = 0;
        assert!(invalid.validate().is_err());
    }
}
