//! Original owner consent for memory use, independent of current feature flags.

use crate::session::{PersistedAgentSession, TriggerOrigin};
use desk_agent_protocol::ai_assistant::{
    history::AssistantDeviceScope,
    source::{validate_optional_device_ref, validate_source_id},
    user_instructions::AssistantDataSettings,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAccessScope {
    pub actor_id: String,
    pub root_conversation_id: String,
    pub task_id: Option<String>,
    pub authorized_input_seq: u64,
    pub accepted_at_unix_ms: u64,
    pub device_rule: AssistantDeviceScope,
    pub current_device_ref: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_device_refs: Vec<String>,
    pub policy_revision: u64,
    /// Preserve an original fixed bound when present. Every task read still
    /// requires its host's current native task and lease authority.
    pub expires_at_unix_ms: Option<u64>,
    pub retrieval_episode_id: String,
}

impl MemoryAccessScope {
    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [
            &self.actor_id,
            &self.root_conversation_id,
            &self.retrieval_episode_id,
        ] {
            validate_source_id(id)?;
        }
        validate_optional_device_ref(&self.current_device_ref)?;
        desk_agent_protocol::ai_assistant::validate_target_refs(
            self.request_device_refs.iter().map(String::as_str),
        )?;
        if self.current_device_ref.is_empty()
            && self.device_rule != AssistantDeviceScope::CurrentDevice
        {
            return Err("account-only memory access cannot include devices");
        }
        if !self.current_device_ref.is_empty() && !self.request_device_refs.is_empty() {
            return Err("fixed-device memory access cannot carry request targets");
        }
        if let Some(task) = &self.task_id {
            validate_source_id(task)?;
        }
        if self.authorized_input_seq == 0
            || self.authorized_input_seq > i64::MAX as u64
            || self.accepted_at_unix_ms == 0
            || self.accepted_at_unix_ms > i64::MAX as u64
            || self.policy_revision == 0
            || self.policy_revision > i64::MAX as u64
            || self
                .expires_at_unix_ms
                .is_some_and(|until| until <= self.accepted_at_unix_ms || until > i64::MAX as u64)
        {
            return Err("invalid original memory access scope");
        }
        Ok(())
    }

    /// Current facts use the original request's targets or its fixed device.
    pub fn current_device_refs(&self) -> &[String] {
        if self.current_device_ref.is_empty() {
            &self.request_device_refs
        } else {
            std::slice::from_ref(&self.current_device_ref)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn owner_memory_scope(
    actor: &str,
    root: &str,
    device: &str,
    input_seq: u64,
    accepted_at: u64,
    episode: &str,
    policy_revision: u64,
    settings: &AssistantDataSettings,
    temporary: bool,
) -> Result<Option<Box<MemoryAccessScope>>, &'static str> {
    if temporary || !settings.automatic_memory_enabled {
        return Ok(None);
    }
    let scope = MemoryAccessScope {
        actor_id: actor.into(),
        root_conversation_id: root.into(),
        task_id: None,
        authorized_input_seq: input_seq,
        accepted_at_unix_ms: accepted_at,
        device_rule: if device.is_empty() {
            AssistantDeviceScope::CurrentDevice
        } else {
            AssistantDeviceScope::AllMyDevices
        },
        current_device_ref: device.into(),
        request_device_refs: Vec::new(),
        policy_revision,
        expires_at_unix_ms: None,
        retrieval_episode_id: episode.into(),
    };
    scope.validate()?;
    Ok(Some(Box::new(scope)))
}

pub fn bind_request_memory_devices(
    session: &mut PersistedAgentSession,
    device_refs: impl IntoIterator<Item = String>,
) -> Result<(), &'static str> {
    if let Some(scope) = session
        .assistant_data
        .as_mut()
        .and_then(|data| data.memory_access_scope.as_mut())
    {
        scope.request_device_refs = device_refs.into_iter().collect();
        scope.validate()?;
    }
    Ok(())
}

/// Select an explicit request target, or the sole target when it is unambiguous.
pub fn request_memory_target<'a>(
    choices: &'a [String],
    requested: Option<&str>,
) -> Option<&'a str> {
    match requested {
        Some(reference) => choices
            .iter()
            .find(|choice| *choice == reference)
            .map(String::as_str),
        None => match choices {
            [only] => Some(only.as_str()),
            _ => None,
        },
    }
}

/// Current settings and each actual source still require fresh host validation.
pub fn usable_memory_scope(
    session: &PersistedAgentSession,
    now: u64,
) -> Option<&MemoryAccessScope> {
    if !super::data_access::eligible_origin(session) {
        return None;
    }
    let data = session.assistant_data.as_deref()?;
    let scope = data.memory_access_scope.as_deref()?;
    if scope.actor_id != session.actor_id
        || scope.root_conversation_id != session.conversation_id
        || scope.current_device_ref != data.source_device_ref
        || scope.task_id != data.retrieval_task_id
        || scope.retrieval_episode_id != data.retrieval_budget.episode_id
        || scope.expires_at_unix_ms.is_some_and(|until| until <= now)
        || (matches!(
            session.trigger_origin,
            TriggerOrigin::GoalContinuation
                | TriggerOrigin::ScheduledTask
                | TriggerOrigin::ScheduledContinuation
        ) && scope.task_id.is_none())
    {
        return None;
    }
    Some(scope)
}

#[cfg(test)]
mod tests;
