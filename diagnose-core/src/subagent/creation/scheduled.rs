//! Immutable publication evidence; dispatch must still read current occurrence authority.

use crate::schedule::contract::{ValidatedTaskContract, parse_contract};
use desk_agent_protocol::{AgentError, capability_grant::TaskGrantProvenance};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledCreationSource {
    pub provenance: TaskGrantProvenance,
    pub contract_canonical_json: String,
    /// The original occurrence start, independent of any planning lease renewal.
    pub started_at_ms: i64,
    pub authorization_expires_at_ms: Option<i64>,
}

impl ScheduledCreationSource {
    /// Storage supplies all inputs while holding the current publication fence.
    /// Neither this constructor nor serialization turns evidence into a grant.
    pub fn capture(
        provenance: TaskGrantProvenance,
        contract: &ValidatedTaskContract,
        started_at_ms: i64,
        authorization_expires_at_ms: Option<i64>,
    ) -> Result<Self, AgentError> {
        let source = Self {
            provenance,
            contract_canonical_json: contract.canonical_json().to_owned(),
            started_at_ms,
            authorization_expires_at_ms,
        };
        source.validate()?;
        Ok(source)
    }

    pub fn validate(&self) -> Result<ValidatedTaskContract, AgentError> {
        let denied = || crate::subagent::invalid("invalid scheduled delegation source");
        self.provenance.validate().map_err(|_| denied())?;
        let contract = parse_contract(&self.contract_canonical_json).map_err(|_| denied())?;
        let definition = contract.contract();
        if !self
            .provenance
            .scheduled_run_id
            .starts_with("schedule-run-")
            || !crate::subagent::valid_id(&self.provenance.scheduled_run_id)
            || definition.schedule_id != self.provenance.schedule_id
            || definition.task_revision != self.provenance.task_revision
            || definition.contract_revision != self.provenance.contract_revision
            || contract.digest() != self.provenance.contract_sha256
            || contract.canonical_json() != self.contract_canonical_json
            || self.started_at_ms <= 0
            || self
                .authorization_expires_at_ms
                .is_some_and(|at| at <= self.started_at_ms)
        {
            return Err(denied());
        }
        Self::deadline_for(self, &contract)?;
        Ok(contract)
    }

    fn deadline_for(&self, contract: &ValidatedTaskContract) -> Result<i64, AgentError> {
        let runtime = i64::from(contract.contract().budget.max_runtime_seconds)
            .checked_mul(1_000)
            .and_then(|duration| self.started_at_ms.checked_add(duration))
            .ok_or_else(|| crate::subagent::invalid("scheduled source deadline overflow"))?;
        Ok(runtime.min(self.authorization_expires_at_ms.unwrap_or(i64::MAX)))
    }

    /// Waiting, compression, a new planner holder and source recovery cannot
    /// replace the original start with the current time or extend this bound.
    pub fn deadline_ms(&self) -> Result<i64, AgentError> {
        Self::deadline_for(self, &self.validate()?)
    }
}

#[cfg(test)]
mod tests;
