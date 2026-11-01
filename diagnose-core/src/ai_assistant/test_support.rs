//! Fresh-format personal assistant fixtures shared by data-access tests.

use super::{data_control::AssistantDataSession, history::AssistantRetrievalBudget};
use crate::session::{AgentSessionSurface, PersistedAgentSession};
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    ai_assistant::user_instructions::{AssistantDataSettings, UserInstructions},
};

pub(crate) fn personal_session(actor: &str, root: &str, device: &str) -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        root,
        actor,
        device,
        1,
        AgentScope {
            granted: Vec::new(),
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-10-20T00:00:00Z",
    );
    session.surface = AgentSessionSurface::AiAssistant;
    session.assistant_data = Some(Box::new(AssistantDataSession {
        limits: Box::default(),
        temporary: false,
        source_device_ref: device.into(),
        source_instance_id: None,
        current_actor_input_seq: 1,
        current_input_recorded_at_unix_ms: 100,
        current_input_timezone: "UTC".into(),
        instructions: super::settings::freeze_instructions(&UserInstructions::default(), 1, 100)
            .unwrap(),
        memory_state_generation: 1,
        control_generation: 1,
        current_memories: Vec::new(),
        retrieval_budget: AssistantRetrievalBudget::new("episode-1".into()).unwrap(),
        retrieval_task_id: None,
        history_access_scope: super::data_access::owner_history_scope(
            actor,
            root,
            device,
            1,
            100,
            "episode-1",
            1,
            &AssistantDataSettings::default(),
            "UTC",
            false,
        )
        .unwrap(),
        memory_access_scope: super::memory_access::owner_memory_scope(
            actor,
            root,
            device,
            1,
            100,
            "episode-1",
            1,
            &AssistantDataSettings::default(),
            false,
        )
        .unwrap(),
        history_dependencies: Vec::new(),
        read_uses: Vec::new(),
    }));
    session
}
