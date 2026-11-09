//! Original dispatch context, including calls with no retrieved source. This
//! freezes identity and time; it grants no future source access or native I/O.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantNativeContextFrame {
    pub session_version: i64,
    pub lease_token: u64,
    pub input_revision: u64,
    pub actor_input_seq: u64,
    pub input_recorded_at_unix_ms: u64,
    pub statement_timezone: String,
    pub retrieval_episode_id: String,
    pub task_id: Option<String>,

    pub policy_revision: u64,
    pub source_contract_revision: u64,
    pub memory_state_generation: u64,
    pub control_generation: u64,
    pub authorized_at_unix_ms: u64,
}

impl AssistantNativeContextFrame {
    pub(super) fn from_grant(grant: &AssistantDataExportAuthorization) -> Self {
        Self {
            session_version: grant.session_version,
            lease_token: grant.lease_token,
            input_revision: grant.input_revision,
            actor_input_seq: grant.actor_input_seq,
            input_recorded_at_unix_ms: grant.input_recorded_at_unix_ms,
            statement_timezone: grant.statement_timezone.clone(),
            retrieval_episode_id: grant.retrieval_episode_id.clone(),
            task_id: grant.task_id.clone(),

            policy_revision: grant.policy_revision,
            source_contract_revision: grant.source_contract_revision,
            memory_state_generation: grant.memory_state_generation,
            control_generation: grant.control_generation,
            authorized_at_unix_ms: grant.authorized_at_unix_ms,
        }
    }

    pub(super) fn from_request(
        session: &PersistedAgentSession,
        manifest: &AssistantModelDataManifest,
        started: u64,
    ) -> Result<Self, &'static str> {
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing native context session")?;
        if manifest.actor_id != session.actor_id
            || manifest.conversation_id != session.conversation_id
            || manifest.source_device_ref != data.source_device_ref
        {
            return Err("native context changed its original subject");
        }
        let value = Self {
            session_version: manifest.session_version,
            lease_token: manifest.lease_token,
            input_revision: manifest.input_revision,
            actor_input_seq: manifest.actor_input_seq,
            input_recorded_at_unix_ms: data.current_input_recorded_at_unix_ms,
            statement_timezone: data.current_input_timezone.clone(),
            retrieval_episode_id: manifest.retrieval_episode_id.clone(),
            task_id: manifest.task_id.clone(),

            policy_revision: manifest.policy_revision,
            source_contract_revision: super::super::source_registry::SOURCE_CONTRACT_REVISION,
            memory_state_generation: manifest.memory_state_generation,
            control_generation: manifest.control_generation,
            authorized_at_unix_ms: started,
        };
        value.validate_session(session)?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        validate_source_id(&self.retrieval_episode_id)?;
        if let Some(task) = &self.task_id {
            validate_source_id(task)?;
        }
        if self.session_version < 0
            || self.lease_token == 0
            || self.lease_token > i64::MAX as u64
            || self.input_revision == 0
            || self.input_revision > i64::MAX as u64
            || (self.actor_input_seq == 0
                && self
                    .task_id
                    .as_ref()
                    .is_none_or(|id| !id.starts_with("schedule-run-")))
            || self.actor_input_seq > i64::MAX as u64
            || self.input_recorded_at_unix_ms == 0
            || self.authorized_at_unix_ms < self.input_recorded_at_unix_ms
            || self.authorized_at_unix_ms > i64::MAX as u64
            || !super::super::source::valid_statement_timezone(&self.statement_timezone)
            || self.policy_revision == 0
            || self.source_contract_revision == 0
            || self.memory_state_generation == 0
            || self.control_generation == 0
        {
            return Err("invalid original native context frame");
        }
        Ok(())
    }

    pub fn validate_session(&self, session: &PersistedAgentSession) -> Result<(), &'static str> {
        self.validate()?;
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing original native context session")?;
        if self.session_version != session.version {
            return Err("original native context session version changed");
        }
        if self.lease_token != session.lease_token {
            return Err("original native context lease token changed");
        }
        if self.memory_state_generation != data.memory_state_generation
            || self.control_generation != data.control_generation
        {
            return Err("original native context data generation changed");
        }
        if !super::super::data_access::retained_context_eligible(session)
            || self.input_revision != session.input_revision
            || self.actor_input_seq != data.current_actor_input_seq
            || self.input_recorded_at_unix_ms != data.current_input_recorded_at_unix_ms
            || self.statement_timezone != data.current_input_timezone
            || self.retrieval_episode_id != data.retrieval_budget.episode_id
            || self.task_id != data.retrieval_task_id
        {
            return Err("original native context session changed");
        }
        Ok(())
    }
}
