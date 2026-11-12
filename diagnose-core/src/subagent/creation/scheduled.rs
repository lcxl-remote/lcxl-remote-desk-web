//! Immutable occurrence evidence; dispatch must still read current run authority.

use crate::schedule::definition::ScheduledTaskDefinition;
use desk_agent_protocol::{AgentError, capability_grant::TaskGrantProvenance};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledCreationSource {
    pub provenance: TaskGrantProvenance,
    /// The task and limits pinned when the occurrence was claimed.
    pub definition: ScheduledTaskDefinition,
    /// The original occurrence start, independent of any planning lease renewal.
    pub started_at_ms: i64,
}

impl ScheduledCreationSource {
    /// Storage supplies all inputs while holding the current run fence.
    /// Neither this constructor nor serialization turns evidence into a grant.
    pub fn capture(
        provenance: TaskGrantProvenance,
        definition: &ScheduledTaskDefinition,
        started_at_ms: i64,
    ) -> Result<Self, AgentError> {
        let source = Self {
            provenance,
            definition: definition.clone(),
            started_at_ms,
        };
        source.validate()?;
        Ok(source)
    }

    pub fn validate(&self) -> Result<&ScheduledTaskDefinition, AgentError> {
        let denied = || crate::subagent::invalid("invalid scheduled delegation source");
        self.provenance.validate().map_err(|_| denied())?;
        self.definition.validate().map_err(|_| denied())?;
        if !self
            .provenance
            .scheduled_run_id
            .starts_with("schedule-run-")
            || !crate::subagent::valid_id(&self.provenance.scheduled_run_id)
            || self.definition.schedule_id != self.provenance.schedule_id
            || self.definition.task_revision != self.provenance.task_revision
            || self.definition.policy_revision != self.provenance.policy_revision
            || self.started_at_ms <= 0
        {
            return Err(denied());
        }
        self.deadline_ms()?;
        Ok(&self.definition)
    }

    /// Waiting, compression, a new planner holder and source recovery cannot
    /// replace the original start with the current time or extend this bound.
    pub fn deadline_ms(&self) -> Result<i64, AgentError> {
        self.definition
            .deadline_ms(self.started_at_ms)
            .ok_or_else(|| crate::subagent::invalid("scheduled source deadline overflow"))
    }
}

#[cfg(test)]
mod tests;
