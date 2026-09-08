//! Task-addressed execution state with independent late-result reconciliation.
use super::{ActionIdentity, ExecutionState};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownExecution {
    pub placeholder_message_id: String,
    pub since: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundExecution {
    pub action: ActionIdentity,
    pub unknown: Option<UnknownExecution>,
}

impl BackgroundExecution {
    fn state(&self) -> ExecutionState {
        match &self.unknown {
            Some(unknown) => ExecutionState::OutcomeUnknown {
                action: self.action.clone(),
                placeholder_message_id: unknown.placeholder_message_id.clone(),
                since: unknown.since.clone(),
            },
            None => ExecutionState::Executing {
                action: self.action.clone(),
            },
        }
    }
}

impl ExecutionState {
    pub fn validate(&self) -> Result<(), String> {
        let tasks = self.tasks();
        for (index, action) in tasks.iter().enumerate() {
            if tasks[..index].iter().any(|other| {
                (other.kind == action.kind && other.work_id == action.work_id)
                    || other.execution_id == action.execution_id
                    || other.action_request_id == action.action_request_id
            }) {
                return Err("ambiguous background task identity".into());
            }
        }
        Ok(())
    }

    pub fn states(&self) -> Vec<Self> {
        match self {
            Self::None => vec![],
            Self::Concurrent {
                tasks,
                interrupted_since,
            } => {
                let mut result: Vec<_> = tasks.iter().map(BackgroundExecution::state).collect();
                if let Some(since) = interrupted_since {
                    result.push(Self::Interrupted {
                        since: since.clone(),
                    });
                }
                result
            }
            other => vec![other.clone()],
        }
    }

    pub fn tasks(&self) -> Vec<&ActionIdentity> {
        match self {
            Self::Executing { action } | Self::OutcomeUnknown { action, .. } => vec![action],
            Self::Concurrent { tasks, .. } => tasks.iter().map(|task| &task.action).collect(),
            _ => vec![],
        }
    }

    pub fn task(&self, task_id: &str) -> Option<&ActionIdentity> {
        self.tasks()
            .into_iter()
            .find(|action| action.action_request_id == task_id)
    }

    pub fn execution(&self, execution_id: &str) -> Option<Self> {
        self.states().into_iter().find(|state| {
            state
                .waitable_task()
                .is_some_and(|a| a.execution_id == execution_id)
        })
    }

    pub fn contains(&self, action: &ActionIdentity) -> bool {
        self.task(&action.action_request_id) == Some(action)
    }

    pub fn has_unresolved_outcome(&self) -> bool {
        self.states().iter().any(|state| {
            matches!(
                state,
                Self::OutcomeUnknown { .. } | Self::Interrupted { .. }
            )
        })
    }

    pub fn is_running(&self) -> bool {
        self.states()
            .iter()
            .any(|state| matches!(state, Self::Executing { .. }))
    }

    pub fn interrupted(&self) -> bool {
        self.states()
            .iter()
            .any(|state| matches!(state, Self::Interrupted { .. }))
    }

    pub fn unknown(&self) -> Option<Self> {
        self.states()
            .into_iter()
            .find(|state| matches!(state, Self::OutcomeUnknown { .. }))
    }

    pub fn remove(&mut self, action: &ActionIdentity) {
        let states = self
            .states()
            .into_iter()
            .filter(|state| state.waitable_task() != Some(action))
            .collect();
        *self = Self::from_states(states);
    }

    /// Replace only the exact task; never clear siblings on a terminal receipt.
    pub fn insert(&mut self, state: Self) {
        let mut states = self.states();
        for next in state.states() {
            if let Some(action) = next.waitable_task() {
                states.retain(|old| old.waitable_task() != Some(action));
            }
            states.push(next);
        }
        *self = Self::from_states(states);
    }

    pub fn from_states(states: Vec<Self>) -> Self {
        let mut tasks = Vec::new();
        let mut interrupted_since = None;
        for state in states.into_iter().flat_map(|state| state.states()) {
            match state {
                Self::Executing { action } => tasks.push(BackgroundExecution {
                    action,
                    unknown: None,
                }),
                Self::OutcomeUnknown {
                    action,
                    placeholder_message_id,
                    since,
                } => tasks.push(BackgroundExecution {
                    action,
                    unknown: Some(UnknownExecution {
                        placeholder_message_id,
                        since,
                    }),
                }),
                Self::Interrupted { since } => interrupted_since = Some(since),
                Self::None | Self::Concurrent { .. } => unreachable!("flattened execution state"),
            }
        }
        if tasks.is_empty() {
            return interrupted_since.map_or(Self::None, |since| Self::Interrupted { since });
        }
        if tasks.len() == 1 && interrupted_since.is_none() {
            return tasks.remove(0).state();
        }
        Self::Concurrent {
            tasks,
            interrupted_since,
        }
    }
}
