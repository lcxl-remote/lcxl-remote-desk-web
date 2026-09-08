//! Construct an isolated initial context from a published task definition.
use super::contract::ValidatedTaskContract;
use crate::{
    chat::{ChatMessage, ChatRole},
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::AgentScope;
use sha2::{Digest, Sha256};

pub struct FreshSessionInput<'a> {
    pub run_id: &'a str,
    pub actor_id: &'a str,
    pub prompt: &'a str,
    pub locale: Option<&'a str>,
    pub policy_revision: i64,
    pub scope: AgentScope,
    pub now: &'a str,
}

/// The caller must verify current task authority and atomically insert this
/// session with its occurrence claim. A validated definition alone is not an
/// authorization. No rehearsal/session state is accepted or copied here.
pub fn initial_session(
    contract: &ValidatedTaskContract,
    input: FreshSessionInput<'_>,
) -> Result<PersistedAgentSession, &'static str> {
    if !input.run_id.starts_with("schedule-run-")
        || input.run_id.len() > 256
        || input.run_id.chars().any(char::is_whitespace)
        || input.run_id.chars().any(char::is_control)
        || input.actor_id.parse::<i32>().ok().is_none_or(|id| id <= 0)
        || input.prompt.is_empty()
        || input
            .locale
            .is_some_and(|locale| locale.is_empty() || locale.len() > 64)
        || chrono::DateTime::parse_from_rfc3339(input.now).is_err()
        || format!("{:x}", Sha256::digest(input.prompt.as_bytes()))
            != contract.contract().prompt_sha256
    {
        return Err("invalid fresh task session identity");
    }
    crate::assistant_policy::require_current_policy(input.policy_revision)
        .map_err(|_| "task policy is not current")?;
    let turn_id = format!("{}-turn", input.run_id);
    let mut session = PersistedAgentSession::new(
        input.run_id,
        input.actor_id,
        &contract.contract().target_device_id,
        input.policy_revision,
        input.scope.clone(),
        input.now,
    );
    // This server-authored display identity belongs only to this occurrence.
    // Task management still resolves the session through the owner-bound run.
    let client_id = format!("task_{:x}", Sha256::digest(input.run_id.as_bytes()));
    session.adopt_client_metadata(Some(&client_id), AgentSessionSurface::DeviceAssistant);
    session.response_locale = input.locale.map(str::to_owned);
    session.begin_focus_epoch(1, Vec::<String>::new())?;
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session
        .begin_turn(
            &turn_id,
            Some(input.run_id.into()),
            None,
            input.policy_revision,
            input.scope,
            input.now,
        )
        .map_err(|_| "fresh task turn cannot start")?;
    session.adopt_trigger(TriggerOrigin::ScheduledTask, &turn_id);
    session.conversation.push(
        ChatMessage::text(
            format!("{}:input", input.run_id),
            ChatRole::User,
            input.prompt,
        )
        .with_turn_id(turn_id),
    );
    Ok(session)
}
