//! A proposal selects a Worker and targets. The native work owns its objective.

use super::*;
use desk_diagnose_core::subagent::{self, tools::SpawnRequest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstDispatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_refs: Option<Vec<String>>,
}

impl FirstDispatch {
    pub(super) fn validate(&self, allowed: &[String]) -> bool {
        self.worker_id
            .as_ref()
            .is_none_or(|id| native_ids::is(Kind::Worker, id))
            && self.target_refs.as_ref().is_none_or(|targets| {
                desk_agent_protocol::ai_assistant::validate_target_refs(
                    targets.iter().map(String::as_str),
                )
                .is_ok()
                    && targets.iter().all(|target| allowed.contains(target))
            })
    }

    pub fn request_from(
        row: &request::Model,
        session: &PersistedAgentSession,
    ) -> Result<Option<SpawnRequest>, AssistantStoreError> {
        let Some(proposal) = row.first_dispatch_json.as_deref() else {
            return Ok(None);
        };
        if session.agent_role.workbench_id() != Some(row.conversation_id.as_str())
            || session.agent_role.workbench_request_id() != Some(row.request_id.as_str())
            || session.actor_id != row.actor_id
            || session.input_revision != 1
            || session.latest_input_seq != 1
            || row.input_revision.is_some()
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let proposal: Self =
            serde_json::from_str(proposal).map_err(|_| AssistantStoreError::Inaccessible)?;
        let input = FrozenRequestInput::decode(row)?;
        if !proposal.validate(&input.ask.target_refs) {
            return Err(AssistantStoreError::Inaccessible);
        }
        let task = input.ask.question;
        let acceptance_criteria = vec![
            "Address the original request and report the outcome or what remains unresolved."
                .into(),
        ];
        subagent::role::validate_task(&task, &acceptance_criteria)
            .map_err(AssistantStoreError::Invalid)?;
        let name = task
            .trim()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(subagent::MAX_SUBAGENT_NAME_CHARS)
            .collect();
        Ok(Some(SpawnRequest {
            worker_id: proposal.worker_id,
            target_refs: proposal.target_refs.unwrap_or(input.ask.target_refs),
            name,
            task,
            acceptance_criteria,
            required_for_completion: true,
        }))
    }
}
