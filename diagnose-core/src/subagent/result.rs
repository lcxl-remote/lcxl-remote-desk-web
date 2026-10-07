//! Model-facing result projection; owner views retain the full stored result.

use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantSubAgentResult, SubAgentState, SubAgentWaitReason, TaskAssessment,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentModelResult {
    pub task_id: String,
    pub state: SubAgentState,
    pub state_revision: u64,
    pub acceptance_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assessment: Option<TaskAssessment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<SubAgentWaitReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivered: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remaining: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipt_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

impl SubAgentModelResult {
    pub fn from_result(result: &AiAssistantSubAgentResult, include_task: bool) -> Self {
        let report = result.report.as_ref();
        Self {
            task_id: result.task.task_id.clone(),
            state: result.task.state,
            state_revision: result.task.state_revision,
            acceptance_criteria: result.acceptance_criteria.clone(),
            objective: include_task.then(|| result.objective.clone()),
            answer: report.map(|report| report.summary.clone()),
            assessment: report.map(|report| report.assessment),
            wait_reason: result.task.wait_reason,
            failure_reason: result.failure_reason.clone(),
            reason: report.and_then(|report| report.reason.clone()),
            findings: report.map_or_else(Vec::new, |report| report.findings.clone()),
            delivered: report.map_or_else(Vec::new, |report| report.delivered.clone()),
            remaining: report.map_or_else(Vec::new, |report| report.remaining.clone()),
            receipt_refs: report.map_or_else(Vec::new, |report| report.receipt_refs.clone()),
            evidence_refs: report.map_or_else(Vec::new, |report| report.evidence_refs.clone()),
        }
    }
}

#[cfg(test)]
mod tests;
