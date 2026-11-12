//! Owner assistant data frozen with a delegation group. A child request uses
//! the personalization and retrieval consent its main conversation had for the
//! same source, never the root's later state.

use super::{
    data_control::AssistantDataSession, history::AssistantRetrievalBudget,
    memory_access::MemoryAccessScope, scheduled_retrieval::TaskAssistantDataSeed,
};
use crate::{
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use desk_agent_protocol::ai_assistant::{history::HistoryAccessScope, limits::AssistantDataLimits};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedAssistantSeed {
    pub limits: Box<AssistantDataLimits>,
    pub source_device_ref: String,
    pub source_instance_id: Option<String>,
    pub owner: TaskAssistantDataSeed,
    pub history_scope: Option<Box<HistoryAccessScope>>,
    pub memory_scope: Option<Box<MemoryAccessScope>>,
}

impl DelegatedAssistantSeed {
    /// The main conversation's assistant data, when its current turn may use
    /// it. A temporary conversation, an external source such as an issue
    /// subscription, or a task definition without an owner chat input gives
    /// the child none.
    pub fn capture(root: &PersistedAgentSession) -> Option<Box<Self>> {
        if !root.agent_role.is_main() || !super::data_access::eligible_origin(root) {
            return None;
        }
        let data = root.assistant_data.as_deref()?;
        if data.current_actor_input_seq == 0 {
            return None;
        }
        Some(Box::new(Self {
            limits: data.limits.clone(),
            source_device_ref: data.source_device_ref.clone(),
            source_instance_id: data.source_instance_id.clone(),
            owner: TaskAssistantDataSeed {
                actor_input_seq: data.current_actor_input_seq,
                input_recorded_at_unix_ms: data.current_input_recorded_at_unix_ms,
                input_timezone: data.current_input_timezone.clone(),
                instructions: data.instructions.clone(),
                memory_state_generation: data.memory_state_generation,
                control_generation: data.control_generation,
            },
            history_scope: data.history_access_scope.clone(),
            memory_scope: data.memory_access_scope.clone(),
        }))
    }

    /// Replace the child's assistant data for the request it is bound to. Each
    /// request has its own retrieval episode and budget, and the scopes name the
    /// child conversation and request so they are usable only there.
    pub fn bind(
        seed: Option<&Self>,
        session: &mut PersistedAgentSession,
    ) -> Result<(), &'static str> {
        let binding = session
            .agent_role
            .binding()
            .ok_or("delegated assistant data requires a child request")?;
        let Some(seed) = seed else {
            session.assistant_data = None;
            return Ok(());
        };
        let task_id = binding.task_id.clone();
        let deadline = u64::try_from(binding.deadline_ms).map_err(|_| "invalid child deadline")?;
        let episode = native_ids::stable(
            Kind::RetrievalEpisode,
            &("delegated-retrieval-v1", &session.actor_id, &task_id),
        );
        let conversation_id = session.conversation_id.clone();
        let mut history = seed.history_scope.clone();
        if let Some(scope) = &mut history {
            scope.root_conversation_id = conversation_id.clone();
            scope.task_id = Some(task_id.clone());
            scope.retrieval_episode_id = episode.clone();
            scope.expires_at_unix_ms = Some(
                scope
                    .expires_at_unix_ms
                    .map_or(deadline, |old| old.min(deadline)),
            );
            scope.validate()?;
        }
        let mut memory = seed.memory_scope.clone();
        if let Some(scope) = &mut memory {
            scope.root_conversation_id = conversation_id;
            scope.task_id = Some(task_id.clone());
            scope.retrieval_episode_id = episode.clone();
            scope.expires_at_unix_ms = Some(
                scope
                    .expires_at_unix_ms
                    .map_or(deadline, |old| old.min(deadline)),
            );
            scope.validate()?;
        }
        let data = AssistantDataSession {
            limits: seed.limits.clone(),
            temporary: false,
            source_device_ref: seed.source_device_ref.clone(),
            source_instance_id: seed.source_instance_id.clone(),
            current_actor_input_seq: seed.owner.actor_input_seq,
            current_input_recorded_at_unix_ms: seed.owner.input_recorded_at_unix_ms,
            current_input_timezone: seed.owner.input_timezone.clone(),
            instructions: seed.owner.instructions.clone(),
            memory_state_generation: seed.owner.memory_state_generation,
            control_generation: seed.owner.control_generation,
            current_memories: Vec::new(),
            retrieval_budget: AssistantRetrievalBudget::with_limits(
                episode,
                seed.limits.retrieval.episode,
            )?,
            retrieval_task_id: Some(task_id),
            history_access_scope: history,
            memory_access_scope: memory,
            history_dependencies: Vec::new(),
            read_uses: Vec::new(),
        };
        data.validate()?;
        session.assistant_data = Some(Box::new(data));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        registry::ToolEffect,
        session::TriggerOrigin,
        subagent::{DelegatedTaskBinding, DelegationSource},
    };
    use desk_agent_protocol::{
        AgentScope, ExecutionMode, ai_assistant::user_instructions::AssistantDataSettings,
    };

    const DEADLINE_MS: i64 = 1_799_999_999_999;

    fn main_session() -> PersistedAgentSession {
        let mut session = crate::ai_assistant::test_support::personal_session("owner", "root", "");
        let settings = AssistantDataSettings {
            automatic_memory_enabled: true,
            model_history_search_enabled: true,
            ..Default::default()
        };
        let data = session.assistant_data.as_mut().unwrap();
        data.history_access_scope = super::super::data_access::owner_history_scope(
            "owner",
            "root",
            "",
            1,
            100,
            "episode-1",
            1,
            &settings,
            "UTC",
            false,
        )
        .unwrap();
        data.memory_access_scope = super::super::memory_access::owner_memory_scope(
            "owner",
            "root",
            "",
            1,
            100,
            "episode-1",
            1,
            &settings,
            false,
        )
        .unwrap();
        session.input_revision = 1;
        session.trigger_origin = TriggerOrigin::User;
        session.bind_assistant_input_targets(&[]).unwrap();
        session
    }

    fn child(task_id: &str) -> PersistedAgentSession {
        let mut child = PersistedAgentSession::new_subagent(
            "sa_child",
            "owner",
            "",
            1,
            AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            DelegatedTaskBinding {
                targets: vec![],
                root_conversation_id: "root".into(),
                group_id: "group".into(),
                task_id: task_id.into(),
                source: DelegationSource::UserInput { input_revision: 1 },
                objective: "Check".into(),
                acceptance_criteria: vec!["Report".into()],
                input_revision: 1,
                control_revision: 1,
                source_epoch: 1,
                deadline_ms: DEADLINE_MS,
            },
            "2026-10-20T00:00:00Z",
        )
        .unwrap();
        child.surface = crate::session::AgentSessionSurface::AiAssistant;
        child.trigger_origin = TriggerOrigin::DelegatedTask;
        child.bind_assistant_input_targets(&[]).unwrap();
        child
    }

    #[test]
    fn a_child_uses_its_main_conversations_owner_data_in_its_own_scope() {
        let main = main_session();
        let seed = DelegatedAssistantSeed::capture(&main).expect("owner input may use its data");
        let mut child = child("task-1");
        DelegatedAssistantSeed::bind(Some(&seed), &mut child).unwrap();
        let data = child.assistant_data.as_deref().unwrap();
        let root = main.assistant_data.as_deref().unwrap();
        assert_eq!(data.instructions, root.instructions);
        assert_eq!(data.current_actor_input_seq, root.current_actor_input_seq);
        assert_eq!(data.retrieval_task_id.as_deref(), Some("task-1"));
        let history = data.history_access_scope.as_deref().unwrap();
        let memory = data.memory_access_scope.as_deref().unwrap();
        for (root_id, task, episode, expires) in [
            (
                &history.root_conversation_id,
                &history.task_id,
                &history.retrieval_episode_id,
                history.expires_at_unix_ms,
            ),
            (
                &memory.root_conversation_id,
                &memory.task_id,
                &memory.retrieval_episode_id,
                memory.expires_at_unix_ms,
            ),
        ] {
            assert_eq!(root_id, "sa_child");
            assert_eq!(task.as_deref(), Some("task-1"));
            assert_eq!(episode, &data.retrieval_budget.episode_id);
            assert!(expires.is_some_and(|until| until <= DEADLINE_MS as u64));
        }
        assert_ne!(
            data.retrieval_budget.episode_id,
            root.retrieval_budget.episode_id
        );
        assert!(super::super::data_access::eligible_origin(&child));
        assert!(super::super::data_access::usable_history_scope(&child, 200).is_some());
        assert!(super::super::memory_access::usable_memory_scope(&child, 200).is_some());
        let stored = child.encode_json_for_storage().unwrap();
        assert_eq!(PersistedAgentSession::decode_json(&stored).unwrap(), child);

        // Each request has its own retrieval budget.
        let mut next = self::child("task-2");
        DelegatedAssistantSeed::bind(Some(&seed), &mut next).unwrap();
        assert_ne!(
            next.assistant_data
                .as_deref()
                .unwrap()
                .retrieval_budget
                .episode_id,
            data.retrieval_budget.episode_id
        );
        // A request without a seed carries no owner data.
        DelegatedAssistantSeed::bind(None, &mut next).unwrap();
        assert!(next.assistant_data.is_none());
        assert!(!super::super::data_access::eligible_origin(&next));
    }

    #[test]
    fn only_a_main_turn_that_may_use_owner_data_seeds_a_child() {
        assert!(DelegatedAssistantSeed::capture(&main_session()).is_some());
        let mut temporary = main_session();
        let data = temporary.assistant_data.as_mut().unwrap();
        data.temporary = true;
        data.history_access_scope = None;
        data.memory_access_scope = None;
        assert!(DelegatedAssistantSeed::capture(&temporary).is_none());
        let mut subscription = main_session();
        subscription.trigger_origin = TriggerOrigin::IssueSubscription;
        assert!(DelegatedAssistantSeed::capture(&subscription).is_none());
        let mut without_input = main_session();
        without_input.assistant_data = None;
        assert!(DelegatedAssistantSeed::capture(&without_input).is_none());
    }

    #[test]
    fn a_child_has_owner_data_tools_but_not_delegation_or_goal_proposals() {
        let child = child("task-1");
        for allowed in [
            ToolEffect::MemoryQuery,
            ToolEffect::MemoryWrite,
            ToolEffect::AssistantHistory,
        ] {
            assert!(child.agent_role.allows_effect(allowed), "{allowed:?}");
        }
        for denied in [
            ToolEffect::GoalOpenPlanning,
            ToolEffect::GoalControl,
            ToolEffect::SubAgentPlanning,
            ToolEffect::SubAgentControl,
        ] {
            assert!(!child.agent_role.allows_effect(denied), "{denied:?}");
        }
    }
}
