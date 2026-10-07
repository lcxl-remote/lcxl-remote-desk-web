//! Durable parent waits fenced by task input/control, independent of progress.

use serde::{Deserialize, Serialize};

use super::{MAX_SUBAGENT_WAIT_TASKS, SubAgentState, valid_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitMode {
    AllTerminal,
    AnyTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFence {
    pub task_id: String,
    pub input_revision: u64,
    pub control_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentWait {
    pub wait_id: String,
    pub tool_call_id: String,
    pub result_message_id: String,
    pub group_id: String,
    pub source_epoch: u64,
    pub retry_after_ms: Option<i64>,
    pub parent_input_revision: u64,
    pub parent_control_revision: u64,
    pub mode: WaitMode,
    pub tasks: Vec<TaskFence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEvaluation {
    Pending,
    Ready,
    DependencyChanged,
    ParentSuperseded,
}

impl ParentWait {
    pub fn validate(&self) -> Result<(), &'static str> {
        let unique: std::collections::BTreeSet<_> =
            self.tasks.iter().map(|task| &task.task_id).collect();
        if !valid_id(&self.wait_id)
            || !valid_id(&self.tool_call_id)
            || !valid_id(&self.result_message_id)
            || !valid_id(&self.group_id)
            || self.source_epoch == 0
            || self.retry_after_ms.is_some_and(|at| at < 0)
            || self.parent_input_revision == 0
            || self.parent_control_revision == 0
            || self.tasks.is_empty()
            || self.tasks.len() > MAX_SUBAGENT_WAIT_TASKS
            || self.tasks.len() != unique.len()
            || self.tasks.iter().any(|task| {
                !valid_id(&task.task_id) || task.input_revision == 0 || task.control_revision == 0
            })
        {
            return Err("invalid subagent wait");
        }
        Ok(())
    }

    /// The store evaluates and registers this wait under the same root lock.
    pub fn evaluate(
        &self,
        parent_input_revision: u64,
        parent_control_revision: u64,
        current: &[(TaskFence, SubAgentState)],
    ) -> WaitEvaluation {
        if self.parent_input_revision != parent_input_revision
            || self.parent_control_revision != parent_control_revision
        {
            return WaitEvaluation::ParentSuperseded;
        }
        let mut terminal = 0;
        for expected in &self.tasks {
            let Some((actual, state)) = current
                .iter()
                .find(|(task, _)| task.task_id == expected.task_id)
            else {
                return WaitEvaluation::DependencyChanged;
            };
            if actual != expected {
                return WaitEvaluation::DependencyChanged;
            }
            terminal += usize::from(state.is_terminal());
        }
        if match self.mode {
            WaitMode::AllTerminal => terminal == self.tasks.len(),
            WaitMode::AnyTerminal => terminal > 0,
        } {
            WaitEvaluation::Ready
        } else {
            WaitEvaluation::Pending
        }
    }
}
