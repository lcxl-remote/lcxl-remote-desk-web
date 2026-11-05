//! Durable parent waits for scoped reports or terminal dependencies.

use serde::{Deserialize, Serialize};

use super::{MAX_SUBAGENT_WAIT_TASKS, SubAgentState, report::ReportKind, valid_id};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitMode {
    AllTerminal,
    #[default]
    AnyTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFence {
    pub task_id: String,
    pub input_revision: u64,
    pub control_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportCursor {
    pub request_id: String,
    pub after_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportWait {
    pub requests: Vec<ReportCursor>,
    pub kinds: Vec<ReportKind>,
    #[serde(default)]
    pub mode: WaitMode,
}

impl ReportWait {
    pub fn reply_cursor(&self, request_id: &str) -> Option<u64> {
        if !self.kinds.contains(&ReportKind::NeedsInput) {
            return None;
        }
        self.requests
            .iter()
            .find(|cursor| cursor.request_id == request_id)
            .map(|cursor| cursor.after_seq)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let unique: std::collections::BTreeSet<_> = self
            .requests
            .iter()
            .map(|request| &request.request_id)
            .collect();
        if self.requests.is_empty()
            || self.requests.len() > MAX_SUBAGENT_WAIT_TASKS
            || self.requests.len() != unique.len()
            || self.requests.iter().any(|request| {
                !valid_id(&request.request_id) || request.after_seq > i64::MAX as u64
            })
            || self.kinds.is_empty()
            || self.kinds.len() > 4
            || self
                .kinds
                .iter()
                .enumerate()
                .any(|(index, kind)| self.kinds[..index].contains(kind))
        {
            return Err("invalid delegated report wait");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentWait {
    pub wait_id: String,
    pub tool_call_id: String,
    pub result_message_id: String,
    pub group_id: String,
    pub source_epoch: u64,
    pub retry_after_ms: Option<i64>,
    pub parent_input_revision: u64,
    pub parent_control_revision: u64,
    pub tasks: Vec<TaskFence>,
    pub report_wait: ReportWait,
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
        self.report_wait.validate()?;
        if self.tasks.len() != self.report_wait.requests.len()
            || self
                .tasks
                .iter()
                .zip(&self.report_wait.requests)
                .any(|(task, cursor)| task.task_id != cursor.request_id)
        {
            return Err("report wait does not match its request fences");
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
        self.evaluate_reports(
            parent_input_revision,
            parent_control_revision,
            current,
            false,
        )
    }

    pub fn evaluate_reports(
        &self,
        parent_input_revision: u64,
        parent_control_revision: u64,
        current: &[(TaskFence, SubAgentState)],
        matching_report: bool,
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
        let ready = match self.report_wait.mode {
            WaitMode::AllTerminal => terminal == self.tasks.len(),
            WaitMode::AnyTerminal => terminal > 0 || matching_report,
        };
        if ready {
            WaitEvaluation::Ready
        } else {
            WaitEvaluation::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_report_and_terminal_state_still_respect_request_and_parent_fences() {
        let fence = TaskFence {
            task_id: "request".into(),
            input_revision: 2,
            control_revision: 3,
        };
        let wait = ParentWait {
            wait_id: "wait".into(),
            tool_call_id: "call".into(),
            result_message_id: "receipt".into(),
            group_id: "group".into(),
            source_epoch: 1,
            retry_after_ms: None,
            parent_input_revision: 1,
            parent_control_revision: 1,
            tasks: vec![fence.clone()],
            report_wait: ReportWait {
                requests: vec![ReportCursor {
                    request_id: fence.task_id.clone(),
                    after_seq: 4,
                }],
                kinds: vec![ReportKind::NeedsInput],
                mode: WaitMode::AnyTerminal,
            },
        };
        wait.validate().unwrap();
        let current = [(fence.clone(), SubAgentState::WaitingInput)];
        assert_eq!(
            wait.evaluate_reports(1, 1, &current, false),
            WaitEvaluation::Pending
        );
        assert_eq!(
            wait.evaluate_reports(1, 1, &current, true),
            WaitEvaluation::Ready
        );
        let mut all = wait.clone();
        all.report_wait.mode = WaitMode::AllTerminal;
        assert_eq!(
            all.evaluate_reports(1, 1, &current, true),
            WaitEvaluation::Pending,
        );
        assert_eq!(
            wait.evaluate_reports(1, 2, &current, true),
            WaitEvaluation::ParentSuperseded
        );
        assert_eq!(
            wait.evaluate_reports(1, 1, &[(fence.clone(), SubAgentState::Cancelled)], false),
            WaitEvaluation::Ready,
        );
        let changed = TaskFence {
            control_revision: 4,
            ..fence
        };
        assert_eq!(
            wait.evaluate_reports(1, 1, &[(changed, SubAgentState::WaitingInput)], true),
            WaitEvaluation::DependencyChanged,
        );
    }
}
