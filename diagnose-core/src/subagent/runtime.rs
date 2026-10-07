//! Read candidates prepare the ordinary runtime; only a host transaction claims it.
use super::{
    DelegationSource,
    creation::{CreationEnvelope, TaskCreationEnvelope},
    state::SubAgentRun,
};
use crate::{
    input_read_context::ReadContextSelection,
    session::{PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::AgentError;

#[derive(Debug, Clone)]
// Bounded records transfer as one owned value across admission and claim.
#[allow(clippy::large_enum_variant)]
pub enum RuntimeTurn {
    Child {
        session: PersistedAgentSession,
        run: SubAgentRun,
        creation: TaskCreationEnvelope,
    },
    ParentCompletion {
        session: PersistedAgentSession,
        source: CreationEnvelope,
    },
}

impl RuntimeTurn {
    pub fn session(&self) -> &PersistedAgentSession {
        match self {
            Self::Child { session, .. } | Self::ParentCompletion { session, .. } => session,
        }
    }
    pub fn source(&self) -> &CreationEnvelope {
        match self {
            Self::Child { creation, .. } => &creation.source,
            Self::ParentCompletion { source, .. } => source,
        }
    }
    pub fn origin(&self) -> TriggerOrigin {
        match self {
            Self::Child { .. } => TriggerOrigin::DelegatedTask,
            Self::ParentCompletion { .. } => TriggerOrigin::SubAgentCompletion,
        }
    }
    pub fn read_context(&self) -> Option<ReadContextSelection> {
        if matches!(self, Self::ParentCompletion { .. }) {
            return None;
        }
        self.source().child_read_context()
    }
    pub fn validate(&self) -> Result<(), AgentError> {
        self.source().validate()?;
        let session = self.session();
        if session.actor_id != self.source().actor_id
            || session.device_id != self.source().device_id
        {
            return Err(super::invalid("delegated runtime subject changed"));
        }
        match self {
            Self::Child { run, creation, .. } => {
                run.validate().map_err(super::invalid)?;
                run.validate_session(session).map_err(super::invalid)?;
                creation.validate_task(&run.binding)?;
                if creation.response_locale != session.response_locale
                    || session.delegated_owner_requirement.as_ref()
                        != Some(&creation.source.owner_requirement)
                {
                    return Err(super::invalid("delegated response locale changed"));
                }
            }
            Self::ParentCompletion { source, .. } => {
                if !session.agent_role.is_main()
                    || session.conversation_id != source.root_conversation_id
                    || session.input_revision != source.parent_input_revision
                    || session.control_revision != source.parent_control_revision
                    || (session.ready_subagent_wait.is_none()
                        && session.ready_subagent_notification.is_none())
                    || session.subagent_wait.is_some()
                    || !matches!(source.source, DelegationSource::UserInput { .. })
                {
                    return Err(super::invalid("root interpretation source changed"));
                }
            }
        }
        Ok(())
    }
}
