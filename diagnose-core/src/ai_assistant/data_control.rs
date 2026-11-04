//! Personal assistant state and idempotent memory-control receipts.
use super::history::AssistantRetrievalBudget;
use crate::chat::ChatMessage;
use desk_agent_protocol::ai_assistant::{
    history::HistoryAccessScope,
    memory::{AssistantDataControlReceipt, MemoryUsePurpose},
    source::HistoryDependency,
    user_instructions::UserInstructionsSnapshot,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
mod memory_forget;
pub use memory_forget::{
    commit_memory_forget_group, read_memory_forget_group, record_stale_memory_forget,
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentMemoryUse {
    pub memory_id: String,
    pub revision: u64,
    pub purpose: MemoryUsePurpose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedMemoryForgetGroup {
    pub assistant_message_id: String,
    pub group_sha256: String,
    pub receipt: AssistantDataControlReceipt,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantDataSession {
    #[serde(default)]
    pub limits: Box<desk_agent_protocol::ai_assistant::limits::AssistantDataLimits>,
    pub temporary: bool,
    pub source_device_ref: String,
    pub source_instance_id: Option<String>,
    pub current_actor_input_seq: u64,
    pub current_input_recorded_at_unix_ms: u64,
    pub current_input_timezone: String,
    pub instructions: UserInstructionsSnapshot,
    pub memory_state_generation: u64,
    pub control_generation: u64,
    pub current_memories: Vec<CurrentMemoryUse>,
    pub retrieval_budget: AssistantRetrievalBudget,
    pub retrieval_task_id: Option<String>,
    pub history_access_scope: Option<Box<HistoryAccessScope>>,
    pub memory_access_scope: Option<Box<super::memory_access::MemoryAccessScope>>,
    pub history_dependencies: Vec<HistoryDependency>,
    pub read_uses: Vec<super::seam::AssistantDataProjectionRef>,
}

impl AssistantDataSession {
    pub fn request_memory_devices(&self) -> &[String] {
        self.memory_access_scope
            .as_ref()
            .map_or(&[], |scope| &scope.request_device_refs)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.retrieval_budget.validate()?;
        desk_agent_protocol::ai_assistant::source::validate_optional_device_ref(
            &self.source_device_ref,
        )?;
        if (self.current_actor_input_seq == 0
            && (self.instructions.accepted_input_seq != 0
                || self
                    .retrieval_task_id
                    .as_ref()
                    .is_none_or(|id| !id.starts_with("schedule-run-"))
                || self.history_access_scope.is_some()
                || self.memory_access_scope.is_some()
                || !self.current_memories.is_empty()
                || !self.read_uses.is_empty()
                || !self.history_dependencies.is_empty()))
            || self.current_input_recorded_at_unix_ms == 0
            || self.current_input_recorded_at_unix_ms > i64::MAX as u64
            || self.current_input_timezone.len() > 128
            || self
                .current_input_timezone
                .parse::<chrono_tz::Tz>()
                .is_err()
            || self.memory_state_generation == 0
            || self.control_generation == 0
            || self.instructions.accepted_input_seq > self.current_actor_input_seq
            || self.instructions.revision == 0
            || self.instructions.frozen_at_unix_ms == 0
            || self.instructions.content_sha256
                != format!("{:x}", Sha256::digest(self.instructions.content.as_bytes()))
        {
            return Err("invalid personal assistant data state");
        }
        if self.source_instance_id.as_ref().is_some_and(|id| {
            desk_agent_protocol::ai_assistant::source::validate_source_id(id).is_err()
        }) {
            return Err("invalid assistant source instance");
        }
        let mut memories = std::collections::BTreeSet::new();
        for memory in &self.current_memories {
            desk_agent_protocol::ai_assistant::source::validate_source_id(&memory.memory_id)?;
            if memory.revision == 0 || !memories.insert((&memory.memory_id, memory.revision)) {
                return Err("invalid current memory references");
            }
        }
        if self.temporary
            && (!self.current_memories.is_empty()
                || !self.history_dependencies.is_empty()
                || !self.read_uses.is_empty()
                || self.history_access_scope.is_some()
                || self.memory_access_scope.is_some())
        {
            return Err("temporary conversations cannot retain memory or external history uses");
        }
        if let Some(scope) = &self.history_access_scope {
            scope.validate()?;
            if scope.current_device_ref != self.source_device_ref
                || scope.retrieval_episode_id != self.retrieval_budget.episode_id
                || scope.authorized_input_seq > self.current_actor_input_seq
                || scope.timezone.parse::<chrono_tz::Tz>().is_err()
            {
                return Err("history scope does not match its original assistant input");
            }
            if scope.task_id != self.retrieval_task_id {
                return Err("history scope task binding changed");
            }
        }
        if let Some(scope) = &self.memory_access_scope {
            scope.validate()?;
            if scope.current_device_ref != self.source_device_ref
                || scope.retrieval_episode_id != self.retrieval_budget.episode_id
                || scope.authorized_input_seq > self.current_actor_input_seq
                || scope.task_id != self.retrieval_task_id
            {
                return Err("memory scope does not match its original assistant input");
            }
        }
        if let Some(task_id) = &self.retrieval_task_id {
            desk_agent_protocol::ai_assistant::source::validate_source_id(task_id)?;
        }
        let mut projections = std::collections::BTreeSet::new();
        for usage in &self.read_uses {
            usage.validate()?;
            if !projections.insert((&usage.projection_id, &usage.source_envelope_id)) {
                return Err("duplicate assistant read projection");
            }
        }
        for dependency in &self.history_dependencies {
            desk_agent_protocol::ai_assistant::source::validate_source_id(&dependency.source_id)?;
            desk_agent_protocol::ai_assistant::source::validate_source_id(&dependency.origin_id)?;
            if dependency.source_revision == 0
                || dependency.origin_deletion_generation == 0
                || dependency.policy_revision == 0
                || dependency.content_sha256.len() != 64
                || !dependency
                    .content_sha256
                    .bytes()
                    .all(|v| v.is_ascii_hexdigit())
            {
                return Err("invalid external history dependency");
            }
        }
        Ok(())
    }
}

pub fn tool_group_sha256<T: Serialize + ?Sized>(messages: &T) -> Result<String, &'static str> {
    let json = serde_json::to_vec(messages).map_err(|_| "invalid assistant tool group")?;
    Ok(format!("{:x}", Sha256::digest(json)))
}
