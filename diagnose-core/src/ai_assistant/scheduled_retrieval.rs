//! Immutable retrieval consent captured with an owner-reviewed task. This
//! metadata neither claims an occurrence nor authorizes model or device I/O.

use super::{
    data_access::TaskRetrievalSnapshot, data_control::AssistantDataSession,
    history::AssistantRetrievalBudget, memory_access::MemoryAccessScope,
};
use crate::{
    native_ids::{self, Kind},
    session::{AgentSessionSurface, PersistedAgentSession},
};
use desk_agent_protocol::ai_assistant::{
    history::HistoryAccessScope,
    source::validate_source_id,
    user_instructions::{UserInstructions, UserInstructionsSnapshot},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Only the original requirement metadata, never a transcript, read projection
/// or spent ledger. Zero means a task definition without an owner chat input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAssistantDataSeed {
    pub actor_input_seq: u64,
    pub input_recorded_at_unix_ms: u64,
    pub input_timezone: String,
    pub instructions: UserInstructionsSnapshot,
    pub memory_state_generation: u64,
    pub control_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRetrievalConsent {
    pub schema_version: u16,
    pub schedule_id: String,
    pub task_revision: u64,
    pub actor_id: String,
    /// Native device identity, never a reusable client id or a model argument.
    pub device_id: String,
    pub source_conversation_id: Option<String>,
    pub source_input_revision: Option<u64>,
    pub captured_at_unix_ms: u64,
    pub temporary: bool,
    pub source_device_ref: Option<String>,
    pub source_instance_id: Option<String>,
    pub history_scope: Option<Box<HistoryAccessScope>>,
    pub memory_scope: Option<Box<MemoryAccessScope>>,
    pub assistant_seed: Option<Box<TaskAssistantDataSeed>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledRetrievalKind {
    FreshTask,
    ConversationResume,
}

/// All fields come from the original current native task/occurrence authority.
/// An existing deadline is its original upper bound, never a renewable lease.
/// Conversation resumes have no fixed runtime bound in the native contract;
/// that absence must not be replaced by their separate start deadline.
pub struct RetrievalOccurrence<'a> {
    pub schedule_id: &'a str,
    pub task_revision: u64,
    pub run_id: &'a str,
    pub kind: ScheduledRetrievalKind,
    pub source_conversation_id: Option<&'a str>,
    pub root_conversation_id: &'a str,
    pub actor_id: &'a str,
    pub device_id: &'a str,
    pub source_device_ref: &'a str,
    pub source_instance_id: Option<&'a str>,
    pub started_at_unix_ms: u64,
    pub original_deadline_unix_ms: Option<u64>,
}

impl TaskRetrievalConsent {
    /// An owner-authored standalone definition freezes personalization only.
    /// It has no accepted chat ordinal and gains no memory/history scope.
    pub fn capture_definition(
        schedule_id: &str,
        task_revision: u64,
        actor: &str,
        device: &str,
        instructions: &UserInstructions,
        timezone: &str,
        now: u64,
    ) -> Result<Self, &'static str> {
        let value = Self {
            schema_version: 1,
            schedule_id: schedule_id.into(),
            task_revision,
            actor_id: actor.into(),
            device_id: device.into(),
            source_conversation_id: None,
            source_input_revision: None,
            captured_at_unix_ms: now,
            temporary: false,
            source_device_ref: None,
            source_instance_id: None,
            history_scope: None,
            memory_scope: None,
            assistant_seed: Some(Box::new(TaskAssistantDataSeed {
                actor_input_seq: 0,
                input_recorded_at_unix_ms: now,
                input_timezone: timezone.into(),
                instructions: super::settings::freeze_task_instructions(instructions, now)?,
                // No retrieved data is inherited. Live generations are
                // reconciled before the new context can read or send.
                memory_state_generation: 1,
                control_generation: 1,
            })),
        };
        value.validate()?;
        Ok(value)
    }

    /// The host first locks the real owner requirement and task definition.
    /// A missing assistant-data state becomes explicit absent consent; loading
    /// it later must never consult newly enabled settings to add capabilities.
    pub fn capture(
        schedule_id: &str,
        task_revision: u64,
        source: &PersistedAgentSession,
        now: u64,
    ) -> Result<Self, &'static str> {
        if source.surface != AgentSessionSurface::AiAssistant
            || !source.agent_role.is_main()
            || source.input_revision == 0
        {
            return Err("task retrieval consent requires its original personal main conversation");
        }
        let data = source.assistant_data.as_deref();
        if let Some(data) = data {
            data.validate()?;
            if data.current_input_recorded_at_unix_ms > now {
                return Err("task consent predates its original owner input");
            }
        }
        let value = Self {
            schema_version: 1,
            schedule_id: schedule_id.into(),
            task_revision,
            actor_id: source.actor_id.clone(),
            device_id: source.device_id.clone(),
            source_conversation_id: Some(source.conversation_id.clone()),
            source_input_revision: Some(source.input_revision),
            captured_at_unix_ms: now,
            temporary: data.is_some_and(|data| data.temporary),
            source_device_ref: data.map(|data| data.source_device_ref.clone()),
            source_instance_id: data.and_then(|data| data.source_instance_id.clone()),
            history_scope: data.and_then(|data| data.history_access_scope.clone()),
            memory_scope: data.and_then(|data| data.memory_access_scope.clone()),
            assistant_seed: data.map(|data| {
                Box::new(TaskAssistantDataSeed {
                    actor_input_seq: data.current_actor_input_seq,
                    input_recorded_at_unix_ms: data.current_input_recorded_at_unix_ms,
                    input_timezone: data.current_input_timezone.clone(),
                    instructions: data.instructions.clone(),
                    memory_state_generation: data.memory_state_generation,
                    control_generation: data.control_generation,
                })
            }),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [&self.schedule_id, &self.actor_id, &self.device_id] {
            validate_source_id(id)?;
        }
        for id in [
            &self.source_conversation_id,
            &self.source_device_ref,
            &self.source_instance_id,
        ]
        .into_iter()
        .flatten()
        {
            validate_source_id(id)?;
        }
        if self.schema_version != 1
            || self.task_revision == 0
            || self.task_revision > i64::MAX as u64
            || self
                .source_input_revision
                .is_some_and(|revision| revision == 0 || revision > i64::MAX as u64)
            || self.source_conversation_id.is_some() != self.source_input_revision.is_some()
            || self.captured_at_unix_ms == 0
            || self.captured_at_unix_ms > i64::MAX as u64
            || (self.temporary && (self.history_scope.is_some() || self.memory_scope.is_some()))
            || (self.source_device_ref.is_none()
                && (self.source_instance_id.is_some()
                    || self.history_scope.is_some()
                    || self.memory_scope.is_some()))
        {
            return Err("invalid frozen task retrieval consent");
        }
        if let Some(seed) = &self.assistant_seed {
            if seed.input_recorded_at_unix_ms == 0
                || seed.input_recorded_at_unix_ms > self.captured_at_unix_ms
                || seed.input_timezone.len() > 128
                || seed.input_timezone.parse::<chrono_tz::Tz>().is_err()
                || seed.memory_state_generation == 0
                || seed.control_generation == 0
                || seed.instructions.accepted_input_seq > seed.actor_input_seq
                || seed.instructions.revision == 0
                || seed.instructions.frozen_at_unix_ms == 0
                || seed.instructions.content_sha256
                    != format!("{:x}", Sha256::digest(seed.instructions.content.as_bytes()))
                || (seed.actor_input_seq == 0
                    && (seed.instructions.accepted_input_seq != 0
                        || self
                            .source_conversation_id
                            .as_ref()
                            .is_some_and(|id| !id.starts_with("schedule-run-"))
                        || self.history_scope.is_some()
                        || self.memory_scope.is_some()))
                || (self.source_conversation_id.is_none()
                    && (seed.actor_input_seq != 0
                        || self.history_scope.is_some()
                        || self.memory_scope.is_some()))
            {
                return Err("invalid frozen task assistant context");
            }
        } else if self.source_device_ref.is_some()
            || self.history_scope.is_some()
            || self.memory_scope.is_some()
        {
            return Err("task lost its original assistant context");
        }
        if let Some(scope) = &self.history_scope {
            scope.validate()?;
            self.validate_source(
                &scope.actor_id,
                &scope.root_conversation_id,
                &scope.current_device_ref,
                scope.accepted_at_unix_ms,
            )?;
        }
        if let Some(scope) = &self.memory_scope {
            scope.validate()?;
            self.validate_source(
                &scope.actor_id,
                &scope.root_conversation_id,
                &scope.current_device_ref,
                scope.accepted_at_unix_ms,
            )?;
        }
        if let (Some(history), Some(memory)) = (&self.history_scope, &self.memory_scope)
            && (history.retrieval_episode_id != memory.retrieval_episode_id
                || history.task_id != memory.task_id)
        {
            return Err("task retrieval consent mixes original task episodes");
        }
        Ok(())
    }

    fn validate_source(
        &self,
        actor: &str,
        root: &str,
        device: &str,
        accepted_at: u64,
    ) -> Result<(), &'static str> {
        if actor != self.actor_id
            || Some(root) != self.source_conversation_id.as_deref()
            || self.source_device_ref.as_deref() != Some(device)
            || accepted_at > self.captured_at_unix_ms
        {
            return Err("task retrieval scope lost its original owner binding");
        }
        Ok(())
    }

    pub fn validate_definition(
        &self,
        schedule_id: &str,
        task_revision: u64,
        actor: &str,
        device: &str,
        source: Option<&str>,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.schedule_id != schedule_id
            || self.task_revision != task_revision
            || self.actor_id != actor
            || self.device_id != device
            || self.source_conversation_id.as_deref() != source
        {
            return Err("task retrieval consent belongs to another definition");
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<String, &'static str> {
        self.validate()?;
        let raw =
            serde_json::to_string(self).map_err(|_| "invalid frozen task retrieval encoding")?;
        Ok(raw)
    }

    pub fn decode(raw: &str) -> Result<Self, &'static str> {
        let value: Self =
            serde_json::from_str(raw).map_err(|_| "invalid frozen task retrieval encoding")?;
        value.validate()?;
        Ok(value)
    }

    /// Persist this snapshot only on the original successful occurrence claim.
    /// Verifiers may derive its frozen identity and scopes, but resumes and
    /// retries must keep the existing spent ledger.
    pub fn for_occurrence(
        &self,
        occurrence: &RetrievalOccurrence<'_>,
        limits: desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits,
    ) -> Result<Box<TaskRetrievalSnapshot>, &'static str> {
        self.validate_definition(
            occurrence.schedule_id,
            occurrence.task_revision,
            occurrence.actor_id,
            occurrence.device_id,
            occurrence.source_conversation_id,
        )?;
        validate_source_id(occurrence.run_id)?;
        validate_source_id(occurrence.root_conversation_id)?;
        validate_source_id(occurrence.source_device_ref)?;
        if !occurrence.run_id.starts_with("schedule-run-")
            || occurrence.started_at_unix_ms < self.captured_at_unix_ms
            || occurrence.original_deadline_unix_ms.is_some_and(|until| {
                until <= occurrence.started_at_unix_ms || until > i64::MAX as u64
            })
            || (occurrence.kind == ScheduledRetrievalKind::FreshTask
                && occurrence.original_deadline_unix_ms.is_none())
            || self
                .source_device_ref
                .as_deref()
                .is_some_and(|device| device != occurrence.source_device_ref)
            || (self.source_device_ref.is_some()
                && self.source_instance_id.as_deref() != occurrence.source_instance_id)
            || match occurrence.kind {
                ScheduledRetrievalKind::FreshTask => {
                    occurrence.root_conversation_id != occurrence.run_id
                }
                ScheduledRetrievalKind::ConversationResume => {
                    Some(occurrence.root_conversation_id) != self.source_conversation_id.as_deref()
                }
            }
        {
            return Err("task retrieval occurrence lost its original native authority");
        }
        let episode = native_ids::stable(
            Kind::RetrievalEpisode,
            &(
                "scheduled-retrieval-v1",
                &self.actor_id,
                &self.schedule_id,
                self.task_revision,
                occurrence.run_id,
            ),
        );
        let mut history = self.history_scope.clone();
        let mut memory = self.memory_scope.clone();
        if let Some(scope) = &mut history {
            scope.root_conversation_id = occurrence.root_conversation_id.into();
            scope.task_id = Some(occurrence.run_id.into());
            scope.retrieval_episode_id = episode.clone();
            if let Some(until) = occurrence.original_deadline_unix_ms {
                scope.expires_at_unix_ms =
                    Some(scope.expires_at_unix_ms.map_or(until, |old| old.min(until)));
            }
            scope.validate()?;
        }
        if let Some(scope) = &mut memory {
            scope.root_conversation_id = occurrence.root_conversation_id.into();
            scope.task_id = Some(occurrence.run_id.into());
            scope.retrieval_episode_id = episode.clone();
            if let Some(until) = occurrence.original_deadline_unix_ms {
                scope.expires_at_unix_ms =
                    Some(scope.expires_at_unix_ms.map_or(until, |old| old.min(until)));
            }
            scope.validate()?;
        }
        Ok(Box::new(TaskRetrievalSnapshot {
            budget: AssistantRetrievalBudget::with_limits(episode, limits)?,
            scope: history,
            memory_scope: memory,
            task_id: Some(occurrence.run_id.into()),
        }))
    }

    /// The native host has already checked the first, unstarted continuation
    /// claim. This binds metadata only and grants no source or execution I/O.
    pub fn bind_conversation_resume(
        &self,
        session: &mut PersistedAgentSession,
        occurrence: &RetrievalOccurrence<'_>,
        limits: &desk_agent_protocol::ai_assistant::limits::AssistantDataLimits,
    ) -> Result<(), &'static str> {
        if occurrence.kind != ScheduledRetrievalKind::ConversationResume {
            return Err("continuation binding requires its original conversation");
        }
        let expected = self.for_occurrence(occurrence, limits.retrieval.episode)?;
        self.validate_session(session, occurrence)?;
        let Some(data) = &mut session.assistant_data else {
            return Ok(());
        };
        if data.retrieval_task_id.as_deref() == Some(occurrence.run_id) {
            return Err("occurrence retrieval budget is already bound");
        }
        let mut candidate = data.clone();
        candidate.retrieval_budget = expected.budget;
        candidate.limits = Box::new(limits.clone());
        candidate.retrieval_task_id = expected.task_id;
        candidate.history_access_scope = expected.scope;
        candidate.memory_access_scope = expected.memory_scope;
        candidate.validate()?;
        *data = candidate;
        Ok(())
    }

    /// Initialize only a genuinely new native occurrence. A fresh context never
    /// copies old transcripts, projections, active tools, receipts or counters.
    pub fn bind_fresh_task(
        &self,
        session: &mut PersistedAgentSession,
        occurrence: &RetrievalOccurrence<'_>,
        limits: &desk_agent_protocol::ai_assistant::limits::AssistantDataLimits,
    ) -> Result<(), &'static str> {
        if occurrence.kind != ScheduledRetrievalKind::FreshTask
            || session.assistant_data.is_some()
            || session.actor_id != self.actor_id
            || session.device_id != self.device_id
            || session.conversation_id != occurrence.run_id
            || session.input_revision != 1
            || session.surface != AgentSessionSurface::AiAssistant
            || !session.agent_role.is_main()
        {
            return Err("fresh task assistant context is already initialized or rebound");
        }
        let snapshot = self.for_occurrence(occurrence, limits.retrieval.episode)?;
        let Some(seed) = &self.assistant_seed else {
            return Ok(());
        };
        let data = Box::new(AssistantDataSession {
            limits: Box::new(limits.clone()),
            temporary: self.temporary,
            source_device_ref: occurrence.source_device_ref.into(),
            source_instance_id: occurrence.source_instance_id.map(str::to_owned),
            current_actor_input_seq: seed.actor_input_seq,
            current_input_recorded_at_unix_ms: seed.input_recorded_at_unix_ms,
            current_input_timezone: seed.input_timezone.clone(),
            instructions: seed.instructions.clone(),
            memory_state_generation: seed.memory_state_generation,
            control_generation: seed.control_generation,
            current_memories: Vec::new(),
            retrieval_budget: snapshot.budget,
            retrieval_task_id: snapshot.task_id,
            history_access_scope: snapshot.scope,
            memory_access_scope: snapshot.memory_scope,
            history_dependencies: Vec::new(),
            read_uses: Vec::new(),
        });
        data.validate()?;
        session.assistant_data = Some(data);
        Ok(())
    }

    /// Rechecking a running occurrence compares its frozen scope and episode,
    /// never the unspent counters derived above. The native rows still need
    /// fresh validation in the host's original transaction.
    pub fn validate_occurrence_binding(
        &self,
        session: &PersistedAgentSession,
        occurrence: &RetrievalOccurrence<'_>,
    ) -> Result<(), &'static str> {
        let episode_limits = session
            .assistant_data
            .as_ref()
            .map(|data| data.retrieval_budget.limits)
            .unwrap_or_default();
        let expected = self.for_occurrence(occurrence, episode_limits)?;
        self.validate_session(session, occurrence)?;
        let Some(data) = &session.assistant_data else {
            return Ok(());
        };
        if data.retrieval_task_id != expected.task_id
            || data.retrieval_budget.episode_id != expected.budget.episode_id
            || data.history_access_scope != expected.scope
            || data.memory_access_scope != expected.memory_scope
        {
            return Err("occurrence retrieval scope or episode changed");
        }
        Ok(())
    }

    fn validate_session(
        &self,
        session: &PersistedAgentSession,
        occurrence: &RetrievalOccurrence<'_>,
    ) -> Result<(), &'static str> {
        if session.actor_id != self.actor_id
            || session.device_id != self.device_id
            || session.conversation_id != occurrence.root_conversation_id
            || session.surface != AgentSessionSurface::AiAssistant
            || !session.agent_role.is_main()
            || (occurrence.kind == ScheduledRetrievalKind::ConversationResume
                && self
                    .source_input_revision
                    .is_none_or(|revision| session.input_revision < revision))
        {
            return Err("occurrence retrieval session changed");
        }
        let Some(data) = &session.assistant_data else {
            return if self.source_device_ref.is_none()
                && self.history_scope.is_none()
                && self.memory_scope.is_none()
                && self.assistant_seed.is_none()
            {
                Ok(())
            } else {
                Err("occurrence lost its original assistant data")
            };
        };
        data.validate()?;
        if let Some(seed) = &self.assistant_seed
            && (data.instructions != seed.instructions
                || (occurrence.kind == ScheduledRetrievalKind::FreshTask
                    && (data.current_actor_input_seq != seed.actor_input_seq
                        || data.current_input_recorded_at_unix_ms
                            != seed.input_recorded_at_unix_ms
                        || data.current_input_timezone != seed.input_timezone)))
        {
            return Err("task lost its original personalization or input context");
        }
        if occurrence.source_device_ref != data.source_device_ref
            || occurrence.source_instance_id != data.source_instance_id.as_deref()
            || self.source_device_ref.as_ref().is_some_and(|device| {
                device != &data.source_device_ref
                    || self.source_instance_id != data.source_instance_id
                    || self.temporary != data.temporary
            })
        {
            return Err("occurrence retrieval device or temporary policy changed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
